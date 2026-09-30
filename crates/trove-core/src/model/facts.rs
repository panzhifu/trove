//! Typed per-kind metadata persisted in the asset's `extra` JSON column.
//!
//! Historically this column was a free-form string-keyed map filled by the
//! metadata miners. It is now represented by [`AssetFacts`], whose sub-structs
//! flatten back onto the *same flat key names*, so existing rows, the smart
//! queries that compile `json_extract(assets.extra, '$.…')` against these
//! keys, and the v2 export format all keep working without a migration.
//!
//! Extending: add an `Option` field to the matching sub-struct — old data
//! lacking the key reads as `None`, and keys written by newer versions that
//! this build does not know land in [`AssetFacts::unknown`] instead of being
//! dropped. Both directions stay compatible.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// Camera facts mined from EXIF (`media::metadata`). Present for photos.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhotoFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<u32>,
    /// Aperture as the photographed convention, e.g. `f/2.8`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aperture_f: Option<String>,
    /// Focal length in mm, trimmed (`50`, `23.5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<String>,
    /// Exposure time, e.g. `1/60s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_time: Option<String>,
    /// GPS position in decimal degrees (already hemisphere-signed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_lat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_lng: Option<f64>,
}

/// Playback facts for video assets, mined from the container at import: what
/// the preview needs to start a decoder *without* a probe round trip. Both
/// keys are absent on rows imported before the group existed — the preview
/// then falls back to probing the file, exactly as it always did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoFacts {
    #[serde(rename = "video_fps", default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(
        rename = "video_has_audio",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub has_audio: Option<bool>,
}

/// Derived image appearance facts: the mined palette and the visual search
/// signature (computed in background after import, backfillable via
/// maintenance).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VisualFacts {
    /// Dominant color as lowercase `#rrggbb` (what smart `Color` matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_color: Option<String>,
    /// Full palette, strongest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_colors: Option<Vec<String>>,
    /// 64-bit perceptual hash, 16 lowercase hex chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_phash: Option<String>,
    /// Compact color-histogram encoding (`media::search::ColorHistogram`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_color_hist: Option<String>,
    /// The colour space the file itself claims — its ICC profile's own
    /// name, when the container carries one. Absent means the pixels were
    /// taken as sRGB (no profile, or a decoder without a profile channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
}

/// Font-table facts mined at import. Keys keep their historical `font_`
/// prefixes; the struct is named for the kind.
///
/// `facts_rev` marks which extractor revision filled the group: fields have
/// been added over time, and a row mined by the old extractor must look
/// *old* even though its `font_family` is present, or the re-mine would
/// never reach it (see `services::maintenance::remine_complete`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FontFacts {
    #[serde(
        rename = "font_family",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub family: Option<String>,
    #[serde(
        rename = "font_style",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub style: Option<String>,
    #[serde(
        rename = "font_weight",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub weight: Option<u16>,
    #[serde(
        rename = "font_italic",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub italic: Option<bool>,
    #[serde(
        rename = "font_glyphs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub glyphs: Option<u32>,
    /// Name ids 4 / 5 / 8 / 0: the file's own record of what it is.
    #[serde(
        rename = "font_full_name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub full_name: Option<String>,
    #[serde(
        rename = "font_version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub version: Option<String>,
    #[serde(
        rename = "font_manufacturer",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub manufacturer: Option<String>,
    #[serde(
        rename = "font_copyright",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub copyright: Option<String>,
    /// OS/2 `usWidthClass`, 1–9 (1 UltraCondensed … 9 UltraExpanded).
    #[serde(
        rename = "font_width_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub width_class: Option<u16>,
    /// `head.unitsPerEm`: the design grid everything else is measured in.
    #[serde(
        rename = "font_units_per_em",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub units_per_em: Option<u16>,
    /// OS/2 `fsSelection` bold bit — a claim about the file, as opposed to
    /// `weight`, which is the number.
    #[serde(rename = "font_bold", default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// CJK languages the OS/2 code-page bits declare (`ja` `ko` `zh-Hans`
    /// `zh-Hant` tokens); empty sets serialize as absent.
    #[serde(
        rename = "font_languages_declared",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub languages_declared: Option<Vec<String>>,
    /// The one language the file's own evidence names, if any — `latin`
    /// included, `None` when the evidence ties and we refuse to guess.
    #[serde(
        rename = "font_language",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub language: Option<String>,
    /// The `wght` axis of a variable font as `min–max` (`100–900`); static
    /// fonts carry none.
    #[serde(
        rename = "font_variable_weight",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub variable_weight: Option<String>,
    /// Which extractor revision filled this group; bump when fields change
    /// meaning or appear so the re-mine knows to revisit old rows.
    #[serde(
        rename = "font_facts_rev",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub facts_rev: Option<u8>,
}

