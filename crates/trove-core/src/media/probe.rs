//! Media type probing: extension -> mime/kind mapping and image dimensions.

use crate::model::AssetKind;

/// Classification + mime for one imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub kind: AssetKind,
    pub mime: String,
}

/// The dimension of a raster image, present when decodable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

/// Lower-case the extension and strip a leading dot.
pub fn normalize_ext(raw: &str) -> String {
    let ext = raw.trim().trim_start_matches('.').to_lowercase();
    ext.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

/// Camera-RAW extensions decoded via `rawler`. `raw` is last so it cannot
/// shadow a more specific match above.
pub fn is_raw_ext(ext: &str) -> bool {
    matches!(
        ext,
        "cr2"
            | "cr3"
            | "nef"
            | "arw"
            | "dng"
            | "raf"
            | "orf"
            | "rw2"
            | "raw"
            | "srw"
            | "pef"
            | "x3f"
            | "3fr"
            | "erf"
            | "kdc"
            | "dcr"
            | "mrw"
    )
}

/// Classify a file from its normalized extension.
pub fn probe(ext: &str) -> Probe {
    let kind = match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif" | "avif"
        | "heic" | "heif" | "svg" | "psd" => AssetKind::Image,
        ext if is_raw_ext(ext) => AssetKind::Image,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mpg" | "mpeg" | "wmv" => AssetKind::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => AssetKind::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt"
        | "ods" | "odp" | "csv" => AssetKind::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => AssetKind::Archive,
        "ttf" | "otf" | "ttc" | "woff" | "woff2" => AssetKind::Font,
        // Only the formats the mesh parser actually reads; other 3D extensions
        // stay `Other` rather than promising a preview we cannot render.
        ext if crate::media::mesh::is_model_ext(ext) => AssetKind::Model,
        _ => AssetKind::Other,
    };
    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "avif" => "image/avif",
        "heic" | "heif" => "image/heic",
        "svg" => "image/svg+xml",
        "psd" => "image/vnd.adobe.photoshop",
        "cr2" => "image/x-canon-cr2",
        "cr3" => "image/x-canon-cr3",
        "nef" => "image/x-nikon-nef",
        "arw" => "image/x-sony-arw",
        "dng" => "image/x-adobe-dng",
        "raf" => "image/x-fuji-raf",
        "orf" => "image/x-olympus-orf",
        "rw2" => "image/x-panasonic-rw2",
        "pef" => "image/x-pentax-pef",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "m4v" => "video/x-m4v",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" | "md" | "csv" => "text/plain",
        "rtf" => "application/rtf",
        "odt" => "application/vnd.oasis.opendocument.text",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "ttc" => "font/collection",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "obj" => "model/obj",
        "stl" => "model/stl",
        "ply" => "model/ply",
        _ => "application/octet-stream",
    }
    .to_string();
    Probe { kind, mime }
}

/// Container-level facts of an MP4-family file (mp4 / m4v / m4a / mov),
/// read from the moov box: track dimensions plus duration. `None` when the
/// container cannot be parsed (e.g. mkv/webm/avi) — never fails the caller.
pub struct VideoFacts {
    pub width: u32,
    pub height: u32,
    pub duration_ms: Option<u64>,
}

pub fn video_facts(path: &std::path::Path) -> Option<VideoFacts> {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), len).ok()?;

    let mut width = 0u32;
    let mut height = 0u32;
    for track in reader.tracks().values() {
        if matches!(track.track_type(), Ok(mp4::TrackType::Video)) {
            width = track.width() as u32;
            height = track.height() as u32;
            break;
        }
    }
    if width == 0 || height == 0 {
        return None;
    }

    // Container duration over its timescale → milliseconds.
    let timescale = reader.timescale() as u64;
    let duration_ms = (timescale > 0).then(|| reader.duration().as_millis() as u64);

    Some(VideoFacts {
        width,
        height,
        duration_ms,
    })
}

/// Read the pixel dimensions of a raster image by decoding only its header.
///
/// Unsupported or corrupt images return `None` — never fail the import.
pub fn image_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    // Vector/layer formats carry their intrinsic size in structured headers.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "svg" => return svg_dimensions(path),
        "psd" => return psd_dimensions(path),
        "heic" | "heif" => return heic_dimensions(path),
        _ if is_raw_ext(&ext) => return raw_dimensions(path),
        _ => {}
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let format = image::ImageReader::new(&mut reader)
        .with_guessed_format()
        .ok()?;
    // Format-level probing still performs a bounded read; unknown formats
    // would return an error here.
    let dims = format.into_dimensions().ok()?;
    Some(Dimensions {
        width: dims.0,
        height: dims.1,
    })
}

/// Intrinsic size of an SVG (from its root `<svg width/height/viewBox>`).
fn svg_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let bytes = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    Some(Dimensions {
        width: size.width().ceil() as u32,
        height: size.height().ceil() as u32,
    })
}

/// Size of a PSD's composite canvas.
fn psd_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let bytes = std::fs::read(path).ok()?;
    let psd = psd::Psd::from_bytes(&bytes).ok()?;
    Some(Dimensions {
        width: psd.width(),
        height: psd.height(),
    })
}

/// Sensor dimensions of a camera-RAW file (full decode; the thumbnail
/// pipeline decodes again — RAW imports are background work, so this stays
/// simple at the cost of some CPU).
fn raw_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let raw = rawler::decode_file(path).ok()?;
    Some(Dimensions {
        width: raw.width.max(1) as u32,
        height: raw.height.max(1) as u32,
    })
}

/// HEIC dimensions need a real decoder; shell out to libheif's `heif-dec`
/// when present (same opt-in pattern as ffmpeg for video posters) and read
/// the converted image. `None` when the tool is missing or fails.
fn heic_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let converted = heic_to_image(path)?;
    Some(Dimensions {
        width: converted.width(),
        height: converted.height(),
    })
}

/// Convert a HEIC/HEIF blob to a raster image with the system `heif-dec`.
pub(crate) fn heic_to_image(path: &std::path::Path) -> Option<image::DynamicImage> {
    let tmp = std::env::temp_dir().join(format!(
        "trove-heic-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ));
    let output = std::process::Command::new("heif-dec")
        .arg(path)
        .arg(&tmp)
        .output()
        .ok()?;
    let result = if output.status.success() {
        image::open(&tmp).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_extensions_map_to_font_kind() {
        for ext in ["ttf", "otf", "ttc", "woff", "woff2", "TTF"] {
            let p = probe(&normalize_ext(ext));
            assert_eq!(p.kind, AssetKind::Font, "{ext}");
            assert!(p.mime.starts_with("font/"), "{ext} mime {}", p.mime);
        }
    }

    #[test]
    fn model_extensions_map_to_model_kind() {
        for ext in ["obj", "stl", "ply", "OBJ", "Stl"] {
            let p = probe(&normalize_ext(ext));
            assert_eq!(p.kind, AssetKind::Model, "{ext}");
            assert!(p.mime.starts_with("model/"), "{ext} mime {}", p.mime);
        }
        // Formats we cannot parse stay unclassified rather than half-supported.
        assert_eq!(probe("fbx").kind, AssetKind::Other);
        assert_eq!(probe("gltf").kind, AssetKind::Other);
    }

    #[test]
    fn video_facts_reject_garbage() {
        let dir = std::env::temp_dir().join(format!("trove-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.mp4");
        std::fs::write(&path, b"not an mp4").unwrap();
        assert!(video_facts(&path).is_none());
    }
}
