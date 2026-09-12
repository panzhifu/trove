//! Metadata mining: extract rich facts (capture date, camera parameters, audio
//! tags, duration) from imported media files.
//!
//! EXIF / audio decoding happens here. Every extractor is best-effort: a file
//! that cannot be parsed yields an empty [`MinedMetadata`] rather than an error,
//! so mining never fails an import.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{TimeZone, Utc};
use exif::{Field, In, Tag, Value};
use serde_json::Value as Json;

use super::color;
use crate::model::AssetKind;

/// Metadata facts mined from a media file, merged into the asset on import.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinedMetadata {
    /// When the photo was taken (EXIF `DateTimeOriginal`). `None` when unknown.
    pub captured_at: Option<chrono::DateTime<Utc>>,
    /// Playback duration, present for audio that carries it.
    pub duration_ms: Option<u64>,
    /// Natural title (audio `title` tag, …).
    pub title: Option<String>,
    /// Free-form camera / tag fields, stored in the asset's `extra` map.
    pub extra: BTreeMap<String, Json>,
}

impl MinedMetadata {
    fn insert(&mut self, key: &str, value: impl Into<Json>) {
        self.extra.insert(key.to_string(), value.into());
    }
}

/// Mine metadata for a blob of `kind`. Never fails: unsupported kinds and
/// undecodable files both fall back to empty metadata.
pub fn mine(path: &Path, kind: AssetKind) -> MinedMetadata {
    match kind {
        AssetKind::Image => mine_image(path),
        AssetKind::Audio => mine_audio(path).unwrap_or_default(),
        AssetKind::Font => mine_font(path).unwrap_or_default(),
        // Video duration rides the mp4 container when it is one (mkv/webm/avi
        // fall back to defaults); documents keep only the probe facts.
        AssetKind::Video => mine_video(path).unwrap_or_default(),
        _ => MinedMetadata::default(),
    }
}

// -- font ---------------------------------------------------------------------

/// Read font facts (family, style, weight) from the name/OS2 tables via
/// ttf-parser. Covers ttf/otf/ttc (first face) and woff; woff2 would need a
/// Brotli decoder and stays metadata-less.
fn mine_font(path: &Path) -> Option<MinedMetadata> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;
    let mut m = MinedMetadata::default();

    let family = face_name(&face, ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
        .or_else(|| face_name(&face, ttf_parser::name_id::FAMILY))?;
    m.insert("font_family", family);
    if let Some(style) = face_name(&face, ttf_parser::name_id::TYPOGRAPHIC_SUBFAMILY)
        .or_else(|| face_name(&face, ttf_parser::name_id::SUBFAMILY))
    {
        m.insert("font_style", style);
    }
    let weight = face.weight().to_number();
    if weight != 0 {
        m.insert("font_weight", weight);
    }
    if face.style() == ttf_parser::Style::Italic {
        m.insert("font_italic", true);
    }
    m.insert("font_glyphs", face.number_of_glyphs());
    Some(m)
}

/// The family (and optional subfamily) recorded in a font file's name
/// table, preferring the Windows/English records. Covers the same formats
/// as [`mine_font`] (ttf/otf/ttc, first face). Powers the system-font
/// browser, which needs names without importing anything.
pub fn font_family(path: &Path) -> Option<(String, Option<String>)> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;
    let family = face_name(&face, ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
        .or_else(|| face_name(&face, ttf_parser::name_id::FAMILY))?;
    let style = face_name(&face, ttf_parser::name_id::TYPOGRAPHIC_SUBFAMILY)
        .or_else(|| face_name(&face, ttf_parser::name_id::SUBFAMILY));
    Some((family, style))
}

/// One name record out of a font's name table: prefer Windows/English,
/// accept any language as a fallback.
fn face_name(face: &ttf_parser::Face, name_id: u16) -> Option<String> {
    face.names()
        .into_iter()
        .filter(|n| n.name_id == name_id)
        .find(|n| n.is_unicode())
        .and_then(|n| n.to_string())
        .or_else(|| {
            face.names()
                .into_iter()
                .filter(|n| n.name_id == name_id)
                .find_map(|n| n.to_string())
        })
}

// -- video --------------------------------------------------------------------

/// Container duration of an MP4-family file (dimensions are probed separately
/// in `probe::video_facts` because they belong on the asset row).
fn mine_video(path: &Path) -> Option<MinedMetadata> {
    let facts = super::probe::video_facts(path)?;
    Some(MinedMetadata {
        duration_ms: facts.duration_ms,
        ..Default::default()
    })
}

