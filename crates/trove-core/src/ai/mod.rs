//! AI providers: the seam between Trove and the model servers.
//!
//! Two distinct AI capabilities share this module:
//!
//! - **Embeddings** ([`EmbeddingProvider`]): turn text into vectors for
//!   semantic search. One vector per (asset, model, space).
//! - **Multimodal analysis** ([`analysis::VendorAdapter`]): hand a vision
//!   model an asset's thumbnail and metadata; get back a description, tags,
//!   and optional rating. See the [`analysis`] and [`vendor`] modules.
//!
//! Everything here is synchronous by design: providers run on background
//! task threads ([`crate::tasks`]), which are plain `std::thread`s.

pub mod analysis;
pub mod search_planner;
mod http;
pub mod mock;
mod embedding_openai;
pub mod vendor;

pub use embedding_openai::OpenAICompatible;
pub use mock::MockProvider;

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

/// BLAKE3 of an embedding input, hex — the `source_hash` column's value.
///
/// This is **not** a content hash of anything the library owns: it fingerprints
/// the exact text that was sent to the provider, and it exists so the embed
/// task can skip an asset whose input has not changed since the vector was
/// paid for (`tasks/embed`). It goes through
/// [`crate::media::hash`] like every other hash in the crate — one primitive,
/// one implementation — rather than carrying a second algorithm of its own.
///
/// 🔴 **Changing this function invalidates every stored embedding.** The skip
/// above is an equality test against the value written last time, so a new
/// digest means the next embed run treats the whole library as changed and
/// re-embeds it: one provider call per asset, at the user's expense. That is
/// the intended behaviour when the *input* changes (an edited title, a new
/// tag) and a one-off cost when the *algorithm* does — so it is worth doing
/// deliberately and never as a tidy-up. Nothing else keys off this value: the
/// `source_hash` column is plain `TEXT` with no width or format constraint,
/// and no search path reads it.
pub fn source_hash(input: &str) -> String {
    crate::media::hash::hash_bytes(input.as_bytes())
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

    /// The fingerprint is a digest, stable for one input and different for
    /// another — a cache key for work that costs money, so "stable" is the
    /// property that matters. It is pinned to the shared implementation: the
    /// day `media::hash` changes algorithm, this test is where the
    /// invalidation of every stored embedding shows up.
    #[test]
    fn source_hash_is_stable_hex_and_input_sensitive() {
        let a = source_hash("one");
        let b = source_hash("one");
        let c = source_hash("two");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));

        assert_eq!(a, crate::media::hash::hash_bytes(b"one"));
        assert_eq!(
            a,
            crate::media::hash::hex(blake3::hash(b"one").as_bytes()),
            "the fingerprint is BLAKE3, the same primitive as every other hash"
        );
    }
}
