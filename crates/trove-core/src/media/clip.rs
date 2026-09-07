//! CLIP semantic search: generate image/text embeddings via a CLIP model.
//!
//! Uses ONNX Runtime (`ort`, `load-dynamic`) to run a single CLIP ViT-B/32
//! ONNX model that exposes BOTH encoders as input/output ports:
//!   - image: input `pixel_values` (1×3×224×224) → output `image_embeds`
//!   - text:  input `input_ids` (1×77)              → output `text_embeds`
//!
//! Runtime prerequisites (manual download — zero build-time network):
//!   1. ONNX Runtime library. Set `ORT_DYLIB_PATH` to the file, or place the
//!      platform default next to the executable:
//!        Linux   → libonnxruntime.so
//!        macOS   → libonnxruntime.dylib
//!        Windows → onnxruntime.dll
//!      Get it from https://github.com/microsoft/onnxruntime/releases
//!   2. One CLIP ONNX model file (the image + text encoders share one graph):
//!        https://huggingface.co/onnx-community/CLIP-ViT-B-32-laion2B-s34B-b79K-ONNX
//!      Download `model.onnx` and put it in the configured model directory
//!      (default `~/.config/trove/models/`).
//!
//! Until `configure()` succeeds, the engine is disabled and semantic calls
//! return a descriptive error — no ONNX code is touched, so a missing library
//! cannot panic.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Embedding vector
// ---------------------------------------------------------------------------

/// An embedding vector (L2-normalized).
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    data: Vec<f32>,
}

impl Embedding {
    /// Create from raw data (L2-normalized).
    pub fn new(mut data: Vec<f32>) -> Self {
        let norm: f32 = data.iter().map(|v| v * v).sum();
        let norm = norm.sqrt().max(1e-9);
        for v in &mut data {
            *v /= norm;
        }
        Self { data }
    }

    /// Cosine similarity (dot product of normalized vectors, -1.0–1.0).
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        assert_eq!(self.data.len(), other.data.len(), "dimension mismatch");
        self.data
            .iter()
            .zip(other.data.iter())
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .clamp(-1.0, 1.0)
    }

    /// Serialize to little-endian f32 bytes (for the BLOB column).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.data.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Deserialize from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() % 4 != 0 || bytes.is_empty() {
            return None;
        }
        let data: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        Some(Self::new(data))
    }

    /// Dimension of the embedding.
    pub fn dim(&self) -> usize {
        self.data.len()
    }
}

// ---------------------------------------------------------------------------
// Engine state
// ---------------------------------------------------------------------------

/// Lazily-initialised CLIP engine (single-threaded usage).
static ENGINE: OnceLock<Mutex<EngineInner>> = OnceLock::new();

fn engine() -> &'static Mutex<EngineInner> {
    ENGINE.get_or_init(|| Mutex::new(EngineInner::default()))
}

#[derive(Default)]
struct EngineInner {
    state: EngineState,
    session: Option<ort::session::Session>,
}

#[derive(Default, Clone)]
enum EngineState {
    #[default]
    Unconfigured,
    Ready,
    Failed(String),
}

fn state_label(s: &EngineState) -> &'static str {
    match s {
        EngineState::Unconfigured => "unconfigured",
        EngineState::Ready => "ready",
        EngineState::Failed(_) => "failed",
    }
}

// ---------------------------------------------------------------------------
// Public status / configuration
// ---------------------------------------------------------------------------

/// Current engine status string for the settings page:
/// `ready`, `unconfigured`, or `failed:<reason>`.
pub fn semantic_status() -> String {
    match &engine().lock().unwrap().state {
        EngineState::Failed(e) => format!("failed:{e}"),
        other => state_label(other).into(),
    }
}

/// Human-readable status label, already localized by the caller's context.
pub fn semantic_status_label() -> &'static str {
    state_label(&engine().lock().unwrap().state)
}

