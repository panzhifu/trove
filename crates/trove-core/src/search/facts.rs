//! Metadata facts as indexable text: what the `extra` JSON column says,
//! flattened onto the fact surfaces the schema exposes.

use crate::model::AssetFacts;

/// Per-category searchable text extracted from an asset's metadata.
///
/// Each field is empty when the asset carries no data for that category.
/// The composite [`composite`](FactTexts::composite) joins them all for the
/// catch-all `Facts` target and for pinyin/abbreviation derivation.
#[derive(Default)]

pub(super) struct FactTexts {
    pub(super) camera: String,
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) font: String,
    pub(super) audio: String,
    pub(super) embedded_title: String,
    /// The colour space the file's ICC profile claims, as the profile names
    /// it (`Adobe RGB (1998)`); empty when no profile was recorded.
    pub(super) color: String,
}

/// The matching form of a colour-space name: letters and digits only, lower
/// cased — `Adobe RGB (1998)` becomes `adobergb1998`. The trigram surface
/// indexes this, because the raw name's spaces would otherwise make
/// `color:AdobeRGB` unmatchable by construction.
pub(super) fn color_compact(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

impl FactTexts {
    /// All categories concatenated, for the composite facts field and for
    /// pinyin/abbreviation derivation.
    pub(super) fn composite(&self) -> String {
        [
            self.camera.as_str(),
            self.artist.as_str(),
            self.album.as_str(),
            self.font.as_str(),
            self.audio.as_str(),
            self.embedded_title.as_str(),
            self.color.as_str(),
        ]
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    }
}

/// Build per-category searchable text from an asset's typed metadata.
///
/// Each category collects the human-readable facts a user would type:
///
/// - **camera**: make, model, ISO, aperture, focal length, exposure time
/// - **artist**: embedded artist tag (audio/video)
/// - **album**: embedded album tag
/// - **font**: family, style, weight, glyph count
///
/// Numbers are formatted the way a user would type them (`"ISO 400"`,
/// `"f/2.8"`, `"1/60s"`), so a free-text search for `400` or `2.8` finds the
/// asset without a qualifier.  Source URL is appended to the camera text
/// because it is the closest analogue to "where this came from".
pub(super) fn extract_fact_texts(facts: &AssetFacts, source_url: Option<&str>) -> FactTexts {
    let mut camera_parts: Vec<String> = Vec::new();
    if let Some(ref s) = facts.photo.make {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.model {
        camera_parts.push(s.clone());
    }
    if let Some(iso) = facts.photo.iso {
        camera_parts.push(format!("ISO {iso}"));
    }
    if let Some(ref s) = facts.photo.aperture_f {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.focal_length_mm {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.exposure_time {
        camera_parts.push(s.clone());
    }
    if let Some(url) = source_url {
        camera_parts.push(url.to_owned());
    }

    let artist = facts.media.artist.clone().unwrap_or_default();
    let album = facts.media.album.clone().unwrap_or_default();

    let mut font_parts: Vec<String> = Vec::new();
    if let Some(ref s) = facts.font.family {
        font_parts.push(s.clone());
    }
    if let Some(ref s) = facts.font.style {
        font_parts.push(s.clone());
    }
    if let Some(w) = facts.font.weight {
        font_parts.push(w.to_string());
    }
    if let Some(g) = facts.font.glyphs {
        font_parts.push(format!("{g}glyphs"));
    }

    // Audio technical specs: a dedicated `audio:` qualifier scopes to these
    // (see `Target::Audio`), and the composite facts field still carries them
    // so unqualified searches for e.g. "48000" or "24bit" find audio assets.
    let audio_parts: Vec<String> = {
        let mut v = Vec::new();
        if let Some(hz) = facts.audio.sample_rate {
            v.push(format!("{hz} Hz"));
        }
        if let Some(ch) = facts.audio.channels {
            v.push(format!("{ch}c"));
        }
        if let Some(bd) = facts.audio.bit_depth {
            v.push(format!("{bd}bit"));
        }
        if let Some(br) = facts.audio.bitrate {
            v.push(format!("{br}kbps"));
        }
        v
    };

    let embedded_title = facts.media.embedded_title.clone().unwrap_or_default();
    let color = facts.visual.color_space.clone().unwrap_or_default();

    FactTexts {
        camera: camera_parts.join(" "),
        artist,
        album,
        font: font_parts.join(" "),
        audio: audio_parts.join(" "),
        embedded_title,
        color,
    }
}

// ============================ index ==========================================
