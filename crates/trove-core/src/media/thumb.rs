//! Thumbnail cache: small JPEG previews generated beside the blobs.
//!
//! Layout mirrors the blob buckets: `thumbs/<sha[:2]>/<sha>.jpg`. A thumbnail
//! is derived purely from content, so it is safe to delete and regenerate.

use std::path::{Path, PathBuf};

use image::GenericImageView;

use crate::model::AssetKind;

/// Longest edge of generated thumbnails, in pixels.
pub const THUMB_MAX: u32 = 512;

/// Relative path of the thumbnail for `sha`, e.g. `thumbs/ab/<sha>.jpg`.
pub fn rel_path(sha: &str) -> String {
    let (a, b) = sha.split_at(2);
    format!("thumbs/{a}/{b}.jpg")
}

/// Absolute path of the thumbnail for `sha` inside a library root.
pub fn abs_path(root: &Path, sha: &str) -> PathBuf {
    root.join(rel_path(sha))
}

/// Ensure a thumbnail exists for the image blob at `blob_path` and return its
/// absolute path, or `None` when the file is not a decodable image.
///
/// Missing or corrupt thumbnails never fail the caller — they are treated as
/// "no thumbnail yet".
pub fn ensure(root: &Path, sha: &str, kind: AssetKind, blob_path: &Path) -> Option<PathBuf> {
    let out = abs_path(root, sha);
    if out.is_file() {
        return Some(out);
    }
    match kind {
        AssetKind::Image => write_thumb(blob_path, &out),
        AssetKind::Video => write_video_thumb(blob_path, &out),
        AssetKind::Font => write_font_card(blob_path, &out),
        _ => None,
    }
}

/// Regenerate a thumbnail unconditionally, overwriting any existing file.
/// Returns the thumbnail path on success, or `None` when the blob is not a
/// decodable image. Used by maintenance to rebuild a corrupt cache entry.
pub fn regenerate(root: &Path, sha: &str, kind: AssetKind, blob_path: &Path) -> Option<PathBuf> {
    let out = abs_path(root, sha);
    match kind {
        AssetKind::Image => write_thumb(blob_path, &out),
        AssetKind::Video => write_video_thumb(blob_path, &out),
        AssetKind::Font => write_font_card(blob_path, &out),
        _ => None,
    }
}

/// Size of the font-specimen card, in pixels (landscape, thumbnail-scale).
const FONT_CARD_SIZE: (u32, u32) = (512, 256);
/// Pixel size used to rasterize the sample text on the card.
const FONT_CARD_PX: f32 = 88.0;

/// Render a "font specimen card" for a font blob: the configured sample text
/// (Settings ▸ General) set in the font itself on a light card. Characters
/// the font does not cover are skipped, so CJK fonts show the CJK sample
/// glyph and Latin-only fonts fall back to "Aa 123". Returns `None` when the
/// bytes are not a parseable TTF/OTF (the asset keeps its icon).
fn write_font_card(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(blob_path).ok()?;
    let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()?;
    let sample = crate::config::AppConfig::load().font_sample_text();

    let (w, h) = FONT_CARD_SIZE;
    let mut card = image::RgbaImage::from_pixel(w, h, image::Rgba([0xF7, 0xF6, 0xF3, 0xFF]));
    let ink = [0x20_u8, 0x21, 0x24];
    let mut pen_x = 28.0_f32;
    let baseline = 152.0_f32;

    for ch in sample.chars() {
        if pen_x + FONT_CARD_PX > w as f32 {
            break;
        }
        if font.lookup_glyph_index(ch) == 0 {
            // Glyph missing from this font; keep word gaps sensible.
            if ch == ' ' {
                pen_x += FONT_CARD_PX * 0.35;
            }
            continue;
        }
        let (metrics, bitmap) = font.rasterize(ch, FONT_CARD_PX);
        if metrics.width == 0 || metrics.height == 0 {
            pen_x += metrics.advance_width;
            continue;
        }
        // fontdue works y-up; the bitmap's top row is the glyph's ymax.
        let left = (pen_x + metrics.xmin as f32).round() as i32;
        let top = (baseline - (metrics.ymin as f32 + metrics.height as f32)).round() as i32;
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let a = bitmap[row * metrics.width + col] as u32;
                if a == 0 {
                    continue;
                }
                let x = left + col as i32;
                let y = top + row as i32;
                if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                    continue;
                }
                let p = card.get_pixel_mut(x as u32, y as u32);
                let blend = |c: u8, ink: u8| ((ink as u32 * a + c as u32 * (255 - a)) / 255) as u8;
                *p = image::Rgba([
                    blend(p[0], ink[0]),
                    blend(p[1], ink[1]),
                    blend(p[2], ink[2]),
                    255,
                ]);
            }
        }
        pen_x += metrics.advance_width;
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let tmp = out.with_extension("tmp.jpg");
    match image::DynamicImage::ImageRgba8(card).save_with_format(&tmp, image::ImageFormat::Jpeg) {
        Ok(()) => {
            std::fs::rename(&tmp, out).ok()?;
            Some(out.to_path_buf())
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            None
        }
    }
}

