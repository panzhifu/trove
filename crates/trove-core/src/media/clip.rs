//! CLIP semantic search: generate and compare image/text embeddings.
//!
//! This module provides the trait and types for CLIP-based semantic search.
//! The actual ONNX integration requires:
//! 1. `ort` and `ndarray` dependencies (uncomment in Cargo.toml)
//! 2. Download CLIP ONNX models:
//!    - `clip-image-vit-b-32.onnx` (~150MB)
//!    - `clip-text-vit-b-32.onnx` (~150MB)
//!    - From: https://huggingface.co/onnx-community/CLIP-ViT-B-32
//!
//! To enable: uncomment ort/ndarray in Cargo.toml and implement ClipEngine for OrtClipEngine.

use std::path::Path;

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
    /// Create from raw vector (will be L2-normalized).
    pub fn new(mut data: Vec<f32>) -> Self {
        let norm = data.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        for v in &mut data {
            *v /= norm;
        }
        Self { data }
    }

    /// Cosine similarity (dot product of normalized vectors, -1.0 to 1.0).
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        assert_eq!(self.data.len(), other.data.len(), "dimension mismatch");
        self.data
            .iter()
            .zip(other.data.iter())
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .clamp(-1.0, 1.0)
    }

    /// Serialize to bytes (for BLOB storage).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.data.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Deserialize from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() % 4 != 0 {
            return None;
        }
        let data: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        if data.is_empty() {
            None
        } else {
            Some(Self::new(data))
        }
    }

    /// Dimension of the embedding.
    pub fn dim(&self) -> usize {
        self.data.len()
    }
}

// ---------------------------------------------------------------------------
// CLIP engine trait (implement this for your backend)
// ---------------------------------------------------------------------------

/// Trait for CLIP model implementations.
pub trait ClipEngine: Send + Sync {
    /// Generate an embedding for an image file.
    fn image_embedding(&self, path: &Path) -> Result<Embedding>;

    /// Generate an embedding for a text query.
    fn text_embedding(&self, text: &str) -> Result<Embedding>;
}

// ---------------------------------------------------------------------------
// Placeholder engine (returns error - replace with real implementation)
// ---------------------------------------------------------------------------

/// Placeholder CLIP engine that returns errors.
/// Replace this with a real implementation using ort + ONNX models.
pub struct PlaceholderClipEngine;

impl ClipEngine for PlaceholderClipEngine {
    fn image_embedding(&self, _path: &Path) -> Result<Embedding> {
        Err(Error::Db(
            "CLIP model not initialized. Download models and implement OrtClipEngine.".into(),
        ))
    }

