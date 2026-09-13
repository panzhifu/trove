//! The asset record and its enums: origin, kind, and the create/patch inputs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::facts::AssetFacts;
use super::{MAX_DESCRIPTION_LEN, MAX_NAME_LEN, MAX_RATING};

/// Where the asset's file lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The file was copied into the library (content-addressed storage).
    Stored,
    /// The file is referenced at its external path. Reserved for future use.
    Linked,
}

/// Coarse asset classification, derived from the mime type and overridable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Font,
    /// Triangle mesh (OBJ / STL / PLY), previewed in the main-area viewport.
    Model,
    Other,
}

/// Image orientation derived from width vs height. Landscape is wider than
/// tall, portrait taller than wide, square equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    Landscape,
    Portrait,
    Square,
}

/// Where the asset stands in the user's workflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageStatus {
    /// Not used yet (the default for fresh imports).
    #[default]
    Unused,
    /// Marked as used.
    Used,
}

/// A single media asset: one record, one deduplicated blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    pub origin: Origin,
    /// Content-addressed path inside the library when `origin == Stored`,
    /// e.g. `media/ab/cdef0123….png`. `None` for `Linked`.
    pub rel_path: Option<String>,
    /// Original file name, kept for export / display. May repeat.
    pub file_name: String,
    pub ext: String,
    pub mime: String,
    pub size_bytes: u64,
    /// SHA-256 of the file content, hex. Deduplication key.
    pub sha256: Option<String>,
    pub kind: AssetKind,
    /// Media dimensions / duration, present only when the file carries them.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    /// Time the media was originally created (EXIF etc.), if known.
    pub captured_at: Option<DateTime<Utc>>,
    /// Display title; defaults to the file name without extension.
    pub title: Option<String>,
    pub description: Option<String>,
    /// 0..=MAX_RATING. `None` means unrated.
    pub rating: Option<u8>,
    pub is_favorite: bool,
    /// Where the asset was collected from, when applicable.
    pub source_url: Option<String>,
    /// Where the asset stands in the user's workflow (used / unused).
    pub usage_status: UsageStatus,
    /// Commercial-use clearance of the license: `None` = not verified yet,
    /// `Some(true)` = cleared for commercial use, `Some(false)` = forbidden.
    pub commercial_use: Option<bool>,
    /// Typed per-kind metadata (EXIF, font tables, audio tags, …) plus any
    /// unrecognized passthrough keys. Serialized under the legacy name
    /// `extra` as a flat key map, so the storage column and v2 exports are
    /// unchanged by the typing.
    #[serde(rename = "extra")]
    pub facts: AssetFacts,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `None` while live; set to the deletion moment when in the trash.
    pub trashed_at: Option<DateTime<Utc>>,
}

/// Input describing a new asset, before the importer fills media facts.
#[derive(Debug, Clone)]
pub struct NewAsset {
    pub file_name: String,
    pub ext: String,
    pub mime: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub kind: AssetKind,
    pub title: Option<String>,
    pub description: Option<String>,
    pub facts: AssetFacts,
}

impl NewAsset {
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        if self.file_name.trim().is_empty() {
            return Err(crate::error::Error::Validation(
                "file name must not be empty".into(),
            ));
        }
        if self.file_name.len() > MAX_NAME_LEN {
            return Err(crate::error::Error::Validation(format!(
                "file name exceeds {MAX_NAME_LEN} characters"
            )));
        }
        Ok(())
    }
}

/// Patch describing a partial update to an asset.
#[derive(Debug, Clone, Default)]
pub struct AssetPatch {
    pub title: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub kind: Option<AssetKind>,
    pub rating: Option<Option<u8>>,
    pub is_favorite: Option<bool>,
    pub source_url: Option<Option<String>>,
    /// Set the usage state.
    pub usage_status: Option<UsageStatus>,
    /// Set/clear the commercial-use flag. `Some(None)` clears back to
    /// "not verified".
    pub commercial_use: Option<Option<bool>>,
    /// Replace the whole [`AssetFacts`] when `Some`.
    pub facts: Option<AssetFacts>,
}

impl AssetPatch {
    /// Validate against a rule set; applies `rating` bounds.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        if let Some(Some(rating)) = self.rating
            && rating > MAX_RATING
        {
            return Err(crate::error::Error::Validation(format!(
                "rating must be 0..={MAX_RATING}, got {rating}"
            )));
        }
        if let Some(Some(desc)) = &self.description
            && desc.len() > MAX_DESCRIPTION_LEN
        {
            return Err(crate::error::Error::Validation(
                "description too long".into(),
            ));
        }
        Ok(())
    }
}
