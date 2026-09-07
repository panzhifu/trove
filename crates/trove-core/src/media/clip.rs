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
//!   3. The CLIP BPE vocab, next to the model (needed for TEXT search only):
//!        https://github.com/openai/CLIP/blob/main/clip/bpe_simple_vocab_16e6.txt
//!      Save as `bpe_simple_vocab_16e6.txt` in the same directory.
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
    ///
    /// Returns `0.0` for dimension-mismatched vectors instead of panicking —
    /// rows written by an older model (different dim) must never crash a
    /// search. `semantic_search` filters those out explicitly.
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        if self.data.len() != other.data.len() {
            return 0.0;
        }
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

/// Number of stored embeddings skipped by the last `semantic_search` call
/// because their dimension differs from the loaded model (the usual cause:
/// the model was changed without re-embedding). Surfaced in Settings.
static DIM_MISMATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Dimension-mismatch count from the most recent `semantic_search` call.
pub fn dim_mismatches() -> usize {
    DIM_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed)
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

/// `true` when the BPE vocab is loaded and TEXT search can run (image
/// embedding does not need the vocab).
pub fn text_ready() -> bool {
    super::tokenizer::ready()
}

/// Where the vocab is expected for the current model directory — used by the
/// settings page to hint at a missing `bpe_simple_vocab_16e6.txt`.
pub fn vocab_expected_path() -> Option<PathBuf> {
    crate::config::AppConfig::load()
        .clip_model_dir()
        .map(|d| super::tokenizer::vocab_path(&d))
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
    // Config dir and its models/ subdirectory.
    if let Some(dir) = crate::config::AppConfig::config_dir() {
        candidates.push(dir.join(name));
        candidates.push(dir.join("models").join(name));
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
             libonnxruntime.(so|dylib|dll) next to the app."
                .into(),
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
        // The BPE vocab is only needed for text search; a missing file must
        // not disable image embedding, so a failure here is not fatal.
        if let Some(dir) = model_path.parent() {
            let vocab = super::tokenizer::vocab_path(dir);
            if vocab.is_file() {
                super::tokenizer::load(&vocab)?;
            }
        }
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

/// Pull the first usable float embedding out of the model output. Tries the
/// requested port name first, then falls back to any float tensor with ≥128
/// elements — robust across differently-named CLIP exports.
fn extract_embedding(outputs: &ort::session::SessionOutputs, preferred: &str) -> Result<Vec<f32>> {
    if let Some(v) = outputs.get(preferred) {
        if let Ok((_, data)) = v.try_extract_tensor::<f32>() {
            if !data.is_empty() {
                return Ok(data.to_vec());
            }
        }
    }
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
    let pixel_tensor = ort::value::Tensor::from_array(([1_i64, 3, 224, 224], input))
        .map_err(|e| Error::Db(format!("image tensor: {e}")))?;
    // The combined CLIP graph expects ALL inputs. Provide dummy text inputs
    // (zeros) so the image encoder runs; we read `image_embeds` as output.
    let dummy_ids = ort::value::Tensor::from_array(([1_i64, 77], vec![0i64; 77]))
        .map_err(|e| Error::Db(format!("dummy ids: {e}")))?;
    let dummy_mask = ort::value::Tensor::from_array(([1_i64, 77], vec![0i64; 77]))
        .map_err(|e| Error::Db(format!("dummy mask: {e}")))?;
    let outputs = session
        .run(ort::inputs!["pixel_values" => pixel_tensor, "input_ids" => dummy_ids, "attention_mask" => dummy_mask])
        .map_err(|e| Error::Db(format!("image inference: {e}")))?;
    extract_embedding(&outputs, "image_embeds")
}

/// Encode a text query into an embedding vector. Requires the CLIP BPE vocab
/// (loaded by `configure` from the model directory).
pub fn text_embedding(text: &str) -> Result<Vec<f32>> {
    let mut eng = engine().lock().unwrap();
    let Some(session) = eng.session.as_mut() else {
        return Err(Error::Db(format!(
            "CLIP engine not ready ({}). Configure the model in Settings ▸ Search.",
            state_label(&eng.state)
        )));
    };
    if !super::tokenizer::ready() {
        return Err(Error::Db(
            "CLIP BPE vocab not loaded. Place bpe_simple_vocab_16e6.txt next to \
             model.onnx (see Settings ▸ Search), then reselect the model directory."
                .into(),
        ));
    }
    // Real CLIP token ids (Int64) + attention mask — the model was trained
    // with these, so anything else yields meaningless embeddings.
    let (ids, mask) = super::tokenizer::encode(text)?;
    let id_tensor = ort::value::Tensor::from_array(([1_i64, 77], ids))
        .map_err(|e| Error::Db(format!("text tensor: {e}")))?;
    let mask_tensor = ort::value::Tensor::from_array(([1_i64, 77], mask))
        .map_err(|e| Error::Db(format!("mask tensor: {e}")))?;
    // The combined CLIP graph expects ALL inputs: without (zeroed)
    // pixel_values the vision branch's Shape node fails the whole run with
    // "Missing Input: pixel_values". We read `text_embeds` as output.
    let dummy_pixels = ort::value::Tensor::from_array(
        ([1_i64, 3, 224, 224], vec![0_f32; 3 * 224 * 224]),
    )
    .map_err(|e| Error::Db(format!("dummy pixels: {e}")))?;
    let outputs = session
        .run(ort::inputs![
            "pixel_values" => dummy_pixels,
            "input_ids" => id_tensor,
            "attention_mask" => mask_tensor,
        ])
        .map_err(|e| Error::Db(format!("text inference: {e}")))?;
    extract_embedding(&outputs, "text_embeds")
}

// ---------------------------------------------------------------------------
// Embedding pipeline (compute here; all SQL lives in `store::assets`)
// ---------------------------------------------------------------------------

/// Embed a single image asset and store the vector. Skips non-images, assets
/// without a blob file, and assets that already carry an embedding. Returns
/// `Ok(true)` when a new embedding was stored, `Ok(false)` when skipped.
pub fn embed_asset(
    store: &crate::store::Store,
    library_root: &Path,
    asset_id: uuid::Uuid,
) -> Result<bool> {
    use crate::store::assets;
    let conn = store.conn();
    let Some(asset) = assets::get(conn, asset_id)? else {
        return Ok(false);
    };
    if asset.kind != crate::model::AssetKind::Image {
        return Ok(false);
    }
    if assets::has_embedding(conn, asset_id)? {
        return Ok(false);
    }
    let Some(rel) = asset.rel_path else {
        return Ok(false);
    };
    let path = library_root.join(rel);
    if !path.is_file() {
        return Ok(false);
    }
    let vec = image_embedding(&path)?;
    assets::set_embedding(conn, asset_id, &Embedding::new(vec).to_bytes())?;
    Ok(true)
}

/// Embed every image asset that has no embedding yet. Returns (done, skipped).
pub fn embed_all_missing(store: &crate::store::Store, library_root: &Path) -> Result<(u64, u64)> {
    use crate::store::assets;
    let conn = store.conn();
    let missing = assets::images_missing_embedding(conn)?;
    let mut done = 0_u64;
    let mut skipped = 0_u64;
    for (id, rel) in missing {
        match assets::get(conn, id) {
            // Re-check the blob file here so a missing file counts as skipped
            // without going through the (expensive) model call.
            Ok(Some(asset)) if asset.rel_path.is_some() => {}
            _ => {
                skipped += 1;
                continue;
            }
        }
        let Some(rel) = rel else {
            skipped += 1;
            continue;
        };
        if !library_root.join(&rel).is_file() {
            skipped += 1;
            continue;
        }
        match image_embedding(&library_root.join(rel)) {
            Ok(vec) => {
                assets::set_embedding(conn, id, &Embedding::new(vec).to_bytes())?;
                done += 1;
            }
            Err(_) => skipped += 1,
        }
    }
    Ok((done, skipped))
}

/// Rank assets by cosine similarity against a query embedding. Rows whose
/// dimension differs from the query (e.g. written by an older model) are
/// ignored. `min_similarity` filters out noise; typical CLIP text-to-image
/// matches land above 0.2–0.3.
pub fn semantic_search(
    store: &crate::store::Store,
    query: &Embedding,
    min_similarity: f32,
    limit: Option<u32>,
) -> Result<Vec<(uuid::Uuid, f32)>> {
    let rows = crate::store::assets::all_embeddings(store.conn(), 500)?;
    let mut mismatched = 0usize;
    let mut scored = Vec::new();
    for (id, bytes) in rows {
        if let Some(emb) = Embedding::from_bytes(&bytes) {
            if emb.dim() != query.dim() {
                mismatched += 1;
                continue;
            }
            let sim = query.cosine_similarity(&emb);
            if sim > min_similarity {
                scored.push((id, sim));
            }
        }
    }
    // Record for the Settings coverage row instead of chattering on stderr.
    DIM_MISMATCHES.store(mismatched, std::sync::atomic::Ordering::Relaxed);
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
