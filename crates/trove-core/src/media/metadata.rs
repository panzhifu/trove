//! Metadata mining: extract rich facts (capture date, camera parameters, audio
//! tags, duration) from imported media files.
//!
//! EXIF / audio decoding happens here. Every extractor is best-effort: a file
//! that cannot be parsed yields an empty [`MinedMetadata`] rather than an error,
//! so mining never fails an import.

use std::path::Path;

use chrono::{TimeZone, Utc};
use exif::{Field, In, Tag, Value};

use super::color;
use crate::model::{AssetFacts, AssetKind};

/// Metadata facts mined from a media file, merged into the asset on import.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinedMetadata {
    /// When the photo was taken (EXIF `DateTimeOriginal`). `None` when unknown.
    pub captured_at: Option<chrono::DateTime<Utc>>,
    /// Playback duration, present for audio that carries it.
    pub duration_ms: Option<u64>,
    /// Natural title (audio `title` tag, …).
    pub title: Option<String>,
    /// Typed per-kind facts, persisted in the asset's `extra` JSON column.
    pub facts: AssetFacts,
}

/// Mine metadata for a blob of `kind`. Never fails: unsupported kinds and
/// undecodable files both fall back to empty metadata.
///
/// `color_source` is where the dominant-colour palette is read from. Images
/// are decoded at full size once for the thumbnail (`thumb::ensure`); pointing
/// `color_source` at that thumbnail avoids a second full decode of the
/// original — on a 6000x4000 JPEG that difference is measured at ~120 ms per
/// file. EXIF still reads `path` itself: thumbnails do not carry it. Passing
/// `path` for both restores the old behaviour (used by tests and callers
/// without a thumbnail).
///
/// Callers that already hold the decode prefer [`mine_from_palette`], which
/// skips the palette read entirely.
pub fn mine(path: &Path, kind: AssetKind, color_source: &Path) -> MinedMetadata {
    mine_impl(path, kind, None, color_source)
}

/// [`mine`] with the palette already computed from a decode the caller holds —
/// the import pipeline's path, where one decode feeds the thumbnail, the
/// palette and the visual signature. Only the palette is taken from the
/// caller; EXIF still reads `path`.
pub fn mine_from_palette(path: &Path, kind: AssetKind, palette: Vec<String>) -> MinedMetadata {
    mine_impl(path, kind, Some(palette), path)
}