/// Grab a poster frame from a video with the system `ffmpeg` (opt-in
/// dependency: when it is not on PATH the video simply keeps its icon). The
/// frame is written as JPEG directly by ffmpeg, then moved into place.
fn write_video_thumb(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    let parent = out.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    // Must keep a known extension (ffmpeg picks the muxer from it): the
    // temp file is `<stem>.tmp.jpg`, renamed onto `<stem>.jpg` on success.
    let tmp = out.with_extension("tmp.jpg");
    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-ss", "1", "-i"])
        .arg(blob_path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale='min({THUMB_MAX},iw)':-2"),
        ])
        .arg(&tmp)
        .output()
        .ok()?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::fs::rename(&tmp, out).ok()?;
    Some(out.to_path_buf())
}

/// Decode an image blob: the `image` crate handles raster formats; SVG is
/// rendered via resvg and PSD composites via the psd crate (both store
/// vector/layer data the raster decoder cannot read).
pub fn decode_image(blob_path: &Path) -> Option<image::DynamicImage> {
    let ext = blob_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "svg" => render_svg(blob_path),
        "psd" => render_psd(blob_path),
        "heic" | "heif" => crate::media::probe::heic_to_image(blob_path),
        _ if crate::media::probe::is_raw_ext(&ext) => render_raw(blob_path),
        _ => image::open(blob_path).ok(),
    }
}

/// Rasterize an SVG at its intrinsic size, capped at [`THUMB_MAX`].
fn render_svg(path: &Path) -> Option<image::DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    let mut options = resvg::usvg::Options::default();
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    options.fontdb = std::sync::Arc::new(fontdb);
    let tree = resvg::usvg::Tree::from_data(&bytes, &options).ok()?;
    let size = tree.size();
    let (w, h) = (size.width().ceil() as f32, size.height().ceil() as f32);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (THUMB_MAX as f32 / w.max(h)).min(1.0);
    let (tw, th) = ((w * scale).max(1.0) as u32, (h * scale).max(1.0) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(tw, th)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(tw as f32 / w, th as f32 / h),
        &mut pixmap.as_mut(),
    );
    let png = pixmap.encode_png().ok()?;
    image::load_from_memory(&png).ok()
}

/// Composite a PSD to RGBA (uses the embedded flattened preview, which
/// Photoshop and GIMP write by default; falls back to compositing layers).
fn render_psd(path: &Path) -> Option<image::DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    let psd = psd::Psd::from_bytes(&bytes).ok()?;
    let rgba = if psd.layers().is_empty() {
        psd.rgba()
    } else {
        psd.flatten_layers_rgba(&|_| true).ok()?
    };
    image::RgbaImage::from_raw(psd.width(), psd.height(), rgba).map(image::DynamicImage::ImageRgba8)
}

/// Decode `blob_path` and atomically write its downscaled JPEG to `out`
/// (via a temporary file + rename). Missing/corrupt thumbs never fail the
/// caller — they mean "no thumbnail yet".
fn write_thumb(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    let image = decode_image(blob_path)?;
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let scale = (THUMB_MAX as f32 / w.max(h) as f32).min(1.0);
    let thumb = image.thumbnail(
        (w as f32 * scale).max(1.0) as u32,
        (h as f32 * scale).max(1.0) as u32,
    );

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    // Must keep a known extension (ffmpeg picks the muxer from it): the
    // temp file is `<stem>.tmp.jpg`, renamed onto `<stem>.jpg` on success.
    let tmp = out.with_extension("tmp.jpg");
    match thumb.save_with_format(&tmp, image::ImageFormat::Jpeg) {
        Ok(()) => {
            std::fs::rename(&tmp, out).ok()?;
            Some(out.to_path_buf())
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            None
        }
    }
}

