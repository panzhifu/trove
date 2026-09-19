//! A deterministic, dependency-free embedding provider for tests and
//! offline development.
//!
//! The vectors are pure functions of the input text (an FNV-1a seed walk),
//! so the same text always lands on the same vector — the property that
//! makes `source_hash` staleness checks reproducible in tests — while
//! different texts land far apart, which is what makes top-k ranking
//! assertions meaningful.

use super::EmbeddingProvider;
use crate::error::Result;
use crate::model::EmbeddingSpace;

/// A fake text-embedding model of any dimension.
pub struct MockProvider {
    model: String,
    dim: usize,
}

impl MockProvider {
    /// A mock identifying itself as `model` and producing `dim`-dimensional
    /// vectors.
    pub fn new(model: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            dim,
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new("mock-embed", 8)
    }
}

impl EmbeddingProvider for MockProvider {
    fn id(&self) -> &str {
        &self.model
    }

    fn asset_space(&self) -> EmbeddingSpace {
        EmbeddingSpace::Text
    }

    fn dim(&self) -> Option<usize> {
        Some(self.dim)
    }

    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| pseudo_vector(text, self.dim))
            .collect())
    }
}

/// FNV-1a seed over the text's bytes, then one LCG step per dimension.
/// Values spread over `[-0.5, 0.5)`; the store normalizes before writing,
/// and the odds of an all-zero output from 64 bits of LCG are nil.
fn pseudo_vector(text: &str, dim: usize) -> Vec<f32> {
    let mut seed: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        seed ^= u64::from(*byte);
        seed = seed.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (0..dim)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) as f32 / (u64::MAX >> 33) as f32) - 0.5
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_are_deterministic_and_distinguish_inputs() {
        let provider = MockProvider::new("mock", 16);

        let once = provider.embed_texts(&["a red car in snow".into()]).unwrap();
        let again = provider.embed_texts(&["a red car in snow".into()]).unwrap();
        assert_eq!(once, again, "same text, same vector");
        assert_eq!(once.len(), 1);
        assert_eq!(once[0].len(), 16);

        let other = provider
            .embed_texts(&["a blue boat at sea".into()])
            .unwrap();
        assert_ne!(once[0], other[0]);

        // Order is preserved, and an empty batch is an empty result.
        let batch = provider.embed_texts(&["x".into(), "y".into()]).unwrap();
        assert_eq!(batch.len(), 2);
        assert!(provider.embed_texts(&[]).unwrap().is_empty());

        // Every vector can be normalized (never all-zero): the store's
        // validation is the real assertion, run here directly.
        for vector in &batch {
            crate::model::normalized(vector).unwrap();
        }
    }

    #[test]
    fn identity_and_metadata() {
        let provider = MockProvider::default();
        assert_eq!(provider.id(), "mock-embed");
        assert_eq!(provider.dim(), Some(8));
        assert_eq!(provider.asset_space(), EmbeddingSpace::Text);
        // The image path stays unsupported — the default implementation.
        assert!(
            provider
                .embed_images(&[std::path::PathBuf::from("media/ab/abc.png")])
                .is_err()
        );
    }
}
