//! Batch format conversion: decode any image asset the library can display
//! (raster, SVG, PSD, RAW, HEIC) and re-encode it as a plain raster file in
//! a user-chosen folder. Quality applies to JPEG only; the image crate's
//! WebP encoder is lossless.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use image::{GenericImageView, ImageDecoder};
use uuid::Uuid;

use crate::media::{probe, thumb};

/// The raster formats offered by batch conversion. All encoders ship with
/// the `image` crate features already enabled in this workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertFormat {
    Jpeg,
    Png,
    WebP,
    Bmp,
    Tiff,
}

/// The conversion formats, in dialog order.
pub const CONVERT_FORMATS: [ConvertFormat; 5] = [
    ConvertFormat::Jpeg,
    ConvertFormat::Png,
    ConvertFormat::WebP,
    ConvertFormat::Bmp,
    ConvertFormat::Tiff,
];

impl ConvertFormat {
    /// Lowercase file extension (without the dot).
    pub fn ext(self) -> &'static str {
        match self {
            ConvertFormat::Jpeg => "jpg",
            ConvertFormat::Png => "png",
            ConvertFormat::WebP => "webp",
            ConvertFormat::Bmp => "bmp",
            ConvertFormat::Tiff => "tiff",
        }
    }

    fn image_format(self) -> image::ImageFormat {
        match self {
            ConvertFormat::Jpeg => image::ImageFormat::Jpeg,
            ConvertFormat::Png => image::ImageFormat::Png,
            ConvertFormat::WebP => image::ImageFormat::WebP,
            ConvertFormat::Bmp => image::ImageFormat::Bmp,
            ConvertFormat::Tiff => image::ImageFormat::Tiff,
        }
    }

    /// Parse the stable id used by the dialog.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jpeg" => Some(ConvertFormat::Jpeg),
            "png" => Some(ConvertFormat::Png),
            "webp" => Some(ConvertFormat::WebP),
            "bmp" => Some(ConvertFormat::Bmp),
            "tiff" => Some(ConvertFormat::Tiff),
            _ => None,
        }
    }

    /// Stable id (inverse of [`ConvertFormat::parse`]).
    pub fn id(self) -> &'static str {
        match self {
            ConvertFormat::Jpeg => "jpeg",
            ConvertFormat::Png => "png",
            ConvertFormat::WebP => "webp",
            ConvertFormat::Bmp => "bmp",
            ConvertFormat::Tiff => "tiff",
        }
    }

    /// Whether the quality slider applies (JPEG only; WebP is lossless here).
    pub fn supports_quality(self) -> bool {
        matches!(self, ConvertFormat::Jpeg)
    }
}

/// User options for one conversion batch.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub format: ConvertFormat,
    /// JPEG quality, 1–100.
    pub quality: u8,
    /// Optional cap on the longest edge. Images are never enlarged.
    pub max_dimension: Option<u32>,
}

/// One planned conversion. Built on the main thread (resolved source paths)
/// and executed on the background executor, hence plain `Send` data.
pub struct ConvertItem {
    pub asset_id: Uuid,
    /// Absolute source path: the library blob or a linked original.
    pub source: PathBuf,
    /// Base name for the output file (without extension).
    pub title: String,
}

/// The outcome of one item in a batch.
pub struct ConvertOutcome {
    pub item: ConvertItem,
    /// Path of the written file, or a human-readable failure reason.
    pub result: Result<PathBuf, String>,
}

/// Run the whole plan sequentially, writing every output into `dest`.
/// Failures are reported per item; the batch always runs to completion.
pub fn run_convert_plan(
    dest: &Path,
    items: Vec<ConvertItem>,
    opts: &ConvertOptions,
) -> Vec<ConvertOutcome> {
    let mut used: HashSet<String> = HashSet::new();
    let mut outcomes = Vec::with_capacity(items.len());
    for item in items {
        let result = convert_item(dest, &item, opts, &mut used);
        outcomes.push(ConvertOutcome { item, result });
    }
    outcomes
}