// -- image -------------------------------------------------------------------

/// Extract universal color facts plus EXIF (camera, timestamp) from a raster.
///
/// Color always runs and never fails; EXIF is best-effort on top. Unlike the
/// other extractors this therefore never short-circuits — a photo without EXIF
/// still yields its dominant palette.
fn mine_image(path: &Path) -> MinedMetadata {
    let mut m = MinedMetadata::default();

    // Universal color facts: present for every decodable image, unlike EXIF.
    let palette = color::dominant_colors(path);
    if let Some(first) = palette.first() {
        m.insert("dominant_color", first.clone());
        m.insert(
            "dominant_colors",
            Json::Array(palette.into_iter().map(Json::String).collect()),
        );
    }

    // EXIF below is best-effort: unreadable or EXIF-less images stop here but
    // keep the color facts already collected.
    let Ok(file) = std::fs::File::open(path) else {
        return m;
    };
    let mut reader = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else {
        return m;
    };

    // Capture time: favor `DateTimeOriginal`, fall back to `DateTimeDigitized`.
    let taken = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTimeDigitized, In::PRIMARY));
    if let Some(date) = taken {
        m.captured_at = parse_exif_datetime(&date.display_value().to_string());
    }

    if let Some(v) = exif.get_field(Tag::Make, In::PRIMARY) {
        m.insert("make", v.display_value().to_string());
    }
    if let Some(v) = exif.get_field(Tag::Model, In::PRIMARY) {
        m.insert("model", v.display_value().to_string());
    }
    if let Some(v) = exif.get_field(Tag::PhotographicSensitivity, In::PRIMARY)
        && let Some(first) = v.value.get_uint(0)
    {
        m.insert("iso", first);
    }
    if let Some(v) = exif.get_field(Tag::FNumber, In::PRIMARY)
        && let Some(r) = first_ratio(&v.value)
    {
        m.insert("aperture_f", format!("f/{r}"));
    }
    if let Some(v) = exif.get_field(Tag::FocalLength, In::PRIMARY)
        && let Some(r) = first_ratio(&v.value)
    {
        m.insert("focal_length_mm", trimmed(r));
    }
    if let Some(v) = exif.get_field(Tag::ExposureTime, In::PRIMARY)
        && let Some(r) = first_ratio(&v.value)
    {
        m.insert("exposure_time", format!("{r}s"));
    }

    if let (Some(lat), Some(lng)) = (
        exif.get_field(Tag::GPSLatitude, In::PRIMARY),
        exif.get_field(Tag::GPSLongitude, In::PRIMARY),
    ) {
        let lat = dms_to_decimal(&lat.value);
        let lng = dms_to_decimal(&lng.value);
        if let (Some(lat), Some(lng)) = (lat, lng) {
            let lat = if south(exif.get_field(Tag::GPSLatitudeRef, In::PRIMARY)) {
                -lat
            } else {
                lat
            };
            let lng = if west(exif.get_field(Tag::GPSLongitudeRef, In::PRIMARY)) {
                -lng
            } else {
                lng
            };
            m.insert("gps_lat", num(lat));
            m.insert("gps_lng", num(lng));
        }
    }

    m
}

/// First value of an EXIF rational field as a plain float.
fn first_ratio(value: &Value) -> Option<f64> {
    match value {
        Value::Rational(ratios) => ratios.first().map(|r| div(r.num, r.denom)),
        Value::SRational(ratios) => ratios
            .first()
            .map(|r| div(r.num.unsigned_abs(), r.denom.unsigned_abs())),
        _ => None,
    }
}

fn div(num: u32, denom: u32) -> f64 {
    if denom == 0 {
        0.0
    } else {
        num as f64 / denom as f64
    }
}

/// `178/100` → `1.8` (trim a trailing `.0` for whole numbers).
fn trimmed(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// `2020:01:02 03:04:05` (EXIF) → UTC. EXIF stores no timezone offset, so the
/// naive value is treated as UTC; ambiguity in local times is unresolved but
/// rare and harmless for a capture time.
fn parse_exif_datetime(s: &str) -> Option<chrono::DateTime<Utc>> {
    let s = s.trim();
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S")
        .ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())?;
    Some(Utc.from_utc_datetime(&naive))
}

