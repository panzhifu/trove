//! Listing filters, sort keys and the paged result shape shared by the asset
//! query paths.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AspectPreset, AssetKind, Orientation, UsageStatus};

/// Sort key of an asset listing. The Tantivy search path always sorts by
/// relevance and ignores this.
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
///
/// There is deliberately no text/full-text field here: free-text matching is
/// owned by the Tantivy index (`crate::search`), which returns asset ids in
/// relevance order, and callers intersect that ranked set with the filters
/// below (`store::assets::rank_intersect`). Keeping a second, `LIKE`-based
/// text path in SQL is what this type no longer offers — see
/// `Library::search_assets` and `store::browse::BrowseContext`.
#[derive(Debug, Clone, Default)]
pub struct AssetQuery {
    pub kind: Option<AssetKind>,
    pub collection_id: Option<Uuid>,
    /// Limit to assets carrying *all* these tags.
    pub tag_ids: Vec<Uuid>,
    pub is_favorite: Option<bool>,
    /// Only assets whose recorded source path starts with this prefix
    /// (folders panel). Escaped LIKE match.
    pub source_path_prefix: Option<String>,
    /// Only assets in this usage state.
    pub usage_status: Option<UsageStatus>,
    /// Only images in this orientation (derived from width vs height;
    /// assets without dimensions match nothing).
    pub orientation: Option<Orientation>,
    /// Only assets whose width/height falls in this media preset's band
    /// (see [`AspectPreset`]; assets without dimensions match nothing).
    /// Independent of [`AssetQuery::orientation`] — the two compose.
    pub aspect: Option<AspectPreset>,
    /// Only assets rated `min_rating` or higher (unrated assets match
    /// nothing).
    pub min_rating: Option<u8>,
    /// Only assets with this file extension (case-insensitive).
    pub ext: Option<String>,
    /// Only assets carrying this commercial-use clearance. Unverified rows
    /// (`None`) match neither `Some(true)` nor `Some(false)`.
    pub commercial_use: Option<bool>,
    pub is_trashed: bool,
    /// Sort key of the listing (default: import time).
    pub sort: AssetSort,
    /// Sort direction: `true` (default) = descending.
    pub sort_desc: bool,
    /// `Some(limit)` enables paging; cap applied by the caller.
    pub limit: Option<u32>,
    pub offset: u64,
}

/// One page of a listing: the total number of matching items plus the
/// materialized slice. Every paged query returns this instead of a raw
/// `(total, items)` tuple.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub total: u64,
    pub items: Vec<T>,
}

impl<T> Page<T> {
    pub fn new(total: u64, items: Vec<T>) -> Self {
        Self { total, items }
    }

    /// Map the items, keeping the total (used to materialize id pages).
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            total: self.total,
            items: self.items.into_iter().map(f).collect(),
        }
    }
}
