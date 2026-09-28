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
    /// The file was copied into the library (content-addressed storage). Only
    /// ever a copy Trove made for itself: an extracted media package, the
    /// re-encoded output of an in-place edit.
    Stored,
    /// The file is referenced at its external path, which rides in the asset's
    /// mined facts (`extra["source_path"]`). What a user import produces.
    Linked,
}

/// Where an asset's bytes live.
///
/// The library has these states whether the type names them or not, and naming
/// them is the whole point: `origin` and `rel_path` are two independent columns,
/// so "stored with no path" and "linked with no path" were both writable, and
/// every reader had to remember which of them meant what. `Library::asset_file`
/// and `media::thumb::blob_path` turned the second one into a plain `None`,
/// which is a wrong answer wearing the right clothes — it looks exactly like a
/// file the user moved.
///
/// Read it with [`Asset::location`], change it with [`Asset::set_location`].
/// Those two are the only public way through the underlying columns and the
/// `extra.source_path` key, which is what keeps the pairs consistent without
/// asking every caller to re-derive them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetLocation {
    /// The library holds the file, content-addressed under its root.
    Stored { rel_path: String },
    /// A record a metadata restore created before its blob arrived: name, tags,
    /// rating and dates are real, the file is not. Import turns it into
    /// [`AssetLocation::Stored`] the moment the matching content hash lands, and
    /// orphan cleanup ignores it until then — that ignore rule is a *state*, not
    /// a missing value, which is why it has a variant.
    Placeholder,
    /// The file stays where the user put it, outside the library. What a normal
    /// import produces; the path was recorded at import time.
    Linked { source_path: String },
    /// A `linked` row with no recorded path. Only [`Asset::location`] can hand
    /// this back — `media::import` always records a path — so reaching it means
    /// a database written by something other than the current importer. There is
    /// no file to open and no restore will bring one, which is the difference
    /// between this and [`AssetLocation::Placeholder`].
    Unrecorded,
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

/// Media-industry aspect-ratio presets for the shape filter: a width/height
/// band the asset's dimensions must fall into. Standard canvases vary a pixel
/// or two around the nominal ratio, so matching is tolerant — see
/// [`AspectPreset::ratio_range`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AspectPreset {
    /// WeChat Official Account cover canvas (900×383, 2.35:1).
    WechatCover,
    /// Wide video cover / thumbnail (16:9).
    VideoWide,
    /// Vertical short-video canvas (9:16).
    VideoVertical,
    /// Classic photo landscape (4:3).
    PhotoLandscape,
    /// Classic photo portrait (3:4).
    PhotoPortrait,
    /// Square canvas (1:1).
    Square,
}

/// Relative tolerance around a preset's nominal ratio. Standard canvases
/// ship at slightly rounded sizes (e.g. a "2.35:1" cover at 900×383 is
/// 2.3499…), so an exact comparison would miss real matches.
pub const ASPECT_TOLERANCE: f32 = 0.03;

impl AspectPreset {
    /// The nominal width/height ratio of the preset.
    pub fn ratio(self) -> f32 {
        match self {
            AspectPreset::WechatCover => 2.35,
            AspectPreset::VideoWide => 16.0 / 9.0,
            AspectPreset::VideoVertical => 9.0 / 16.0,
            AspectPreset::PhotoLandscape => 4.0 / 3.0,
            AspectPreset::PhotoPortrait => 3.0 / 4.0,
            AspectPreset::Square => 1.0,
        }
    }

    /// The inclusive width/height band an asset matches: the nominal ratio
    /// shrunk/grown by [`ASPECT_TOLERANCE`] on each side.
    pub fn ratio_range(self) -> (f32, f32) {
        let r = self.ratio();
        ((1.0 - ASPECT_TOLERANCE) * r, (1.0 + ASPECT_TOLERANCE) * r)
    }
}

