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

/// Classify a file from its normalized extension.
pub fn probe(ext: &str) -> Probe {
    let kind = match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif" | "avif"
        | "heic" | "heif" | "svg" => AssetKind::Image,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mpg" | "mpeg" | "wmv" => AssetKind::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => AssetKind::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt"
        | "ods" | "odp" | "csv" => AssetKind::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => AssetKind::Archive,
        "ttf" | "otf" | "ttc" | "woff" | "woff2" => AssetKind::Font,
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
    fn video_facts_reject_garbage() {
        let dir = std::env::temp_dir().join(format!("trove-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.mp4");
        std::fs::write(&path, b"not an mp4").unwrap();
        assert!(video_facts(&path).is_none());
    }
}