fn mine_impl(
    path: &Path,
    kind: AssetKind,
    palette: Option<Vec<String>>,
    color_source: &Path,
) -> MinedMetadata {
    match kind {
        AssetKind::Image => mine_image(
            path,
            palette.unwrap_or_else(|| color::dominant_colors(color_source)),
        ),
        AssetKind::Audio => mine_audio(path).unwrap_or_default(),
        AssetKind::Font => mine_font(path).unwrap_or_default(),
        // Videos, documents and archives carry only what the probe stage found.
        // A video's duration is already on the shared stage state — the mp4
        // moov box, or `ffprobe` for containers the mp4 reader cannot open — and
        // the mine stage copies it across, so reading the container again here
        // would only parse it twice.
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
    let font = &mut m.facts.font;
    font.family = Some(family);
    if let Some(style) = face_name(&face, ttf_parser::name_id::TYPOGRAPHIC_SUBFAMILY)
        .or_else(|| face_name(&face, ttf_parser::name_id::SUBFAMILY))
    {
        font.style = Some(style);
    }
    let weight = face.weight().to_number();
    if weight != 0 {
        font.weight = Some(weight);
    }
    if face.style() == ttf_parser::Style::Italic {
        font.italic = Some(true);
    }
    font.glyphs = Some(face.number_of_glyphs() as u32);
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

// -- image -------------------------------------------------------------------

/// Extract universal color facts plus EXIF (camera, timestamp) from a raster.
///
/// Color always runs and never fails; EXIF is best-effort on top. Unlike the
/// other extractors this therefore never short-circuits — a photo without EXIF
/// still yields its dominant palette.
///
/// `palette` is handed in already computed (the pipeline's shared decode, or
/// the thumbnail an older caller pointed at — see [`mine`]); EXIF always reads
/// `path`, which carries the metadata.
fn mine_image(path: &Path, palette: Vec<String>) -> MinedMetadata {
    let mut m = MinedMetadata::default();

    // Universal color facts: present for every decodable image, unlike EXIF.
    if let Some(first) = palette.first() {
        let visual = &mut m.facts.visual;
        visual.dominant_color = Some(first.clone());
        visual.dominant_colors = Some(palette);
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
    if let Some(date) = taken
        && let Some(raw) = exif_text(&date.value)
    {
        m.captured_at = parse_exif_datetime(&raw);
    }

    let photo = &mut m.facts.photo;
    if let Some(v) = exif.get_field(Tag::Make, In::PRIMARY) {
        photo.make = exif_text(&v.value);
    }
    if let Some(v) = exif.get_field(Tag::Model, In::PRIMARY) {
        photo.model = exif_text(&v.value);
    }
    if let Some(v) = exif.get_field(Tag::PhotographicSensitivity, In::PRIMARY)
        && let Some(first) = v.value.get_uint(0)
    {
        photo.iso = Some(first);
    }
    if let Some(v) = exif.get_field(Tag::FNumber, In::PRIMARY)
        && let Some(r) = first_ratio(&v.value)
    {
        photo.aperture_f = Some(format!("f/{r}"));
    }
    if let Some(v) = exif.get_field(Tag::FocalLength, In::PRIMARY)
        && let Some(r) = first_ratio(&v.value)
    {
        photo.focal_length_mm = Some(trimmed(r));
    }
    if let Some(v) = exif.get_field(Tag::ExposureTime, In::PRIMARY)
        && let Some(text) = exposure_text(&v.value)
    {
        photo.exposure_time = Some(text);
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
            photo.gps_lat = Some(lat);
            photo.gps_lng = Some(lng);
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

/// The shutter speed as a photographer writes it: `1/60s`, `1/8s`, `2.5s`.
///
/// The stored value is a ratio, and the sub-second case is the common one — so
/// formatting it as a decimal would put `0.016666666666666666s` on screen for a
/// normal 1/60 exposure. Flipping the fraction keeps the number the dial showed.
fn exposure_text(value: &Value) -> Option<String> {
    let Value::Rational(ratios) = value else {
        return None;
    };
    let r = ratios.first()?;
    if r.denom == 0 || r.num == 0 {
        return None;
    }
    if r.num < r.denom {
        // `1/60`, not `1/60.0000001`: the denominator is what the dial reads.
        let per_second = (r.denom as f64 / r.num as f64).round();
        Some(format!("1/{per_second}s"))
    } else {
        Some(format!("{}s", trimmed(div(r.num, r.denom))))
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

/// An EXIF text field as the plain string it holds.
///
/// `display_value()` is the call that looks right and is not: it exists for
/// debugging output and **quotes** string values, so a make that should read
/// `Canon` comes back as `"Canon"`. That one wrapper was why every capture date
/// this miner touched failed to parse — a quoted string matches no date format —
/// which left the timeline sorting photos by the day they were imported.
fn exif_text(value: &Value) -> Option<String> {
    let Value::Ascii(parts) = value else {
        return None;
    };
    let text = parts
        .first()
        .map(|part| String::from_utf8_lossy(part).to_string())?;
    // Conforming writers drop the terminator; plenty do not.
    let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!text.is_empty()).then(|| text.to_string())
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

// -- audio -------------------------------------------------------------------

/// Extract tags + duration from an audio file via lofty. Best-effort: an
/// undecodable file returns `None` (caller falls back to empty metadata).
fn mine_audio(path: &Path) -> Option<MinedMetadata> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    // `guess_file_type` reads the container from the bytes and only falls back
    // to the extension hint when it cannot, which matters for `.oga`: an
    // ordinary Ogg stream that lofty refuses to identify by name, so every
    // tagged file with that extension would otherwise lose its duration and
    // tags to an extension that is real but unmapped.
    let tagged = Probe::open(path)
        .ok()?
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let mut m = MinedMetadata::default();
    let media = &mut m.facts.media;

    // Prefer a title from any tag present. Accessor values are `Cow<str>`.
    if let Some(title) = tagged
        .tags()
        .iter()
        .find_map(|t| t.title().map(|s| s.into_owned()))
    {
        m.title = Some(title.clone());
        media.embedded_title = Some(title);
    }
    if let Some(artist) = tagged
        .tags()
        .iter()
        .find_map(|t| t.artist().map(|s| s.into_owned()))
    {
        media.artist = Some(artist);
    }
    if let Some(album) = tagged
        .tags()
        .iter()
        .find_map(|t| t.album().map(|s| s.into_owned()))
    {
        media.album = Some(album);
    }

    let props = tagged.properties();
    let duration = props.duration();
    if !duration.is_zero() {
        m.duration_ms = Some(duration.as_millis() as u64);
    }

    // Technical properties sit beside the tags rather than inside them: a tag
    // is something the encoder was told, a property is something the stream
    // is, and the inspector reads them apart.
    let audio = &mut m.facts.audio;
    audio.sample_rate = props.sample_rate();
    audio.channels = props.channels();
    audio.bit_depth = props.bit_depth();
    audio.bitrate = props.audio_bitrate();

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

    /// A 4×3 JPEG whose EXIF carries the whole camera set: Make/Model, an Exif
    /// sub-IFD with ISO, shutter, aperture, focal length and DateTimeOriginal,
    /// and a GPS pair in degrees/minutes/seconds. Built once with Pillow; the
    /// point is that it is a **real** container, because every bug this fixture
    /// caught was in reading one.
    const EXIF_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xE1, 0x01, 0x2E, 0x45, 0x78, 0x69, 0x66, 0x00, 0x00,
        0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08, 0x00, 0x04, 0x01, 0x0F, 0x00, 0x02, 0x00,
        0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x3E, 0x01, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, 0x0D,
        0x00, 0x00, 0x00, 0x44, 0x87, 0x69, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x52, 0x88, 0x25, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00,
        0x00, 0x00, 0x43, 0x61, 0x6E, 0x6F, 0x6E, 0x00, 0x43, 0x61, 0x6E, 0x6F, 0x6E, 0x20, 0x45,
        0x4F, 0x53, 0x20, 0x52, 0x35, 0x00, 0x00, 0x00, 0x05, 0x82, 0x9A, 0x00, 0x05, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x94, 0x82, 0x9D, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x9C, 0x88, 0x27, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x64, 0x00, 0x00,
        0x90, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0xA4, 0x92, 0x0A, 0x00,
        0x05, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xB8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x05, 0x32,
        0x30, 0x32, 0x36, 0x3A, 0x30, 0x35, 0x3A, 0x31, 0x32, 0x20, 0x30, 0x39, 0x3A, 0x33, 0x30,
        0x3A, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x00,
        0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x4E, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x05,
        0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0xF6, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00,
        0x02, 0x45, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x05, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x01, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x79, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00, 0x00, 0x01, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xDB, 0x00, 0x43, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC0, 0x00,
        0x11, 0x08, 0x00, 0x03, 0x00, 0x04, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11,
        0x01, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02,
        0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04,
        0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32,
        0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62,
        0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A,
        0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A,
        0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88,
        0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
        0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2,
        0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8,
        0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3,
        0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xC4, 0x00, 0x1F, 0x01, 0x00, 0x03, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x11,
        0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02,
        0x77, 0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
        0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23,
        0x33, 0x52, 0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17,
        0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43,
        0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A,
        0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79,
        0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96,
        0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3,
        0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9,
        0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6,
        0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA,
        0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3F, 0x00, 0x28, 0xA2, 0x8A,
        0x91, 0x9F, 0xFF, 0xD9,
    ];

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    /// The whole EXIF read, end to end, against a real container. This is the
    /// test that was missing: each bug it now pins survived because nothing
    /// ever handed the miner an actual camera file.
    #[test]
    fn a_real_exif_file_mines_the_whole_camera_set() {
        let p = tmp("exif.jpg", EXIF_JPEG);
        let m = mine(&p, AssetKind::Image, &p);
        let photo = &m.facts.photo;
        assert_eq!(photo.make.as_deref(), Some("Canon"));
        assert_eq!(photo.model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(photo.iso, Some(100));
        assert_eq!(photo.aperture_f.as_deref(), Some("f/2.8"));
        assert_eq!(photo.focal_length_mm.as_deref(), Some("35"));
        assert_eq!(photo.exposure_time.as_deref(), Some("1/60s"));
        assert_eq!(
            m.captured_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
            Some("2026-05-12 09:30:00".to_string())
        );
        // 31°23'4" N, 121°28'25" E.
        assert_eq!(photo.gps_lat.unwrap(), 31.384444444444445);
        assert_eq!(photo.gps_lng.unwrap(), 121.47361111111111);
    }

    /// `display_value()` quotes strings — the bug that made every capture date
    /// unreadable and would have put `"Canon"` on screen.
    #[test]
    fn exif_text_strips_the_quotes_display_value_adds() {
        assert_eq!(
            exif_text(&Value::Ascii(vec![b"Canon".to_vec()])).as_deref(),
            Some("Canon")
        );
        assert_eq!(
            exif_text(&Value::Ascii(vec![b"Canon\0".to_vec()])).as_deref(),
            Some("Canon")
        );
        assert_eq!(exif_text(&Value::Ascii(vec![b"  ".to_vec()])), None);
        assert_eq!(exif_text(&Value::Short(vec![1])), None);
    }

    /// A sub-second shutter is a fraction, not a decimal.
    #[test]
    fn exposure_text_reads_as_a_shutter_speed() {
        use exif::Rational;
        let rat = |n, d| Value::Rational(vec![Rational { num: n, denom: d }]);
        assert_eq!(exposure_text(&rat(1, 60)).as_deref(), Some("1/60s"));
        assert_eq!(exposure_text(&rat(1, 8)).as_deref(), Some("1/8s"));
        assert_eq!(exposure_text(&rat(25, 10)).as_deref(), Some("2.5s"));
        assert_eq!(exposure_text(&rat(0, 1)), None);
        assert_eq!(exposure_text(&rat(1, 0)), None);
    }

    #[test]
    fn plain_png_yields_color_but_no_exif() {
        let p = tmp("plain.png", PNG_1X1);
        let m = mine(&p, AssetKind::Image, &p);
        // No EXIF on a hand-crafted PNG…
        assert!(m.captured_at.is_none());
        // …but the universal color facts are always present.
        assert!(m.facts.visual.dominant_color.is_some());
        assert!(m.facts.visual.dominant_colors.is_some());
    }

    #[test]
    fn garbage_audio_and_unknown_kinds_never_panic() {
        let g = tmp("garbage.mp3", b"not really an mp3");
        let m = mine(&g, AssetKind::Audio, &g);
        assert!(m.duration_ms.is_none());
        assert!(m.facts.is_empty());
        for kind in [
            AssetKind::Video,
            AssetKind::Document,
            AssetKind::Archive,
            AssetKind::Font,
            AssetKind::Other,
        ] {
            assert_eq!(mine(&g, kind, &g), MinedMetadata::default());
        }
    }

    #[test]
    fn font_metadata_mines_family_from_a_real_face() {
        // Hermetic when the system has no fonts: the assertions only run on a
        // found face; otherwise the test only proves garbage never panics.
        let face = find_system_font();
        let Some(path) = face else { return };
        let m = mine(&path, AssetKind::Font, &path);
        assert!(
            m.facts
                .font
                .family
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
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
