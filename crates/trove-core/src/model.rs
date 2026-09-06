//! Domain models for the Trove asset library.
//!
//! These types are plain data with validation; they carry no storage or UI
//! concerns. Times are stored and compared as UTC; the DB layer serializes
//! them to RFC 3339 text.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
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

// ---------------------------------------------------------------------------
// Asset
// ---------------------------------------------------------------------------

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
    Other,
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
    /// Free-form custom fields, e.g. `{"aperture": "f/2.8"}`.
    pub extra: BTreeMap<String, serde_json::Value>,
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
    pub extra: BTreeMap<String, serde_json::Value>,
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
    /// Replace the whole `extra` map when `Some`.
    pub extra: Option<BTreeMap<String, serde_json::Value>>,
}

impl AssetPatch {
    /// Validate against a rule set; applies `rating` bounds.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        if let Some(Some(rating)) = self.rating
            && rating > MAX_RATING {
                return Err(crate::error::Error::Validation(format!(
                    "rating must be 0..={MAX_RATING}, got {rating}"
                )));
            }
        if let Some(Some(desc)) = &self.description
            && desc.len() > MAX_DESCRIPTION_LEN {
                return Err(crate::error::Error::Validation(
                    "description too long".into(),
                ));
            }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// A user-organized folder. Collections form a tree (`parent_id`) while the
/// membership of assets is many-to-many, so one asset can live in several
/// collections without duplicating its file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    /// Order among siblings within the same parent.
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewCollection {
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub position: i64,
}

impl NewCollection {
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(crate::error::Error::Validation(
                "collection name must not be empty".into(),
            ));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(crate::error::Error::Validation(format!(
                "collection name exceeds {MAX_NAME_LEN} characters"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------------

/// A flat, deduplicated (case-insensitive) descriptive label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    pub id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTag {
    pub name: String,
    pub color: Option<String>,
}

impl NewTag {
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(crate::error::Error::Validation(
                "tag name must not be empty".into(),
            ));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(crate::error::Error::Validation(format!(
                "tag name exceeds {MAX_NAME_LEN} characters"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Smart (saved-search) collection
// ---------------------------------------------------------------------------

/// A saved search: a named condition tree re-evaluated live against the
/// library whenever it is opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartCollection {
    pub id: Uuid,
    pub name: String,
    /// The condition tree (`SmartNode`), serialized as JSON.
    pub query: Json,
    pub color: Option<String>,
    /// Order among smart collections (flat list, no hierarchy).
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSmartCollection {
    pub name: String,
    pub query: Json,
    pub color: Option<String>,
    pub position: i64,
}

impl NewSmartCollection {
    /// Validate name; the condition tree is validated by compiling it once.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(crate::error::Error::Validation(
                "smart collection name must not be empty".into(),
            ));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(crate::error::Error::Validation(format!(
                "smart collection name exceeds {MAX_NAME_LEN} characters"
            )));
        }
        crate::store::smart::compile(&crate::store::smart::node_from_json(&self.query)?)?;
        Ok(())
    }
}

/// A condition node in a smart-collection query tree.
///
/// Serialized as `{ "op": "and"|"or"|"match", … }`; matches carry the field,
/// comparison operator and a JSON value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum SmartNode {
    And { children: Vec<SmartNode> },
    Or { children: Vec<SmartNode> },
    #[serde(rename = "match")]
    Match {
        field: SmartField,
        /// Comparison operator, serialized as `compare` (the `op` key is the
        /// internal tag, so a payload field cannot reuse it). Defaults to `==`
        /// when omitted.
        #[serde(rename = "compare", default)]
        op: SmartCompare,
        value: Json,
    },
}

/// The domain field a smart-collection condition tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartField {
    Kind,
    IsFavorite,
    Rating,
    Tag,
    Text,
    Extension,
    SizeBytes,
    /// The mined dominant color (`#rrggbb`), matched against the asset's
    /// `extra.dominant_color`.
    Color,
}

/// Comparison operators for a smart-collection condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartCompare {
    #[default]
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Sort key of an asset listing. The FTS search path always sorts by
/// bm25 relevance and ignores this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AssetSort {
    /// Import time (newest first by default).
    #[default]
    CreatedAt,
    /// File name, case-insensitive.
    Name,
    /// File size in bytes.
    SizeBytes,
    /// Star rating (un-rated assets sort last when descending).
    Rating,
}

/// Filtering + paging options for listing assets.
#[derive(Debug, Clone, Default)]
pub struct AssetQuery {
    /// Only assets whose name/title/description contain `text` (case-insensitive).
    pub text: Option<String>,
    pub kind: Option<AssetKind>,
    pub collection_id: Option<Uuid>,
    /// Limit to assets carrying *all* these tags.
    pub tag_ids: Vec<Uuid>,
    pub is_favorite: Option<bool>,
    pub is_trashed: bool,
    /// Sort key of the listing (default: import time).
    pub sort: AssetSort,
    /// Sort direction: `true` (default) = descending.
    pub sort_desc: bool,
    /// `Some(limit)` enables paging; cap applied by the caller.
    pub limit: Option<u32>,
    pub offset: u64,
}