/// Convert one item, resolving a unique output name via `used`. Exposed for
/// the app's per-item background execution with live progress.
pub fn convert_item(
    dest: &Path,
    item: &ConvertItem,
    opts: &ConvertOptions,
    used: &mut HashSet<String>,
) -> Result<PathBuf, String> {
    let base = sanitize_title(&item.title);
    let out = unique_path(dest, &base, opts.format.ext(), used);
    convert_into(&item.source, &out, opts)
}

/// Convert a single file to `out`. The write is atomic (temp file + rename).
fn convert_into(source: &Path, out: &Path, opts: &ConvertOptions) -> Result<PathBuf, String> {
    let mut image = decode_oriented(source)?;
    if let Some(max) = opts.max_dimension {
        image = downscale(image, max);
    }
    let image = match opts.format {
        // JPEG has no alpha channel; blend onto white instead of losing it.
        ConvertFormat::Jpeg => flatten_alpha(image),
        _ => image,
    };

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let ext = opts.format.ext();
    let tmp = out.with_extension(format!("tmp.{ext}"));
    let write = || -> Result<(), String> {
        if opts.format.supports_quality() {
            let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                file,
                opts.quality.clamp(1, 100),
            );
            encoder
                .encode_image(&image)
                .map_err(|e| format!("encode: {e}"))?;
        } else {
            image
                .save_with_format(&tmp, opts.format.image_format())
                .map_err(|e| format!("encode: {e}"))?;
        }
        Ok(())
    };
    match write() {
        Ok(()) => {
            std::fs::rename(&tmp, out).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })?;
            Ok(out.to_path_buf())
        }
        Err(reason) => {
            let _ = std::fs::remove_file(&tmp);
            Err(reason)
        }
    }
}

/// Decode an image with EXIF orientation applied. Vector/composite formats
/// (SVG, PSD) and RAW (already oriented by the develop pipeline) go through
/// the thumbnail decoder.
fn decode_oriented(path: &Path) -> Result<image::DynamicImage, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let special =
        matches!(ext.as_str(), "svg" | "psd" | "heic" | "heif") || probe::is_raw_ext(&ext);
    if special {
        return thumb::decode_image(path).ok_or_else(|| "not a decodable image".into());
    }
    let reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image =
        image::DynamicImage::from_decoder(decoder).map_err(|e| format!("decode: {e}"))?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Cap the longest edge at `max`, preserving the aspect ratio. Never
/// enlarges.
fn downscale(image: image::DynamicImage, max: u32) -> image::DynamicImage {
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return image;
    }
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    image.thumbnail(
        (w as f32 * scale).max(1.0) as u32,
        (h as f32 * scale).max(1.0) as u32,
    )
}

/// Blend an image's alpha channel onto white, producing RGB.
fn flatten_alpha(image: image::DynamicImage) -> image::DynamicImage {
    use image::DynamicImage as D;
    if !image.color().has_alpha() {
        return image;
    }
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut rgb = image::RgbImage::new(w, h);
    for (x, y, pixel) in rgb.enumerate_pixels_mut() {
        let p = rgba.get_pixel(x, y);
        let a = p[3] as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        *pixel = image::Rgb([blend(p[0]), blend(p[1]), blend(p[2])]);
    }
    D::ImageRgb8(rgb)
}

/// Turn an asset title into a safe file base name: keep letters (any
/// script), digits and common punctuation; replace everything else with a
/// space; trim and cap the length.
pub fn sanitize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.' | '(' | ')' | '[' | ']') {
            out.push(ch);
        } else {
            out.push(' ');
        }
    }
    let trimmed = out.trim();
    let base: String = trimmed.chars().take(100).collect();
    if base.is_empty() {
        "image".into()
    } else {
        base
    }
}