/// Degrees/minutes/seconds value → decimal degrees.
fn dms_to_decimal(value: &Value) -> Option<f64> {
    let ratios = match value {
        Value::Rational(r) => r,
        Value::SRational(_) => {
            // GPS uses unsigned Rational; SRational only appears malformed.
            return None;
        }
        _ => return None,
    };
    if ratios.len() < 2 {
        return None;
    }
    let mut decimal =
        div(ratios[0].num, ratios[0].denom) + div(ratios[1].num, ratios[1].denom) / 60.0;
    if let Some(sec) = ratios.get(2) {
        decimal += div(sec.num, sec.denom) / 3600.0;
    }
    Some(decimal)
}

/// Waypoint of a GPS hemisphere: the ref field (`S`/`W`) when present.
fn gps_ref(field: Option<&Field>, bad: u8) -> bool {
    match field {
        Some(f) => match &f.value {
            Value::Ascii(v) => v.first().and_then(|s| s.first()).is_some_and(|&b| b == bad),
            _ => false,
        },
        None => false,
    }
}

fn south(field: Option<&Field>) -> bool {
    gps_ref(field, b'S')
}

fn west(field: Option<&Field>) -> bool {
    gps_ref(field, b'W')
}

fn num(v: f64) -> Json {
    Json::Number(serde_json::Number::from_f64(v).unwrap_or_else(|| serde_json::Number::from(0)))
}

// -- audio -------------------------------------------------------------------

/// Extract tags + duration from an audio file via lofty. Best-effort: an
/// undecodable file returns `None` (caller falls back to empty metadata).
fn mine_audio(path: &Path) -> Option<MinedMetadata> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    let tagged = Probe::open(path).ok()?.read().ok()?;
    let mut m = MinedMetadata::default();

    // Prefer a title from any tag present. Accessor values are `Cow<str>`.
    if let Some(title) = tagged
        .tags()
        .iter()
        .find_map(|t| t.title().map(|s| s.into_owned()))
    {
        m.title = Some(title.clone());
        m.insert("title", title);
    }
    if let Some(artist) = tagged
        .tags()
        .iter()
        .find_map(|t| t.artist().map(|s| s.into_owned()))
    {
        m.insert("artist", artist);
    }
    if let Some(album) = tagged
        .tags()
        .iter()
        .find_map(|t| t.album().map(|s| s.into_owned()))
    {
        m.insert("album", album);
    }

    let duration = tagged.properties().duration();
    if !duration.is_zero() {
        m.duration_ms = Some(duration.as_millis() as u64);
    }

    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn plain_png_yields_color_but_no_exif() {
        let m = mine(&tmp("plain.png", PNG_1X1), AssetKind::Image);
        // No EXIF on a hand-crafted PNG…
        assert!(m.captured_at.is_none());
        // …but the universal color facts are always present.
        assert!(m.extra.contains_key("dominant_color"));
        assert!(m.extra.contains_key("dominant_colors"));
    }

    #[test]
    fn garbage_audio_and_unknown_kinds_never_panic() {
        let g = tmp("garbage.mp3", b"not really an mp3");
        let m = mine(&g, AssetKind::Audio);
        assert!(m.duration_ms.is_none());
        assert!(m.extra.is_empty());
        for kind in [
            AssetKind::Video,
            AssetKind::Document,
            AssetKind::Archive,
            AssetKind::Font,
            AssetKind::Other,
        ] {
            assert_eq!(mine(&g, kind), MinedMetadata::default());
        }
    }

    #[test]
    fn font_metadata_mines_family_from_a_real_face() {
        // Hermetic when the system has no fonts: the assertions only run on a
        // found face; otherwise the test only proves garbage never panics.
        let face = find_system_font();
        let Some(path) = face else { return };
        let m = mine(&path, AssetKind::Font);
        assert!(
            m.extra
                .get("font_family")
                .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())),
            "family missing for {}",
            path.display()
        );
    }

    fn find_system_font() -> Option<std::path::PathBuf> {
        fn walk(dir: &Path, depth: usize) -> Option<std::path::PathBuf> {
            if depth > 4 {
                return None;
            }
            let entries = std::fs::read_dir(dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && let Some(found) = walk(&path, depth + 1)
                {
                    return Some(found);
                }
                let is_face = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "ttf" | "otf"));
                if is_face {
                    return Some(path);
                }
            }
            None
        }
        ["/usr/share/fonts", "/usr/local/share/fonts"]
            .iter()
            .find_map(|root| walk(std::path::Path::new(root), 0))
    }

    #[test]
    fn exif_datetime_parsing() {
        let utc = parse_exif_datetime("2020:03:04 05:06:07").unwrap();
        assert_eq!(utc.to_rfc3339(), "2020-03-04T05:06:07+00:00");
        assert!(parse_exif_datetime("garbage").is_none());
    }
}
