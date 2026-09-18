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

/// Whether this extension is a video container Trove can show: a poster frame
/// from `ffmpeg`, playback through its pipe, and facts from the mp4 reader or
/// `ffprobe`.
///
/// One list, because two of them drifted: the app's "open with" gate carried
/// `flv` and `ts` while this classifier did not, so those files imported as
/// `Other` — no poster frame, no duration — even though the same `ffmpeg` that
/// plays them could have made one.
pub fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mpg" | "mpeg" | "wmv" | "flv" | "ts"
    )
}

/// Classify a file from its normalized extension.
pub fn probe(ext: &str) -> Probe {
    let kind = match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif" | "avif"
        | "jxl" | "heic" | "heif" | "svg" | "psd" => AssetKind::Image,
        ext if is_raw_ext(ext) => AssetKind::Image,
        ext if is_video_ext(ext) => AssetKind::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" => AssetKind::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt"
        | "ods" | "odp" | "csv" => AssetKind::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => AssetKind::Archive,
        "ttf" | "otf" | "ttc" | "woff" | "woff2" => AssetKind::Font,
        // Only the formats the mesh parser actually reads; other 3D extensions
        // stay `Other` rather than promising a preview we cannot render.
        ext if crate::media::formats::is_model_ext(ext) => AssetKind::Model,
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
        "jxl" => "image/jxl",
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
        "wmv" => "video/x-ms-wmv",
        "mpg" | "mpeg" => "video/mpeg",
        "flv" => "video/x-flv",
        "ts" => "video/mp2t",
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
        "gltf" => "model/gltf+json",
        "glb" => "model/gltf-binary",
        // Not a `model/*` type: a `.blend` is Blender's own document, and the
        // media type the Blender packages register says so. It is still a
        // model here because this application previews one.
        "blend" => "application/x-blender",
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

/// Whether [`image_dimensions`] costs a full pixel decode for this extension
/// rather than a header read.
///
/// Camera RAW goes through `rawler` (demosaic + develop), and HEIF/AVIF shell
/// out to `heif-dec` and read the PNG it writes — both are the whole picture,
/// not a header. The import pipeline asks this so it can skip the read: its
/// decode stage is about to produce exactly those pixels.
pub fn dimensions_need_full_decode(ext: &str) -> bool {
    is_raw_ext(ext) || matches!(ext, "heif" | "heic" | "avif")
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
        // AVIF is a HEIF container with an AV1 payload, so libheif reads it
        // whenever it was built with AV1 support; JPEG-XL needs its own crate.
        "avif" | "heic" | "heif" => return heif_dimensions(path),
        "jxl" => return jxl_dimensions(path),
        _ if is_raw_ext(&ext) => return raw_dimensions(path),
        _ => {}
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let format = image::ImageReader::new(&mut reader)
        .with_guessed_format()
        .ok()?;
    // The orientation ride-along costs the same bounded header read that
    // dimensions does: 90-degree orientations swap width and height, and a
    // portrait phone photo stored unswapped would lie on its side in every
    // layout that trusts these columns.
    use image::ImageDecoder as _;
    let mut decoder = format.into_decoder().ok()?;
    let dims = decoder.dimensions();
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let (width, height) = match orientation {
        image::metadata::Orientation::Rotate90
        | image::metadata::Orientation::Rotate270
        | image::metadata::Orientation::Rotate90FlipH
        | image::metadata::Orientation::Rotate270FlipH => (dims.1, dims.0),
        _ => dims,
    };
    Some(Dimensions { width, height })
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

/// Size of a HEIC/HEIF/AVIF file: shell out to libheif's `heif-dec` when
/// present (same opt-in pattern as ffmpeg for video posters). `None` when the
/// tool is missing, or when libheif lacks the codec the file needs.
fn heif_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let converted = heif_to_image(path)?;
    Some(Dimensions {
        width: converted.width(),
        height: converted.height(),
    })
}

/// Size of a JPEG-XL file, read straight from its header (no pixel decode).
fn jxl_dimensions(path: &std::path::Path) -> Option<Dimensions> {
    let image = jxl_oxide::JxlImage::open_with_defaults(path).ok()?;
    Some(Dimensions {
        width: image.width(),
        height: image.height(),
    })
}

/// Convert a HEIC/HEIF/AVIF blob to a raster image with the system `heif-dec`.
///
/// AVIF rides along for free: it is the same ISOBMFF container with an AV1
/// payload, so any libheif built with an AV1 decoder handles it. When libheif
/// lacks that codec the command fails and we degrade to "no thumbnail".
pub(crate) fn heif_to_image(path: &std::path::Path) -> Option<image::DynamicImage> {
    let tmp = std::env::temp_dir().join(format!(
        "trove-heif-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ));
    // A decode worth of work in a subprocess, so it takes a slot: a batch of
    // HEICs on a wide staging pool would otherwise start one `heif-dec` per
    // thread (see [`super::proc`]).
    let _slot = super::proc::slot();
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
    fn avif_and_jxl_are_first_class_images() {
        for (ext, mime) in [("avif", "image/avif"), ("jxl", "image/jxl")] {
            let p = probe(&normalize_ext(ext));
            assert_eq!(p.kind, AssetKind::Image, "{ext}");
            assert_eq!(p.mime, mime, "{ext}");
        }
    }

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
        for ext in ["obj", "stl", "ply", "gltf", "glb", "OBJ", "Stl"] {
            let p = probe(&normalize_ext(ext));
            assert_eq!(p.kind, AssetKind::Model, "{ext}");
            assert!(p.mime.starts_with("model/"), "{ext} mime {}", p.mime);
        }
        // `.blend` previews like a model but is not a mesh format, and its
        // media type is the one Blender's own desktop entry registers.
        let blend = probe(&normalize_ext("blend"));
        assert_eq!(blend.kind, AssetKind::Model);
        assert_eq!(blend.mime, "application/x-blender");
        // Formats we cannot parse stay unclassified rather than half-supported.
        assert_eq!(probe("fbx").kind, AssetKind::Other);
        assert_eq!(probe("dae").kind, AssetKind::Other);
    }

    #[test]
    fn video_facts_reject_garbage() {
        let dir = std::env::temp_dir().join(format!("trove-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.mp4");
        std::fs::write(&path, b"not an mp4").unwrap();
        assert!(video_facts(&path).is_none());
    }

    /// One video list, and every member has a kind and a media type — the
    /// `flv`/`ts` pair used to sit in the app's "open with" gate only, so those
    /// files imported as `Other` with an octet-stream mime.
    #[test]
    fn the_video_list_is_what_the_library_classifies() {
        for ext in [
            "mp4", "mov", "mkv", "webm", "avi", "m4v", "mpg", "mpeg", "wmv", "flv", "ts",
        ] {
            assert!(is_video_ext(ext), "{ext} is missing from the video list");
            assert_eq!(probe(ext).kind, AssetKind::Video, "{ext}");
            assert_ne!(
                probe(ext).mime,
                "application/octet-stream",
                "{ext} has no media type"
            );
        }
        for ext in ["png", "jpg", "mp3", "pdf", "zip", "ttf", "obj", "mystery"] {
            assert!(!is_video_ext(ext), "{ext} is not a video");
        }
    }
}