/// Resolution band by an asset's **longer edge**, the coarse "is this 4K?"
/// question the shape filter cannot ask: [`AspectPreset`] and
/// [`Orientation`] are both about proportions, and a 1920×1080 frame and a
/// 7680×4320 one are the same shape.
///
/// The bounds sit between the tiers rather than on a size anyone ships, so a
/// canvas named for its width ("4K" = 3840, "2K" = 2560, and the 2240-ish DCI
/// variants of each) lands in exactly one band without a tolerance rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionBand {
    /// Under 2240 px: HD, 2K DCI and everything smaller.
    #[serde(rename = "1k")]
    OneK,
    /// 2240–3199 px: QHD / 1440p and its neighbours.
    #[serde(rename = "2k")]
    TwoK,
    /// 3200 px and up: UHD / 4K and larger.
    #[serde(rename = "4k")]
    FourK,
}

/// The longer-edge bounds of the three bands, in pixels.
pub const RESOLUTION_BAND_BOUNDS: (i64, i64) = (2240, 3200);

impl ResolutionBand {
    /// The inclusive longer-edge range this band matches. The top band's
    /// upper bound is `i64::MAX` rather than an `Option` so every band is one
    /// SQL `BETWEEN`, and a file dimension cannot reach it.
    pub fn px_range(self) -> (i64, i64) {
        match self {
            ResolutionBand::OneK => (0, RESOLUTION_BAND_BOUNDS.0 - 1),
            ResolutionBand::TwoK => (RESOLUTION_BAND_BOUNDS.0, RESOLUTION_BAND_BOUNDS.1 - 1),
            ResolutionBand::FourK => (RESOLUTION_BAND_BOUNDS.1, i64::MAX),
        }
    }
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
    /// BLAKE3 of the file content, hex (64 characters). Deduplication key,
    /// blob name and thumbnail cache key at once.
    pub content_hash: Option<String>,
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

impl Asset {
    /// The file name without its extension: the `{name}` rename token and
    /// the display fallback when no title is set.
    pub fn file_stem(&self) -> String {
        std::path::Path::new(&self.file_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.file_name)
            .to_string()
    }

    /// Where this asset's bytes live, as the four states the library actually
    /// has rather than as two columns that happen to be filled a certain way.
    ///
    /// Total by design: a stored record with no path is a
    /// [`AssetLocation::Placeholder`], and a linked record with no path is
    /// [`AssetLocation::Unrecorded`]. Neither can fail, because a decoder that
    /// returned `Option` would push the "what did this mean?" decision onto
    /// every caller — which is the bug this replaces.
    pub fn location(&self) -> AssetLocation {
        match self.origin {
            Origin::Stored => match &self.rel_path {
                Some(rel_path) => AssetLocation::Stored {
                    rel_path: rel_path.clone(),
                },
                None => AssetLocation::Placeholder,
            },
            Origin::Linked => match &self.facts.source_path {
                Some(source_path) => AssetLocation::Linked {
                    source_path: source_path.clone(),
                },
                None => AssetLocation::Unrecorded,
            },
        }
    }

    /// Point this record at `location`, writing the `origin` and `rel_path`
    /// columns and the `extra.source_path` key that state implies.
    ///
    /// A linked state clears `rel_path` and a stored one clears the recorded
    /// path from the *location* it hands back, but switching *away* from linked
    /// deliberately leaves any old `source_path` key in `extra`: deleting a fact
    /// no reader asked us to delete is a data change, and this round's promise
    /// is that behaviour does not move. The key is only ever read under
    /// [`AssetLocation::Linked`], so the stale copy cannot be mistaken for the
    /// live one.
    pub fn set_location(&mut self, location: AssetLocation) {
        match location {
            AssetLocation::Stored { rel_path } => {
                self.origin = Origin::Stored;
                self.rel_path = Some(rel_path);
            }
            AssetLocation::Placeholder => {
                self.origin = Origin::Stored;
                self.rel_path = None;
            }
            AssetLocation::Linked { source_path } => {
                self.origin = Origin::Linked;
                self.rel_path = None;
                self.facts.source_path = Some(source_path);
            }
            AssetLocation::Unrecorded => {
                self.origin = Origin::Linked;
                self.rel_path = None;
                self.facts.source_path = None;
            }
        }
    }
}

/// Input describing a new asset, before the importer fills media facts.
#[derive(Debug, Clone)]
pub struct NewAsset {
    pub file_name: String,
    pub ext: String,
    pub mime: String,
    pub size_bytes: u64,
    pub content_hash: String,
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

#[cfg(test)]
mod aspect_tests {
    use super::{ASPECT_TOLERANCE, AspectPreset};