    fn text_embedding(&self, _text: &str) -> Result<Embedding> {
        Err(Error::Db(
            "CLIP model not initialized. Download models and implement OrtClipEngine.".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// ONNX Runtime implementation (uncomment when ort is available)
// ---------------------------------------------------------------------------

/*
#[cfg(feature = "ort")]
pub mod ort_impl {
    use super::*;
    use std::sync::OnceLock;
    use ort::session::Session;

    static ORT_MODEL: OnceLock<OrtClipEngine> = OnceLock::new();

    pub struct OrtClipEngine {
        image_session: Session,
        text_session: Session,
    }

    impl OrtClipEngine {
        pub fn init(image_model_path: &Path, text_model_path: &Path) -> Result<()> {
            if ORT_MODEL.get().is_some() {
                return Ok(());
            }
            let image_session = Session::builder()?
                .with_optimization_level(ort::session::GraphOptimizationLevel::Level3)?
                .with_intra_threads(2)?
                .commit_from_file(image_model_path)?;
            let text_session = Session::builder()?
                .with_optimization_level(ort::session::GraphOptimizationLevel::Level3)?
                .with_intra_threads(2)?
                .commit_from_file(text_model_path)?;
            let _ = ORT_MODEL.set(OrtClipEngine { image_session, text_session });
            Ok(())
        }
    }

    impl ClipEngine for OrtClipEngine {
        fn image_embedding(&self, path: &Path) -> Result<Embedding> {
            // 1. Load and preprocess image (resize to 224x224, normalize)
            // 2. Run through image_session
            // 3. Extract and normalize output vector
            todo!("Implement with ort::value::Tensor")
        }

        fn text_embedding(&self, text: &str) -> Result<Embedding> {
            // 1. Tokenize text (CLIP tokenizer, max 77 tokens)
            // 2. Run through text_session
            // 3. Extract and normalize output vector
            todo!("Implement with ort::value::Tensor")
        }
    }
}
*/

// ---------------------------------------------------------------------------
// Semantic search functions
// ---------------------------------------------------------------------------

/// Search assets by semantic similarity to a query embedding.
/// Returns top-K results sorted by descending similarity.
pub fn semantic_search(
    store: &crate::store::Store,
    query: &Embedding,
    limit: Option<u32>,
) -> Result<Vec<(uuid::Uuid, f32)>> {
    use crate::store::rows;

    let conn = store.conn();
    let rows_vec = rows::query_map(
        conn,
        "SELECT id, embedding FROM assets WHERE embedding IS NOT NULL AND trashed_at IS NULL",
        vec![],
        |row| {
            let id = rows::req_uuid(row, 0)?;
            let bytes: Vec<u8> = row.get(1)?;
            Ok((id, bytes))
        },
    )?;

    let mut scored: Vec<(uuid::Uuid, f32)> = Vec::new();
    for (id, bytes) in rows_vec {
        if let Some(emb) = Embedding::from_bytes(&bytes) {
            let sim = query.cosine_similarity(&emb);
            if sim > 0.3 {
                scored.push((id, sim));
            }
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let limit = limit.unwrap_or(50) as usize;
    scored.truncate(limit);
    Ok(scored)
}

/// Store an embedding for an asset.
pub fn store_embedding(
    store: &crate::store::Store,
    asset_id: uuid::Uuid,
    embedding: &Embedding,
) -> Result<()> {
    use crate::store::rows;
    use rusqlite::types::Value;
    let conn = store.conn();
    rows::execute(
        conn,
        "UPDATE assets SET embedding = ?1 WHERE id = ?2",
        vec![
            Value::Blob(embedding.to_bytes()),
            rows::uuid(asset_id).into(),
        ],
    )?;
    Ok(())
}

/// Get IDs of image assets that don't have an embedding yet.
pub fn assets_needing_embedding(store: &crate::store::Store) -> Result<Vec<uuid::Uuid>> {
    use crate::store::rows;
    let conn = store.conn();
    rows::query_map(
        conn,
        "SELECT id FROM assets WHERE kind = 'image' AND trashed_at IS NULL AND embedding IS NULL",
        vec![],
        |row| rows::req_uuid(row, 0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_normalized() {
        let e = Embedding::new(vec![3.0, 4.0]);
        let norm: f32 = e.data.iter().map(|v| v * v).sum();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding should be L2-normalized"
        );
    }

    #[test]
    fn cosine_similarity_self() {
        let e = Embedding::new(vec![1.0, 2.0, 3.0]);
        assert!((e.cosine_similarity(&e) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = Embedding::new(vec![1.0, 0.0]);
        let b = Embedding::new(vec![0.0, 1.0]);
        assert!(a.cosine_similarity(&b).abs() < 1e-5);
    }

    #[test]
    fn embedding_bytes_roundtrip() {
        let e = Embedding::new(vec![1.0, 2.0, 3.0, 4.0]);
        let bytes = e.to_bytes();
        let loaded = Embedding::from_bytes(&bytes).unwrap();
        // Compare with tolerance for floating point precision.
        assert_eq!(e.dim(), loaded.dim());
        for (a, b) in e.data.iter().zip(loaded.data.iter()) {
            assert!((a - b).abs() < 1e-6, "values differ: {} vs {}", a, b);
        }
    }

    #[test]
    fn embedding_from_invalid_bytes() {
        assert!(Embedding::from_bytes(&[1, 2, 3]).is_none()); // not multiple of 4
        assert!(Embedding::from_bytes(&[]).is_none());
    }

    #[test]
    fn semantic_search_empty() {
        let store = crate::store::Store::in_memory().unwrap();
        let query = Embedding::new(vec![1.0, 0.0, 0.0]);
        let results = semantic_search(&store, &query, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn store_and_find_embedding() {
        let store = crate::store::Store::in_memory().unwrap();
        let conn = store.conn();

        // Insert a test image asset.
        let id = uuid::Uuid::new_v4();
        crate::store::assets::insert(
            conn,
            &crate::model::test_asset("test.png", crate::model::AssetKind::Image, id),
        )
        .unwrap();

        // Store embedding.
        let emb = Embedding::new(vec![1.0, 0.0, 0.0]);
        store_embedding(&store, id, &emb).unwrap();

        // Search should find it.
        let query = Embedding::new(vec![1.0, 0.0, 0.0]);
        let results = semantic_search(&store, &query, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
        assert!(results[0].1 > 0.99);
    }
}
