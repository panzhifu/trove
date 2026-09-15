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