    #[test]
    fn ratio_ranges_around_nominal_with_tolerance() {
        for preset in [
            AspectPreset::WechatCover,
            AspectPreset::VideoWide,
            AspectPreset::VideoVertical,
            AspectPreset::PhotoLandscape,
            AspectPreset::PhotoPortrait,
            AspectPreset::Square,
        ] {
            let r = preset.ratio();
            let (lo, hi) = preset.ratio_range();
            assert!((lo - (1.0 - ASPECT_TOLERANCE) * r).abs() < 1e-6);
            assert!((hi - (1.0 + ASPECT_TOLERANCE) * r).abs() < 1e-6);
            assert!(lo < r && r < hi);
        }
    }

    #[test]
    fn nominal_canvases_land_inside_their_band() {
        // Real-world canvas sizes, including the rounded ones.
        let cases = [
            (AspectPreset::WechatCover, 900, 383),
            (AspectPreset::WechatCover, 1000, 424), // 2.358…, +0.4% off
            (AspectPreset::VideoWide, 1920, 1080),
            (AspectPreset::VideoVertical, 1080, 1920),
            (AspectPreset::PhotoLandscape, 640, 480),
            (AspectPreset::PhotoPortrait, 480, 640),
            (AspectPreset::Square, 64, 64),
        ];
        for (preset, w, h) in cases {
            let ratio = w as f32 / h as f32;
            let (lo, hi) = preset.ratio_range();
            assert!((lo..=hi).contains(&ratio), "{preset:?}: {ratio}");
        }
    }