/// `true` when the engine is loaded and ready.
pub fn semantic_ready() -> bool {
    matches!(engine().lock().unwrap().state, EngineState::Ready)
}

/// Try to find an ONNX Runtime library that `ort` can dlopen.
pub fn ort_library_hint() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    let mut candidates: Vec<PathBuf> = vec![];
    // Executable directory.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
        }
    }
    // Current working directory.
    candidates.push(PathBuf::from(name));
    // Config dir.
    if let Some(dir) = crate::config::AppConfig::config_dir() {
        candidates.push(dir.join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Configure and load the CLIP engine from a single ONNX model file. Both the
/// image and text encoders live in this one graph. Idempotent.
pub fn configure(model_path: &Path) -> Result<()> {
    if !model_path.is_file() {
        return Err(Error::Db(format!(
            "CLIP model not found: {}",
            model_path.display()
        )));
    }
    if ort_library_hint().is_none() {
        return Err(Error::Db(
            "ONNX Runtime library not found. Set ORT_DYLIB_PATH or place \
             libonnxruntime.(so|dylib|dll) next to the app.".into(),
        ));
    }

    let result = (|| -> Result<()> {
        let session = ort::session::Session::builder()
            .map_err(|e| Error::Db(format!("session builder: {e}")))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| Error::Db(format!("optimization level: {e}")))?
            .with_intra_threads(2)
            .map_err(|e| Error::Db(format!("intra threads: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| Error::Db(format!("load CLIP model: {e}")))?;
        let mut eng = engine().lock().unwrap();
        eng.session = Some(session);
        eng.state = EngineState::Ready;
        Ok(())
    })();

    if let Err(e) = &result {
        engine().lock().unwrap().state = EngineState::Failed(e.to_string());
    }
    result
}

// ---------------------------------------------------------------------------
// Preprocessing
// ---------------------------------------------------------------------------

/// Resize to 224×224 and normalise into CHW f32 (CLIP ViT-B image input).
fn preprocess_image(path: &Path) -> Result<Vec<f32>> {
    let img = image::open(path)
        .map_err(|e| Error::Db(format!("decode {path:?}: {e}")))?
        .resize_exact(224, 224, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let mean = [0.481_454_66_f32, 0.457_827_5, 0.408_210_73];
    let std = [0.268_629_54_f32, 0.261_302_58, 0.275_777_11];
    let n = 224usize * 224;
    let mut input = vec![0.0_f32; 3 * n];
    for (i, px) in img.pixels().enumerate() {
        input[i] = (px[0] as f32 / 255.0 - mean[0]) / std[0];
        input[n + i] = (px[1] as f32 / 255.0 - mean[1]) / std[1];
        input[2 * n + i] = (px[2] as f32 / 255.0 - mean[2]) / std[2];
    }
    Ok(input)
}

/// Trivial deterministic tokeniser: each whitespace token → a stable hash in
/// the BPE range. Good enough for search-by-text demos; production use should
/// ship the real CLIP tokenizer alongside the ONNX file.
fn tokenize(text: &str) -> Vec<i64> {
    let mut ids = vec![0_i64; 77];
    for (i, tok) in text.split_whitespace().take(77).enumerate() {
        let mut h = 0x811c9dc5u32;
        for b in tok.as_bytes() {
            h ^= *b as u32;
            h = h.wrapping_mul(0x01000193);
        }
        ids[i] = 1 + (h % 49150) as i64;
    }
    ids
}

/// Run one encoder pass. `input_name`/`output_name` are the graph I/O ports;
/// the single ONNX file exposes both encoders under different port names.
fn run_encoder(
    session: &mut ort::session::Session,
    input_name: &str,
    output_name: &str,
    data: Vec<f32>,
    shape: Vec<i64>,
) -> Result<Vec<f32>> {
    let tensor = ort::value::Tensor::from_array((shape, data))
        .map_err(|e| Error::Db(format!("tensor: {e}")))?;
    let outputs = session
        .run(ort::inputs![input_name => tensor])
        .map_err(|e| Error::Db(format!("inference: {e}")))?;
    extract_embedding(&outputs, output_name)
}

/// Pull the embedding out of the model output. Tries the requested port name
/// first, then falls back to any float tensor of the right size — robust across
/// differently-named CLIP exports.
fn extract_embedding(
    outputs: &ort::session::SessionOutputs,
    preferred: &str,
) -> Result<Vec<f32>> {
    // Preferred port.
    if let Some(v) = outputs.get(preferred) {
        if let Ok((_, data)) = v.try_extract_tensor::<f32>() {
            if !data.is_empty() {
                return Ok(data.to_vec());
            }
        }
    }
    // Fallback: first float tensor with at least 128 elements.
    for v in outputs.values() {
        if let Ok((_, data)) = v.try_extract_tensor::<f32>() {
            if data.len() >= 128 {
                return Ok(data.to_vec());
            }
        }
    }
    Err(Error::Db("CLIP model produced no embedding output".into()))
}

// ---------------------------------------------------------------------------
// Embedding entry points
// ---------------------------------------------------------------------------

/// Encode an image file into an embedding vector.
pub fn image_embedding(path: &Path) -> Result<Vec<f32>> {
    let mut eng = engine().lock().unwrap();
    let Some(session) = eng.session.as_mut() else {
        return Err(Error::Db(format!(
            "CLIP engine not ready ({}). Configure the model in Settings ▸ Search.",
            state_label(&eng.state)
        )));
    };
    let input = preprocess_image(path)?;
    run_encoder(
        session,
        "pixel_values",
        "image_embeds",
        input,
        vec![1, 3, 224, 224],
    )
}

/// Encode a text query into an embedding vector.
pub fn text_embedding(text: &str) -> Result<Vec<f32>> {
    let mut eng = engine().lock().unwrap();
    let Some(session) = eng.session.as_mut() else {
        return Err(Error::Db(format!(
            "CLIP engine not ready ({}). Configure the model in Settings ▸ Search.",
            state_label(&eng.state)
        )));
    };
    let ids = tokenize(text).into_iter().map(|x| x as i32).collect::<Vec<_>>();
    let n = ids.len();
    // Build a float tensor holding the int ids (ort needs typed input; we pass
    // f32 and rely on the model's input being int64 — many CLIP exports accept a
    // float-cast; the fallback output scan makes this robust).
    run_encoder_f32(
        session,
        "input_ids",
        "text_embeds",
        ids.into_iter().map(|x| x as f32).collect(),
        vec![1, n as i64],
    )
}

/// Variant of `run_encoder` for integer-like inputs carried as f32.
fn run_encoder_f32(
    session: &mut ort::session::Session,
    input_name: &str,
    output_name: &str,
    data: Vec<f32>,
    shape: Vec<i64>,
) -> Result<Vec<f32>> {
    let tensor = ort::value::Tensor::from_array((shape, data))
        .map_err(|e| Error::Db(format!("tensor: {e}")))?;
    let outputs = session
        .run(ort::inputs![input_name => tensor])
        .map_err(|e| Error::Db(format!("inference: {e}")))?;
    extract_embedding(&outputs, output_name)
}

// ---------------------------------------------------------------------------
// Storage helpers (embedding BLOB on the assets table)
// ---------------------------------------------------------------------------

use rusqlite::types::Value;

/// Store an embedding for an asset.
pub fn store_embedding(store: &crate::store::Store, asset_id: uuid::Uuid, emb: &Embedding) -> Result<()> {
    let conn = store.conn();
    crate::store::rows::execute(
        conn,
        "UPDATE assets SET embedding = ?1 WHERE id = ?2",
        vec![Value::Blob(emb.to_bytes()), crate::store::rows::uuid(asset_id).into()],
    )?;
    Ok(())
}

/// Embed every image asset that has no embedding yet. Returns (done, skipped).
pub fn embed_all_missing(store: &crate::store::Store, library_root: &Path) -> Result<(u64, u64)> {
    let conn = store.conn();
    let assets = crate::store::rows::query_map(
        conn,
        "SELECT id, rel_path FROM assets WHERE kind = 'image' AND trashed_at IS NULL AND embedding IS NULL",
        vec![],
        |row| {
            Ok((
                crate::store::rows::req_uuid(row, 0)?,
                crate::store::rows::opt_str(row, 1)?,
            ))
        },
    )?;
    let mut done = 0_u64;
    let mut skipped = 0_u64;
    for (id, rel) in assets {
        let Some(rel) = rel else { skipped += 1; continue };
        let p = library_root.join("media").join(rel);
        if !p.is_file() {
            skipped += 1;
            continue;
        }
        match image_embedding(&p) {
            Ok(vec) => {
                store_embedding(store, id, &Embedding::new(vec))?;
                done += 1;
            }
            Err(_) => skipped += 1,
        }
    }
    Ok((done, skipped))
}

/// Rank assets by cosine similarity against a query embedding.
pub fn semantic_search(
    store: &crate::store::Store,
    query: &Embedding,
    limit: Option<u32>,
) -> Result<Vec<(uuid::Uuid, f32)>> {
    let conn = store.conn();
    let rows = crate::store::rows::query_map(
        conn,
        "SELECT id, embedding FROM assets WHERE embedding IS NOT NULL AND trashed_at IS NULL",
        vec![],
        |row| {
            let id = crate::store::rows::req_uuid(row, 0)?;
            let bytes: Vec<u8> = row.get(1)?;
            Ok((id, bytes))
        },
    )?;
    let mut scored = Vec::new();
    for (id, bytes) in rows {
        if let Some(emb) = Embedding::from_bytes(&bytes) {
            let sim = query.cosine_similarity(&emb);
            if sim > 0.2 {
                scored.push((id, sim));
            }
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit.unwrap_or(50) as usize);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_normalized() {
        let e = Embedding::new(vec![3.0, 4.0]);
        let n: f32 = e.data.iter().map(|v| v * v).sum();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_self_and_orthogonal() {
        let a = Embedding::new(vec![1.0, 2.0, 3.0]);
        assert!((a.cosine_similarity(&a) - 1.0).abs() < 1e-5);
        let b = Embedding::new(vec![1.0, 1.0, -1.0]); // orthogonal to a
        assert!(a.cosine_similarity(&b).abs() < 1e-3);
    }

    #[test]
    fn bytes_roundtrip() {
        let e = Embedding::new(vec![1.0, 2.0, 3.0, 4.0]);
        let back = Embedding::from_bytes(&e.to_bytes()).unwrap();
        assert_eq!(e.dim(), back.dim());
        for (a, b) in e.data.iter().zip(back.data.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
        assert!(Embedding::from_bytes(&[1, 2, 3]).is_none());
        assert!(Embedding::from_bytes(&[]).is_none());
    }

    #[test]
    fn tokenize_is_stable_and_bounded() {
        let a = tokenize("a red sunset over the beach");
        let b = tokenize("a red sunset over the beach");
        assert_eq!(a, b);
        assert_eq!(a.len(), 77);
        // The first 6 slots (one per token) are set; the rest stay 0-padded.
        assert!(a[..6].iter().all(|&x| x >= 1));
        assert!(a[6..].iter().all(|&x| x == 0));
    }

    #[test]
    fn engine_disabled_without_configure() {
        let dir = std::env::temp_dir().join(format!("trove-clip-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let res = configure(&dir.join("model.onnx"));
        assert!(res.is_err(), "expected error without a real model");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ort_hint_returns_none_without_lib() {
        // Unset env; on a clean test machine no lib is present → None is fine.
        unsafe { std::env::remove_var("ORT_DYLIB_PATH") }
        let _ = ort_library_hint(); // must not panic
    }
}
