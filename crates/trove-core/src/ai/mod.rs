//! AI embedding providers: the seam between Trove and the model servers.
//!
//! Trove owns the vectors — where they live, how they are scored, when they
//! are stale — and stays deliberately unopinionated about where they come
//! from. That seam is [`EmbeddingProvider`]: one trait with two concrete
//! shapes, [`openai::OpenAICompatible`] for every OpenAI-compatible server
//! (OpenAI, Ollama, LM Studio, vLLM, proxies) and [`mock::MockProvider`]
//! for tests. Future providers (a local CLIP, a multimodal endpoint) implement
//! the same trait and inherit the whole storage/search/backfill stack.
//!
//! Everything here is synchronous by design: providers run on background
//! task threads ([`crate::tasks`]), which are plain `std::thread`s.

pub mod mock;
pub mod openai;

pub use mock::MockProvider;
pub use openai::OpenAICompatible;

use crate::error::Result;
use crate::model::{Asset, EmbeddingSpace};

/// A source of embeddings for the library's assets and queries.
///
/// Implementations must be deterministic about identity: two providers
/// producing different vectors must produce different [`Self::id`] strings,
/// because that string is the `model` key every stored vector is filed
/// under and the only guard on comparability (a query vector is never
/// scored against rows of another model).
pub trait EmbeddingProvider: Send + Sync {
    /// Identity stored in `asset_embeddings.model`, e.g.
    /// `text-embedding-3-small`. Changing this abandons the old rows.
    fn id(&self) -> &str;

    /// The space the provider's *asset-side* vectors are stored in: a text
    /// embedder files rows under [`EmbeddingSpace::Text`]; a CLIP-style
    /// image embedder files them under [`EmbeddingSpace::Image`] even
    /// though its queries are text (the two live in one vector space).
    fn asset_space(&self) -> EmbeddingSpace;

    /// Vector length when it is knowable up front, `None` when the provider
    /// learns it from the first response. The store records the actual
    /// dimension per row either way.
    fn dim(&self) -> Option<usize>;

    /// Embed a batch of texts, one vector per input, in input order.
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embed images from library-relative paths. Default: unsupported —
    /// only multimodal (CLIP-style) providers implement it.
    fn embed_images(&self, _paths: &[std::path::PathBuf]) -> Result<Vec<Vec<f32>>> {
        Err(crate::error::Error::Validation(format!(
            "provider {} does not embed images",
            self.id()
        )))
    }
}

/// Cap on the text handed to a provider. Embedding APIs bound the input by
/// tokens (OpenAI: 8191); the char budget below is comfortably inside any
/// of them and keeps one pathological description from dominating a batch.
const MAX_TEXT_CHARS: usize = 4_000;

/// Build the text a text-embedding provider sees for one asset: the title
/// (or file name), the description, then the tags — most identifying first,
/// so a truncation at [`MAX_TEXT_CHARS`] drops the least useful tail.
///
/// This is the *fingerprint input*: its hash is the `source_hash` stored
/// beside the vector, so any edit to title/description/tags (or a tag
/// rename, which changes this string through the tag list) marks the row
/// stale and the next backfill re-embeds it.
pub fn asset_embed_text(asset: &Asset, tags: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    let title = asset
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(&asset.file_name);
    parts.push(title.to_string());
    if let Some(description) = asset
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        parts.push(description.to_string());
    }
    if !tags.is_empty() {
        parts.push(tags.join(", "));
    }

    let mut text = parts.join("\n");
    if text.chars().count() > MAX_TEXT_CHARS {
        text = text.chars().take(MAX_TEXT_CHARS).collect();
    }
    text
}

/// SHA-256 of an embedding input, hex — the `source_hash` column's value.
pub fn source_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use uuid::Uuid;

    #[test]
    fn asset_embed_text_prefers_title_and_keeps_all_parts() {
        let mut asset = test_asset(
            "IMG_2049.png",
            crate::model::AssetKind::Image,
            Uuid::new_v4(),
        );
        asset.title = Some("Sunset over the bay".into());
        asset.description = Some("Long exposure, tripod".into());
        let text = asset_embed_text(&asset, &["beach".into(), "trip 2026".into()]);
        assert_eq!(
            text,
            "Sunset over the bay\nLong exposure, tripod\nbeach, trip 2026"
        );

        // No title → the file name stands in; no description → it is skipped.
        let bare = test_asset(
            "IMG_2049.png",
            crate::model::AssetKind::Image,
            Uuid::new_v4(),
        );
        assert_eq!(asset_embed_text(&bare, &[]), "IMG_2049.png");
    }

    #[test]
    fn asset_embed_text_caps_length_on_a_char_boundary() {
        let mut asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        asset.description = Some("水".repeat(MAX_TEXT_CHARS + 500));
        let text = asset_embed_text(&asset, &[]);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn source_hash_is_stable_hex_and_input_sensitive() {
        let a = source_hash("one");
        let b = source_hash("one");
        let c = source_hash("two");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