    #[test]
    fn the_bands_stay_disjoint() {
        // Ordered by nominal ratio; neighbouring presets must not overlap,
        // or one canvas could match two menus at once.
        let mut presets = [
            AspectPreset::Square,
            AspectPreset::PhotoLandscape,
            AspectPreset::VideoWide,
            AspectPreset::WechatCover,
        ];
        presets.sort_by(|a, b| a.ratio().total_cmp(&b.ratio()));
        for pair in presets.windows(2) {
            assert!(
                pair[0].ratio_range().1 < pair[1].ratio_range().0,
                "{:?} and {:?} overlap",
                pair[0],
                pair[1]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    /// A record with every field set to something other than its default.
    ///
    /// The pinning below only catches a dropped field if that field would come
    /// back empty and therefore look unchanged, so a fixture of defaults proves
    /// nothing.
    fn full_asset() -> Asset {
        let stamp = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut facts = AssetFacts::default();
        facts.photo.make = Some("Nikon".into());
        facts.photo.iso = Some(400);
        facts
            .unknown
            .insert("pinned".into(), Json::String("value".into()));
        Asset {
            id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            origin: Origin::Stored,
            rel_path: Some("media/00/pinned.png".into()),
            file_name: "pinned.png".into(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 4096,
            content_hash: Some("b".repeat(64)),
            kind: AssetKind::Image,
            width: Some(800),
            height: Some(600),
            duration_ms: Some(1200),
            captured_at: Some(stamp),
            title: Some("A title".into()),
            description: Some("A description".into()),
            rating: Some(4),
            is_favorite: true,
            source_url: Some("https://example.test/pinned".into()),
            usage_status: UsageStatus::Used,
            commercial_use: Some(true),
            facts,
            created_at: stamp,
            updated_at: stamp,
            trashed_at: Some(stamp),
        }
    }

    /// The keys the v2 export and archive format is made of, by name.
    ///
    /// Written out rather than derived, so a field added to `Asset` without a
    /// matching line in the writer shows up as a failing assertion here instead
    /// of as an export that quietly lost something.
    const WIRE_KEYS: [&str; 24] = [
        "captured_at",
        "commercial_use",
        "content_hash",
        "created_at",
        "description",
        "duration_ms",
        "ext",
        "extra",
        "file_name",
        "height",
        "id",
        "is_favorite",
        "kind",
        "mime",
        "origin",
        "rating",
        "rel_path",
        "size_bytes",
        "source_url",
        "title",
        "trashed_at",
        "updated_at",
        "usage_status",
        "width",
    ];

    fn keys_of(asset: &Asset) -> BTreeSet<String> {
        serde_json::to_value(asset)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Every key the format has, for each of the four location states.
    #[test]
    fn the_exported_key_set_is_the_v2_shape_whatever_the_location() {
        let expected: BTreeSet<String> = WIRE_KEYS.iter().map(|k| (*k).to_string()).collect();
        for location in [
            AssetLocation::Stored {
                rel_path: "media/00/pinned.png".into(),
            },
            AssetLocation::Placeholder,
            AssetLocation::Linked {
                source_path: "/tmp/pinned.png".into(),
            },
            AssetLocation::Unrecorded,
        ] {
            let mut asset = full_asset();
            asset.set_location(location);
            assert_eq!(keys_of(&asset), expected, "the key set moved");
        }
    }

    /// Where each state puts its path: `origin` + `rel_path` at the top level,
    /// a linked path inside `extra` — because the schema's indexed `source_path`
    /// column is generated from that JSON key, so moving the key would move the
    /// index with it.
    #[test]
    fn each_state_writes_its_path_where_the_format_already_keeps_it() {
        let cases = [
            (
                AssetLocation::Stored {
                    rel_path: "media/00/pinned.png".into(),
                },
                "stored",
                Json::String("media/00/pinned.png".into()),
                None,
            ),
            (AssetLocation::Placeholder, "stored", Json::Null, None),
            (
                AssetLocation::Linked {
                    source_path: "/tmp/pinned.png".into(),
                },
                "linked",
                Json::Null,
                Some("/tmp/pinned.png"),
            ),
            (AssetLocation::Unrecorded, "linked", Json::Null, None),
        ];
        for (location, origin, rel_path, source) in cases {
            let mut asset = full_asset();
            asset.set_location(location);
            let value = serde_json::to_value(&asset).unwrap();
            assert_eq!(value["origin"], Json::String(origin.into()));
            assert_eq!(value["rel_path"], rel_path);
            match source {
                Some(path) => assert_eq!(value["extra"]["source_path"], Json::String(path.into())),
                None => assert!(
                    value["extra"].get("source_path").is_none(),
                    "a state with no path must not write one"
                ),
            }
        }
    }

    /// The strongest form of "the type changed, the format did not": a record
    /// with every field set goes through its own JSON and comes back identical.
    /// A field that stops being written, or stops being read, fails here — and
    /// `full_asset` is why this catches anything at all.
    #[test]
    fn a_full_record_survives_its_own_json() {
        let asset = full_asset();
        let back: Asset = serde_json::from_value(serde_json::to_value(&asset).unwrap()).unwrap();
        assert_eq!(back, asset);
    }

    /// `location` and `set_location` agree on the three states a writer can
    /// produce. `Unrecorded` is deliberately not in that list: it is what the
    /// reader invents when a row is missing its path, and setting it is a way to
    /// repair such a row, not a state import produces.
    #[test]
    fn the_states_a_writer_can_produce_round_trip_through_the_columns() {
        for location in [
            AssetLocation::Stored {
                rel_path: "media/00/pinned.png".into(),
            },
            AssetLocation::Placeholder,
            AssetLocation::Linked {
                source_path: "/tmp/pinned.png".into(),
            },
        ] {
            let mut asset = full_asset();
            asset.set_location(location.clone());
            assert_eq!(
                asset.location(),
                location,
                "state did not survive the write"
            );
        }
        // And the row nobody writes is still readable rather than fatal.
        let mut asset = full_asset();
        asset.origin = Origin::Linked;
        asset.rel_path = None;
        asset.facts.source_path = None;
        assert_eq!(asset.location(), AssetLocation::Unrecorded);
    }
}
