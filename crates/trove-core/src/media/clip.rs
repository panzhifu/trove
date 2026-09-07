//! CLIP semantic search: generate and compare image/text embeddings.
//!
//! Uses ONNX Runtime (`ort`, `load-dynamic` mode) to run a CLIP model:
//! - image embedding (query by image)
//! - text embedding (query by description, reserved)
//!
//! Runtime requirements (download manually, no build-time download):
//!   1. ONNX Runtime dynamic library — set `ORT_DYLIB_PATH` to the file, or
//!      place `libonnxruntime.so`/`.dylib`/`.dll` next to the executable.
//!   2. Two CLIP ONNX models (from e.g. `onnx-community/CLIP-ViT-B-32`):
//!      - image encoder → `clip-image.onnx` (input `pixel_values` 1×3×224×224)
//!      - text encoder  → `clip-text.onnx`  (input `input_ids` 1×77)
//!
//! Until `configure()` succeeds, the engine is disabled and every semantic
//! call returns a descriptive error (no ONNX code is touched, so a missing
//! runtime library cannot panic).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::model::AssetKind;

// ---------------------------------------------------------------------------
// Embedding vector
// ---------------------------------------------------------------------------

/// An embedding vector (L2-normalized).
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    data: Vec<f32>,
}

impl Embedding {
    /// Create from raw vector (will be L2-normalized).
    pub fn new(mut data: Vec<f32>) -> Self {
        let norm: f32 = data.iter().map(|v| v * v).sum();
        let norm = norm.sqrt().max(1e-9);
        for v in &mut data {
            *v /= norm;
        }
        Self { data }
    }

    /// Cosine similarity (dot product of normalized vectors, -1.0 to 1.0).
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        debug_assert_eq!(self.data.len(), other.data.len(), "dimension mismatch");
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
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
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
    image: Option<ort::session::Session>,
    text: Option<ort::session::Session>,
}

#[derive(Default, Clone)]
enum EngineState {
    #[default]
    Unconfigured,
    Ready,
    Failed(String),
}

fn state_repr(s: &EngineState) -> String {
    match s {
        EngineState::Unconfigured => "not_configured".into(),
        EngineState::Ready => "ready".into(),
        EngineState::Failed(e) => format!("failed:{e}"),
    }
}

// ---------------------------------------------------------------------------
// Public status / configuration
// ---------------------------------------------------------------------------

/// Current engine status as a short string (for the settings page):
/// `unconfigured`, `ready`, or `failed:<reason>`.
pub fn semantic_status() -> String {
    state_repr(&engine().lock().unwrap().state)
}

/// `true` when the engine is loaded and ready.
pub fn semantic_ready() -> bool {
    matches!(engine().lock().unwrap().state, EngineState::Ready)
}

