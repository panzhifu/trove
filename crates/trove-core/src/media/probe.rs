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
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif"
        | "avif" | "heic" | "heif" | "svg" => AssetKind::Image,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mpg" | "mpeg" | "wmv" => {
            AssetKind::Video
        }
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => AssetKind::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf"
        | "odt" | "ods" | "odp" | "csv" => AssetKind::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => AssetKind::Archive,
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
        _ => "application/octet-stream",
    }
    .to_string();
    Probe { kind, mime }
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
