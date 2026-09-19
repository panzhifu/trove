//! Domain models for the Trove asset library.
//!
//! These types are plain data with validation; they carry no storage or UI
//! concerns. Times are stored and compared as UTC; the DB layer serializes
//! them to RFC 3339 text.
//!
//! Layout: [`asset`] holds the asset record and its enums, [`facts`] the
//! typed per-kind metadata persisted in the `extra` JSON column,
//! [`collection`] the two container types (manual and smart), [`smart_query`]
//! the saved-search condition tree, [`tag`] the tag tree, and [`query`] the
//! listing filters and paging types.

mod asset;
mod collection;
mod embedding;
mod facts;
mod query;
mod smart_query;
mod tag;

pub use asset::{
    ASPECT_TOLERANCE, AspectPreset, Asset, AssetKind, AssetPatch, NewAsset, Orientation, Origin,
    UsageStatus,
};
pub use collection::{Collection, NewCollection, NewSmartCollection, SmartCollection};
pub use embedding::{EmbeddingSpace, MAX_DIM, NewEmbedding, VectorMatch, normalized};
pub use facts::{AssetFacts, FontFacts, MediaTagsFacts, PhotoFacts, VisualFacts};
pub use query::{AssetQuery, AssetSort, Page};
pub use smart_query::{SmartCompare, SmartField, SmartNode};
pub use tag::{NewTag, Tag};

use chrono::{DateTime, Utc};
use uuid::Uuid;

pub const MAX_NAME_LEN: usize = 255;
pub const MAX_DESCRIPTION_LEN: usize = 8_000;
pub const MAX_RATING: u8 = 5;

/// Create a fresh id.
pub fn new_id() -> Uuid {
    Uuid::new_v4()
}

/// Current UTC timestamp.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
pub fn test_asset(name: &str, kind: AssetKind, id: Uuid) -> Asset {
    Asset {
        id,
        origin: Origin::Stored,
        rel_path: Some(format!("media/{}/{}", &id.to_string()[..2], name)),
        file_name: name.into(),
        ext: name.split('.').next_back().unwrap_or("").into(),
        mime: match kind {
            AssetKind::Image => "image/png",
            AssetKind::Video => "video/mp4",
            AssetKind::Audio => "audio/mpeg",
            _ => "application/octet-stream",
        }
        .into(),
        size_bytes: 128,
        sha256: Some("a".repeat(64)),
        kind,
        width: Some(800),
        height: Some(600),
        duration_ms: None,
        captured_at: None,
        title: None,
        description: None,
        rating: None,
        is_favorite: false,
        source_url: None,
        usage_status: UsageStatus::Unused,
        commercial_use: None,
        facts: AssetFacts::default(),
        created_at: now(),
        updated_at: now(),
        trashed_at: None,
    }
}
