//! Listing filters, sort keys and the paged result shape shared by the asset
//! query paths.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AspectPreset, AssetKind, Orientation, ResolutionBand, UsageStatus};

/// Sort key of an asset listing. The Tantivy search path always sorts by
/// relevance and ignores this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AssetSort {
    /// Import time (newest first by default).
    #[default]
    CreatedAt,
    /// Last modified time.
    UpdatedAt,
    /// File name, case-insensitive.
    Name,
    /// File size in bytes.
    SizeBytes,
    /// Star rating (un-rated assets sort last when descending).
    Rating,
    /// Media duration (for video/audio).
    Duration,
    /// Dominant color.
    Color,
}

/// Filtering + paging options for listing assets.
///
/// There is deliberately no text/full-text field here: free-text matching is
/// owned by the Tantivy index (`crate::search`), which returns asset ids in
/// relevance order, and callers intersect that ranked set with the filters
/// below (`store::assets::rank_intersect`). Keeping a second, `LIKE`-based
/// text path in SQL is what this type no longer offers — see
/// `Library::search_assets` and `store::browse::BrowseContext`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Only assets whose longer edge falls in this resolution band
    /// (see [`ResolutionBand`]; assets without dimensions match nothing).
    /// Independent of both shape filters above — a 4K photo is a 4K photo at
    /// any ratio, so the three compose.
    pub resolution: Option<ResolutionBand>,
    /// Only assets rated `min_rating` or higher (unrated assets match
    /// nothing).
    pub min_rating: Option<u8>,
    /// Only assets with this file extension (case-insensitive).
    pub ext: Option<String>,
    /// Only assets carrying this commercial-use clearance. Unverified rows
    /// (`None`) match neither `Some(true)` nor `Some(false)`.
    pub commercial_use: Option<bool>,
    /// Extra conditions written into the search box with the field-qualifier
    /// grammar (`ext:png`, `-kind:video`, `path:/data`, `rating:3`, `fav:no`).
    ///
    /// These live here rather than as more `Option` fields because they arrive
    /// in sets and can be negated: `ext:png ext:jpg` is one OR over two values,
    /// and `-ext:jpg` is a second, opposite condition. See
    /// [`QueryCondition`]. Every listing path applies them through
    /// `store::assets::build_where`, so a qualified filter means the same thing
    /// in the grid, the trash and a search result.
    pub conditions: Vec<QueryCondition>,
    pub is_trashed: bool,
    /// Sort key of the listing (default: import time).
    pub sort: AssetSort,
    /// Sort direction: `true` (default) = descending.
    pub sort_desc: bool,
    /// `Some(limit)` enables paging; cap applied by the caller.
    pub limit: Option<u32>,
    pub offset: u64,
}

impl AssetQuery {
    /// Whether this query turns a row set into a smaller one.
    ///
    /// Sort, direction and paging are deliberately left out: they reorder and
    /// window, they never reject, and a ranking that ran out of candidates is
    /// just as short whichever way it was sorted. Callers ask this before
    /// gathering a ranked pool, because a pool that is about to be rejected
    /// down has to be gathered to the term's whole result set rather than to a
    /// fixed width — see [`crate::search::MAX_RANKED_POOL`].
    pub fn rejects_rows(&self) -> bool {
        self.kind.is_some()
            || self.collection_id.is_some()
            || !self.tag_ids.is_empty()
            || self.is_favorite.is_some()
            || self.source_path_prefix.is_some()
            || self.usage_status.is_some()
            || self.orientation.is_some()
            || self.aspect.is_some()
            || self.resolution.is_some()
            || self.min_rating.is_some()
            || self.ext.is_some()
            || self.commercial_use.is_some()
            || !self.conditions.is_empty()
            || self.is_trashed
    }
}

/// One extra condition the search box's grammar can state.
///
/// The vocabulary is deliberately the same one the `smart_collections` rule
/// tree speaks, so a qualifier typed into the box and a rule saved into a
/// smart collection narrow the library the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryCondition {
    /// Case-insensitive extensions without the dot. Several OR with each
    /// other; a negated set excludes all of them.
    Ext { values: Vec<String>, negate: bool },
    /// Asset kinds, OR-ed, optionally all excluded.
    Kind { kinds: Vec<AssetKind>, negate: bool },
    /// Source-path prefix, matched the way the folders panel matches one.
    Path { prefix: String, negate: bool },
    /// At least this many stars.
    MinRating(u8),
    /// Favourite, required or excluded.
    Favorite(bool),
}

impl QueryCondition {
    /// Fold repeated conditions of the same shape into one, so `ext:png
    /// ext:jpg` asks for either rather than demanding an impossible
    /// two-extension filename.
    ///
    /// A value set merges only when its callers agree on negation:
    /// `ext:png -ext:jpg` has to stay two conditions, because folding them
    /// would turn "png but not jpg" into "png or jpg" — a wider answer set
    /// with no visible cause.
    pub fn fold(conditions: Vec<Self>) -> Vec<Self> {
        let mut out: Vec<Self> = Vec::with_capacity(conditions.len());
        for condition in conditions {
            match out.iter_mut().position(|kept| kept.merges_with(&condition)) {
                Some(index) => out[index].merge_in(&condition),
                None => out.push(condition),
            }
        }
        out
    }

    /// Whether `other` belongs in this same condition.
    fn merges_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ext { negate: a, .. }, Self::Ext { negate: b, .. }) => a == b,
            (Self::Kind { negate: a, .. }, Self::Kind { negate: b, .. }) => a == b,
            _ => false,
        }
    }

    /// Absorb `other`'s values. Only reached after [`Self::merges_with`].
    fn merge_in(&mut self, other: &Self) {
        match (self, other) {
            (Self::Ext { values, .. }, Self::Ext { values: extra, .. }) => {
                for value in extra {
                    if !values.contains(value) {
                        values.push(value.clone());
                    }
                }
            }
            (Self::Kind { kinds, .. }, Self::Kind { kinds: extra, .. }) => {
                for kind in extra {
                    if !kinds.contains(kind) {
                        kinds.push(*kind);
                    }
                }
            }
            _ => {}
        }
    }
}

/// One page of a listing: the total number of matching items plus the
/// materialized slice. Every paged query returns this instead of a raw
/// `(total, items)` tuple.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub total: u64,
    pub items: Vec<T>,
    /// `total` is a floor, not a count: the query stopped gathering before it
    /// had seen everything that matches, so more exists than is reported.
    ///
    /// A caller that renders `total` as a fact would be stating a number it
    /// does not know — which is how a capped search reads as "this library
    /// holds 2000 cat pictures". The places that set it say so.
    pub truncated: bool,
}

impl<T> Page<T> {
    pub fn new(total: u64, items: Vec<T>) -> Self {
        Self {
            total,
            items,
            truncated: false,
        }
    }

    /// [`Self::new`] with [`Page::truncated`] set.
    pub fn truncated(total: u64, items: Vec<T>) -> Self {
        Self {
            total,
            items,
            truncated: true,
        }
    }

    /// Map the items, keeping the total and the truncation (used to
    /// materialize id pages).
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            total: self.total,
            items: self.items.into_iter().map(f).collect(),
            truncated: self.truncated,
        }
    }
}