/// Try to find an ONNX Runtime library that `ort` can dlopen.
fn ort_library_hint() -> Option<PathBuf> {
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
    // Current working directory and the executable's directory.
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(name)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Configure and load the CLIP engine. Both model files must exist and an
/// ONNX Runtime library must be reachable (see module docs). Idempotent.
pub fn configure(image_model: &Path, text_model: &Path) -> Result<()> {
    if !image_model.is_file() {
        return Err(Error::Db(format!(
            "CLIP image model not found: {}",
            image_model.display()
        )));
    }
    if !text_model.is_file() {
        return Err(Error::Db(format!(
            "CLIP text model not found: {}",
            text_model.display()
        )));
    }
    if ort_library_hint().is_none() {
        return Err(Error::Db(
            "ONNX Runtime library not found. Set ORT_DYLIB_PATH or place \
             libonnxruntime.(so|dylib|dll) next to the app."
                .into(),
        ));
    }

    let result = (|| -> Result<()> {
        let image = ort::session::Session::builder()
            .map_err(|e| Error::Db(format!("session builder: {e}")))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| Error::Db(format!("optimization level: {e}")))?
            .with_intra_threads(2)
            .map_err(|e| Error::Db(format!("intra threads: {e}")))?
            .commit_from_file(image_model)
            .map_err(|e| Error::Db(format!("load image model: {e}")))?;
        let text = ort::session::Session::builder()
            .map_err(|e| Error::Db(format!("session builder: {e}")))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| Error::Db(format!("optimization level: {e}")))?
            .with_intra_threads(2)
            .map_err(|e| Error::Db(format!("intra threads: {e}")))?
            .commit_from_file(text_model)
            .map_err(|e| Error::Db(format!("load text model: {e}")))?;
        let mut eng = engine().lock().unwrap();
        eng.image = Some(image);
        eng.text = Some(text);
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

/// Trivial tokenizer: whitespace tokens → 1..=77 ids (this only needs to be
/// deterministic for demo search; real deployments should use the CLIP
/// tokenizer that the model was exported with).
fn tokenize(text: &str) -> Vec<i64> {
    let mut ids = vec![0_i64; 77];
    for (i, tok) in text.split_whitespace().take(77).enumerate() {
        // FNV-1a hash into the BPE vocabulary space (1..=49151), stable.
        let mut h = 0x811c9dc5u32;
        for b in tok.as_bytes() {
            h ^= *b as u32;
            h = h.wrapping_mul(0x01000193);
        }
        ids[i] = 1 + (h % 49150) as i64;
    }
    ids
}

/// Run a session and return the first float tensor with at least `min_len`
/// elements (avoids depending on exact output names between CLIP exports).
fn first_f32_embedding(outputs: &ort::session::SessionOutputs, min_len: usize) -> Result<Vec<f32>> {
    for v in outputs.values() {
        if let Ok((_, data)) = v.try_extract_tensor::<f32>() {
            if data.len() >= min_len {
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
    let Some(session) = eng.image.as_mut() else {
        let why = state_repr(&eng.state);
        return Err(Error::Db(format!(
            "CLIP engine not ready ({why}). Configure models in Settings ▸ Search."
        )));
    };
    let input = preprocess_image(path)?;
    let tensor = ort::value::Tensor::from_array(([1_i64, 3, 224, 224], input))
        .map_err(|e| Error::Db(format!("tensor: {e}")))?;
    let outputs = session
        .run(ort::inputs!["pixel_values" => tensor])
        .map_err(|e| Error::Db(format!("image inference: {e}")))?;
    first_f32_embedding(&outputs, 128)
}

/// Encode a text query into an embedding vector.
pub fn text_embedding(text: &str) -> Result<Vec<f32>> {
    let mut eng = engine().lock().unwrap();
    let Some(session) = eng.text.as_mut() else {
        let why = state_repr(&eng.state);
        return Err(Error::Db(format!(
            "CLIP engine not ready ({why}). Configure models in Settings ▸ Search."
        )));
    };
    let ids = tokenize(text);
    let tensor = ort::value::Tensor::from_array(([1_i64, 77], ids))
        .map_err(|e| Error::Db(format!("tensor: {e}")))?;
    let outputs = session
        .run(ort::inputs!["input_ids" => tensor])
        .map_err(|e| Error::Db(format!("inference: {e}")))?;
    first_f32_embedding(&outputs, 128)
}

// ---------------------------------------------------------------------------
// Storage helpers (embedding BLOB on the assets table)
// ---------------------------------------------------------------------------

use rusqlite::types::Value;

/// Store an embedding for an asset.
pub fn store_embedding(
    store: &crate::store::Store,
    asset_id: uuid::Uuid,
    emb: &Embedding,
) -> Result<()> {
    let conn = store.conn();
    crate::store::rows::execute(
        conn,
        "UPDATE assets SET embedding = ?1 WHERE id = ?2",
        vec![
            Value::Blob(emb.to_bytes()),
            crate::store::rows::uuid(asset_id).into(),
        ],
    )?;
    Ok(())
}

/// Fetch the stored embedding of an asset, if any.
pub fn asset_embedding(
    store: &crate::store::Store,
    asset_id: uuid::Uuid,
) -> Result<Option<Embedding>> {
    let conn = store.conn();
    let rows = crate::store::rows::query_one(
        conn,
        "SELECT embedding FROM assets WHERE id = ?1 AND embedding IS NOT NULL",
        vec![crate::store::rows::uuid(asset_id).into()],
        |row| row.get::<_, Vec<u8>>(0).map_err(crate::error::Error::from),
    )?;
    Ok(rows.and_then(|bytes| Embedding::from_bytes(&bytes)))
}

/// Embed every image asset that has no embedding yet and store the result.
/// Returns (processed, skipped).
pub fn embed_all_missing(store: &crate::store::Store, library_root: &Path) -> Result<(u64, u64)> {
    use crate::model::AssetKind;
    let conn = store.conn();
    let assets = crate::store::rows::query_map(
        conn,
        "SELECT id, rel_path, kind FROM assets WHERE kind = 'image' AND trashed_at IS NULL \
         AND embedding IS NULL",
        vec![],
        |row| {
            Ok((
                crate::store::rows::req_uuid(row, 0)?,
                crate::store::rows::opt_str(row, 1)?,
                crate::store::rows::req_str(row, 2)?,
            ))
        },
    )?;
    let mut done = 0u64;
    let mut skipped = 0u64;
    for (id, rel, kind) in assets {
        if kind != asset_kind_str(AssetKind::Image) {
            continue;
        }
        let Some(rel) = rel else {
            skipped += 1;
            continue;
        };
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

fn asset_kind_str(k: AssetKind) -> &'static str {
    match k {
        AssetKind::Image => "image",
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Document => "document",
        AssetKind::Archive => "archive",
        AssetKind::Font => "font",
        AssetKind::Other => "other",
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        // Truly orthogonal vectors (dot product = 0): [1,2,3]·[1,1,-1]=0.
        let b = Embedding::new(vec![1.0, 1.0, -1.0]);
        assert!(a.cosine_similarity(&b).abs() < 1e-3);
    }

    #[test]
    fn bytes_roundtrip() {
        let e = Embedding::new(vec![1.0, 2.0, 3.0, 4.0]);
        let back = Embedding::from_bytes(&e.to_bytes()).unwrap();
        // Normalization is idempotent up to float tolerance.
        assert_eq!(e.dim(), back.dim());
        for (a, b) in e.data.iter().zip(back.data.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
        assert!(Embedding::from_bytes(&[1, 2, 3]).is_none());
        assert!(Embedding::from_bytes(&[]).is_none());
    }

    #[test]
    fn tokenize_is_stable() {
        let a = tokenize("a red sunset over the beach");
        let b = tokenize("a red sunset over the beach");
        assert_eq!(a, b);
        assert_eq!(a.len(), 77);
    }

    #[test]
    fn engine_disabled_without_configure() {
        // No ONNX library exists in the test env; configuring must return an
        // error rather than panic, and the status string reports failure.
        let dir = std::env::temp_dir().join(format!("trove-clip-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let res = configure(&dir.join("clip-image.onnx"), &dir.join("clip-text.onnx"));
        assert!(res.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