/// Decode and develop a camera-RAW file with rawler: demosaic, white
/// balance, color calibration and sRGB gamma in one pass, then clamp into
/// an 8-bit RGB(A) image (EXIF orientation applied).
fn render_raw(path: &Path) -> Option<image::DynamicImage> {
    let raw = rawler::decode_file(path).ok()?;
    let developed = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(&raw)
        .ok()?;
    let image = match developed {
        rawler::imgop::develop::Intermediate::Monochrome(pix) => {
            let mut gray = image::GrayImage::new(pix.width as u32, pix.height as u32);
            for (x, y, pixel) in gray.enumerate_pixels_mut() {
                let value =
                    (pix.data[y as usize * pix.width + x as usize] * 255.0).clamp(0.0, 255.0) as u8;
                *pixel = image::Luma([value]);
            }
            image::DynamicImage::ImageLuma8(gray)
        }
        rawler::imgop::develop::Intermediate::ThreeColor(pix) => {
            let mut rgb = image::RgbImage::new(pix.width as u32, pix.height as u32);
            for (x, y, pixel) in rgb.enumerate_pixels_mut() {
                let sample = pix.data[y as usize * pix.width + x as usize];
                *pixel = image::Rgb([
                    (sample[0] * 255.0).clamp(0.0, 255.0) as u8,
                    (sample[1] * 255.0).clamp(0.0, 255.0) as u8,
                    (sample[2] * 255.0).clamp(0.0, 255.0) as u8,
                ]);
            }
            image::DynamicImage::ImageRgb8(rgb)
        }
        rawler::imgop::develop::Intermediate::FourColor(pix) => {
            let mut rgb = image::RgbImage::new(pix.width as u32, pix.height as u32);
            for (x, y, pixel) in rgb.enumerate_pixels_mut() {
                let sample = pix.data[y as usize * pix.width + x as usize];
                *pixel = image::Rgb([
                    (sample[0] * 255.0).clamp(0.0, 255.0) as u8,
                    (sample[1] * 255.0).clamp(0.0, 255.0) as u8,
                    (sample[2] * 255.0).clamp(0.0, 255.0) as u8,
                ]);
            }
            image::DynamicImage::ImageRgb8(rgb)
        }
    };
    Some(apply_orientation(image, raw.orientation))
}

/// Apply the EXIF orientation a RAW decoder reports.
fn apply_orientation(
    image: image::DynamicImage,
    orientation: rawler::decoders::Orientation,
) -> image::DynamicImage {
    use rawler::decoders::Orientation as O;
    match orientation {
        O::Normal | O::Unknown => image,
        O::HorizontalFlip => image.fliph(),
        O::Rotate180 => image.rotate180(),
        O::VerticalFlip => image.flipv(),
        O::Transpose => image.rotate90().fliph(),
        O::Rotate90 => image.rotate90(),
        O::Transverse => image.rotate270().fliph(),
        O::Rotate270 => image.rotate270(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// End-to-end poster-frame extraction, skipped when the optional ffmpeg
    /// dependency is not installed.
    #[test]
    fn video_thumb_extracted_when_ffmpeg_present() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-thumb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=64x48:rate=1:duration=2",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());

        let out = ensure(&dir, "a".repeat(64).as_str(), AssetKind::Video, &video);
        assert!(out.is_some_and(|p| p.is_file()));
    }

    /// Font-card generation against a system font, skipped when none can be
    /// found (keep the suite independent of installed fonts).
    #[test]
    fn font_card_generated_from_system_font() {
        let font_path = ["usr/share/fonts"]
            .iter()
            .flat_map(|d| walkdir_candidates(std::path::Path::new(d)))
            .find(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("ttf") | Some("otf")
                )
            });
        let Some(font_path) = font_path else {
            eprintln!("skipping: no system ttf/otf font found");
            return;
        };

        let dir = std::env::temp_dir().join(format!("trove-fontcard-{}", uuid::Uuid::new_v4()));
        let out = ensure(&dir, "b".repeat(64).as_str(), AssetKind::Font, &font_path);
        assert!(out.as_ref().is_some_and(|p| p.is_file()));
        // The card decodes back and has the expected card dimensions.
        let img = image::open(out.expect("card")).unwrap();
        let (w, _) = img.dimensions();
        assert_eq!(w, FONT_CARD_SIZE.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    fn walkdir_candidates(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walkdir_candidates(&p));
            } else {
                out.push(p);
            }
        }
        out
    }
}