/// The current [`FontFacts`] extractor revision.
pub const FONT_FACTS_REV: u8 = 1;

/// Embedded tag metadata (audio/video), as opposed to the user-set
/// `Asset.title`. The key `title` predates the typing and stays.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaTagsFacts {
    #[serde(rename = "title", default, skip_serializing_if = "Option::is_none")]
    pub embedded_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
}

/// Technical properties of an audio stream, as opposed to the tags above.
///
/// Keys are prefixed the way `font_glyphs` is: the `extra` column is one flat
/// map, and a bare `channels` or `bitrate` would collide the day video facts
/// get their own.
///
/// These are read from the file at import and never recomputed: there is no
/// re-mine path, so an asset imported before this existed carries none of them
/// and the inspector hides the rows rather than showing a dash.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioFacts {
    /// Hertz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    /// Kilobits per second, not bits: lofty reports `audio_bitrate` in kbps,
    /// and a label that multiplied it again would be off by a thousand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
}

/// Everything stored in the asset's `extra` JSON column, typed.
///
/// The sub-structs flatten onto one flat key map; `unknown` collects every
/// key no field claims (forward compatibility + free-form custom fields).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssetFacts {
    #[serde(flatten)]
    pub photo: PhotoFacts,
    #[serde(flatten)]
    pub video: VideoFacts,
    #[serde(flatten)]
    pub visual: VisualFacts,
    #[serde(flatten)]
    pub font: FontFacts,
    #[serde(flatten)]
    pub media: MediaTagsFacts,
    #[serde(flatten)]
    pub audio: AudioFacts,
    /// Where a linked (`Origin::Linked`) file lives on disk. Recorded at
    /// import and updated by relinking; also filtered on by the folders
    /// panel, which reads it through the schema's generated `source_path`
    /// column so the query has an index behind it.
    /// Where a linked file lives. `pub(crate)`: the owned-by-anyone-readable
    /// form of this is [`crate::model::Asset::location`], and the only writer is
    /// `Asset::set_location` -- two copies of a path, one in the record's
    /// location and one in its facts, is the disagreement this removed.
    ///
    /// The field stays *here* rather than moving into the enum on purpose: this
    /// struct passes unknown JSON keys through to `AssetFacts::unknown`, so
    /// deleting the field would not delete the key -- it would re-ingest it as
    /// an untyped entry, i.e. create the second copy from the other direction.
    /// Storage and the indexed generated column both need the key in `extra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_path: Option<String>,
    /// Keys not claimed by any typed field, preserved verbatim.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Json>,
}

impl AssetFacts {
    /// Whether any typed group carries data (`unknown` does not count).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Legacy rows / v2 exports use flat keys; they must deserialize into
    /// the typed groups and serialize back to the same flat shape.
    #[test]
    fn legacy_flat_keys_round_trip() {
        let legacy = json!({
            "font_family": "Inter",
            "font_weight": 500u16,
            "font_italic": true,
            "iso": 200u32,
            "dominant_color": "#aabbcc",
            "dominant_colors": ["#aabbcc", "#000000"],
            "title": "Song",
            "artist": "A",
            "gps_lat": 1.5,
            "source_path": "/tmp/x.png",
            "future_key": {"nested": true},
        });
        let facts: AssetFacts = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(facts.font.family.as_deref(), Some("Inter"));
        assert_eq!(facts.font.weight, Some(500));
        assert_eq!(facts.font.italic, Some(true));
        assert_eq!(facts.photo.iso, Some(200));
        assert_eq!(facts.photo.gps_lat, Some(1.5));
        assert_eq!(
            facts.visual.dominant_colors,
            Some(vec!["#aabbcc".into(), "#000000".into()])
        );
        assert_eq!(facts.media.embedded_title.as_deref(), Some("Song"));
        assert_eq!(facts.source_path.as_deref(), Some("/tmp/x.png"));
        // Unrecognized keys are preserved verbatim, not dropped.
        assert_eq!(
            facts.unknown.get("future_key"),
            Some(&json!({"nested": true}))
        );

        let out = serde_json::to_value(&facts).unwrap();
        assert_eq!(out, legacy);
    }

    #[test]
    fn empty_facts_stay_an_empty_object() {
        let facts = AssetFacts::default();
        assert!(facts.is_empty());
        assert_eq!(serde_json::to_value(&facts).unwrap(), json!({}));
        let parsed: AssetFacts = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, facts);
    }
}
