//! AI embeddings: the typed shape of a stored vector and the values written
//! with it.
//!
//! A vector is one row of `asset_embeddings`: an asset viewed through one
//! model in one [`EmbeddingSpace`]. The store persists it L2-normalized as
//! little-endian f32, so a similarity score is a plain dot product — see
//! [`crate::search`]'s vector index.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Sanity cap on embedding dimensions. Real models sit between 256 (small
/// text encoders) and a few thousand (OpenAI's 3072); the cap only exists so
/// a misconfigured provider cannot hand us a pathological row.
pub const MAX_DIM: usize = 65_536;

/// Which view of an asset a vector describes. A text embedding summarizes
/// the asset's words (title, description, tags); an image embedding
/// summarizes its pixels. Two rows can share asset and model but differ in
/// space — a CLIP-style model fills both and scores across them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EmbeddingSpace {
    Text,
    Image,
}

impl EmbeddingSpace {
    /// The wire name stored in the `space` column (and pinned by the CHECK).
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingSpace::Text => "text",
            EmbeddingSpace::Image => "image",
        }
    }

    /// Decode a `space` column value.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(EmbeddingSpace::Text),
            "image" => Ok(EmbeddingSpace::Image),
            other => Err(Error::Validation(format!(
                "unknown embedding space {other:?}"
            ))),
        }
    }
}

/// A vector ready to store. The input need not be normalized — the store
/// L2-normalizes on write — but it must be finite and non-degenerate.
#[derive(Debug, Clone)]
pub struct NewEmbedding {
    pub asset_id: Uuid,
    /// Model identity as the provider names it (e.g. `text-embedding-3-small`).
    /// Comparability hangs off this string: a query vector is only ever
    /// scored against rows of the same model and space.
    pub model: String,
    pub space: EmbeddingSpace,
    pub vector: Vec<f32>,
    /// Fingerprint of the exact input the vector was computed from (a hash
    /// of the source text, or of the file bytes). The backfill compares it
    /// against a recomputed fingerprint to skip rows whose input has not
    /// changed.
    pub source_hash: String,
}

impl NewEmbedding {
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() {
            return Err(Error::Validation("embedding model must not be empty".into()));
        }
        if self.model.len() > 255 {
            return Err(Error::Validation(
                "embedding model name exceeds 255 characters".into(),
            ));
        }
        let dim = self.vector.len();
        if dim == 0 {
            return Err(Error::Validation("embedding vector must not be empty".into()));
        }
        if dim > MAX_DIM {
            return Err(Error::Validation(format!(
                "embedding dimension {dim} exceeds the {MAX_DIM} cap"
            )));
        }
        normalized(&self.vector)?;
        Ok(())
    }
}

/// One scored similarity hit: an asset and how close its vector came.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorMatch {
    pub asset_id: Uuid,
    /// Cosine similarity in `[-1.0, 1.0]` — both sides are unit vectors, so
    /// this is the raw dot product. Higher is closer; 1.0 is identical.
    pub score: f32,
}

/// Scale `vector` to unit length. Every stored vector and every query
/// vector goes through this, which is what makes a dot product a cosine.
/// Refuses NaN/∞ and the all-zero vector, which have no direction.
pub fn normalized(vector: &[f32]) -> Result<Vec<f32>> {
    if vector.is_empty() {
        return Err(Error::Validation("embedding vector must not be empty".into()));
    }
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(Error::Validation(
            "embedding vector must hold only finite values".into(),
        ));
    }
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return Err(Error::Validation(
            "embedding vector must not be all zeros (it cannot be normalized)".into(),
        ));
    }
    Ok(vector.iter().map(|v| v / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_roundtrips_and_rejects_unknown() {
        for space in [EmbeddingSpace::Text, EmbeddingSpace::Image] {
            assert_eq!(EmbeddingSpace::parse(space.as_str()).unwrap(), space);
        }
        assert!(EmbeddingSpace::parse("video").is_err());
    }

    #[test]
    fn new_embedding_validates_shape_and_finiteness() {
        let ok = |vector: Vec<f32>| NewEmbedding {
            asset_id: Uuid::new_v4(),
            model: "text-embedding-3-small".into(),
            space: EmbeddingSpace::Text,
            vector,
            source_hash: "abc".into(),
        };
        assert!(ok(vec![0.5, 0.5]).validate().is_ok());

        // Zero vector: finite but cannot be normalized.
        assert!(ok(vec![0.0, 0.0]).validate().is_err());
        // NaN slips past the finiteness check as an error, not a panic.
        assert!(ok(vec![f32::NAN, 1.0]).validate().is_err());
        assert!(ok(vec![f32::INFINITY, 1.0]).validate().is_err());
        assert!(ok(Vec::new()).validate().is_err());
        assert!(ok(vec![1.0; MAX_DIM + 1]).validate().is_err());

        let no_model = NewEmbedding {
            model: "  ".into(),
            ..ok(vec![1.0])
        };
        assert!(no_model.validate().is_err());
    }
}