/// First available `base.ext` in `dest`, appending ` (2)`, ` (3)`, … when
/// the name is taken by the folder or by an earlier item of the same batch.
fn unique_path(dest: &Path, base: &str, ext: &str, used: &mut HashSet<String>) -> PathBuf {
    let mut candidate = format!("{base}.{ext}");
    let mut n = 2;
    while used.contains(&candidate) || dest.join(&candidate).exists() {
        candidate = format!("{base} ({n}).{ext}");
        n += 1;
    }
    used.insert(candidate.clone());
    dest.join(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-convert-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_sample_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, _| image::Rgb([(x % 256) as u8, 0x80, 0x40]));
        img.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    fn item(title: &str, source: &Path) -> ConvertItem {
        ConvertItem {
            asset_id: Uuid::new_v4(),
            source: source.to_path_buf(),
            title: title.into(),
        }
    }

    fn opts(format: ConvertFormat, max_dimension: Option<u32>) -> ConvertOptions {
        ConvertOptions {
            format,
            quality: 85,
            max_dimension,
        }
    }

    #[test]
    fn converts_png_to_jpeg() {
        let dir = temp_dir("jpg");
        let src = dir.join("photo.png");
        write_sample_png(&src, 64, 32);

        let outcomes = run_convert_plan(
            &dir,
            vec![item("Holiday", &src)],
            &opts(ConvertFormat::Jpeg, None),
        );
        let out = outcomes[0].result.as_ref().expect("conversion succeeds");
        assert_eq!(out.file_name().unwrap(), "Holiday.jpg");

        let decoded = image::ImageReader::open(out)
            .unwrap()
            .with_guessed_format()
            .unwrap();
        assert_eq!(decoded.format(), Some(image::ImageFormat::Jpeg));
        assert_eq!(decoded.into_dimensions().unwrap(), (64, 32));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_dimension_downscales_but_never_enlarges() {
        let dir = temp_dir("scale");
        let src = dir.join("big.png");
        write_sample_png(&src, 100, 50);

        let outcomes = run_convert_plan(
            &dir,
            vec![item("small", &src)],
            &opts(ConvertFormat::Png, Some(40)),
        );
        let out = outcomes[0].result.as_ref().unwrap();
        assert_eq!(
            image::ImageReader::open(out)
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (40, 20)
        );

        // A cap above the original size keeps the original dimensions.
        let outcomes = run_convert_plan(
            &dir,
            vec![item("kept", &src)],
            &opts(ConvertFormat::Png, Some(200)),
        );
        let out = outcomes[0].result.as_ref().unwrap();
        assert_eq!(
            image::ImageReader::open(out)
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (100, 50)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn name_collisions_get_suffixes() {
        let dir = temp_dir("collide");
        let src = dir.join("a.png");
        write_sample_png(&src, 8, 8);

        // An existing file in the destination folder collides too.
        std::fs::write(dir.join("same.jpg"), b"existing").unwrap();
        let outcomes = run_convert_plan(
            &dir,
            vec![item("same", &src), item("same", &src)],
            &opts(ConvertFormat::Jpeg, None),
        );
        let names: Vec<String> = outcomes
            .iter()
            .map(|o| {
                o.result
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["same (2).jpg".to_string(), "same (3).jpg".to_string()]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_source_is_reported_and_batch_continues() {
        let dir = temp_dir("corrupt");
        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"definitely not a png").unwrap();
        let good = dir.join("good.png");
        write_sample_png(&good, 8, 8);

        let outcomes = run_convert_plan(
            &dir,
            vec![item("bad", &bad), item("good", &good)],
            &opts(ConvertFormat::Png, None),
        );
        assert!(outcomes[0].result.is_err());
        assert!(outcomes[1].result.is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitize_title_removes_path_characters() {
        assert_eq!(sanitize_title("a/b\\c:d"), "a b c d");
        assert_eq!(sanitize_title("照片 2026 (1)"), "照片 2026 (1)");
        assert_eq!(sanitize_title("///"), "image");
    }

    #[test]
    fn format_parse_roundtrip() {
        for format in CONVERT_FORMATS {
            assert_eq!(ConvertFormat::parse(format.id()), Some(format));
        }
        assert_eq!(ConvertFormat::parse("nope"), None);
    }
}
