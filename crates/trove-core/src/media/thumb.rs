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
        AssetKind::Model => write_model_card(blob_path, &out),
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
        AssetKind::Model => write_model_card(blob_path, &out),
        _ => None,
    }
}

/// How a model thumbnail gets its geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardSource {
    /// Parse the file: it is small enough that one pass is nothing.
    Whole,
    /// Parse it through the chunked loader, which bounds the memory it needs
    /// but still reads the file once.
    Chunked,
    /// Sample a bounded number of points off disk: too large to parse at all
    /// on an import.
    Sampled,
}

/// Below this, a model is parsed whole for its card.
const CARD_WHOLE_MAX: u64 = 64 << 20;
/// Above this, it is only sampled.
const CARD_CHUNKED_MAX: u64 = 512 << 20;
/// Points sampled for a card of a file too large to parse.
const CARD_SAMPLES: usize = 32_768;

/// Which way a card gets the file's geometry.
///
/// The chunked loader only reads PLY, so a large OBJ or GLB goes down the
/// whole-file path (which caps at 2 GiB) rather than being handed a format it
/// cannot parse and coming back with no card at all.
fn card_source(path: &Path, size: u64) -> CardSource {
    let is_ply = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ply"));
    if !is_ply {
        return CardSource::Whole;
    }
    if size <= CARD_WHOLE_MAX {
        CardSource::Whole
    } else if size <= CARD_CHUNKED_MAX {
        CardSource::Chunked
    } else {
        CardSource::Sampled
    }
}

/// Size of the font-specimen card, in pixels (landscape, thumbnail-scale).
const FONT_CARD_SIZE: (u32, u32) = (512, 256);

/// Horizontal margin kept clear on both sides of the card.
const FONT_CARD_MARGIN: f32 = 28.0;
/// Pixel size for the specimen rows, by how many rows the card shows: one
/// row gets the card to itself and renders larger than three sharing it.
const FONT_CARD_PX: [f32; 4] = [0.0, 96.0, 72.0, 64.0];

/// Which specimen row a character belongs to. The card stacks the three
/// scripts the way a type founder's specimen does: Latin on top, CJK in the
/// middle, digits at the bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptRow {
    Latin,
    Cjk,
    Digit,
}

/// CJK ideographs, kana, hangul and the fullwidth forms that travel with
/// them — everything the middle row is allowed to show.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x9FFF // radicals, CJK symbols, kana, ideographs
            | 0xAC00..=0xD7AF // hangul syllables
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF00..=0xFFEF // fullwidth forms
            | 0x1100..=0x11FF // hangul jamo
            | 0x20000..=0x2FA1F // ideograph extensions B..F
            | 0x30000..=0x323AF // ideograph extensions G..H
    )
}

fn script_row(ch: char) -> Option<ScriptRow> {
    if ch.is_ascii_digit() {
        Some(ScriptRow::Digit)
    } else if ch.is_ascii_alphabetic() {
        Some(ScriptRow::Latin)
    } else if is_cjk(ch) {
        Some(ScriptRow::Cjk)
    } else {
        None
    }
}

/// The built-in specimen text — Latin, CJK, digits, the three rows the
/// specimen card stacks. It used to be configurable in Settings; it is now
/// fixed, so the card never depends on config file I/O.
pub const DEFAULT_FONT_SAMPLE: &str = "AaBbGg 永 0123456789";

/// Built-in lines filling a category the sample text never mentions.
const SPECIMEN_FALLBACKS: [&str; 3] = ["AaBbGg", "永", "0123456789"];

/// The built-in specimen rows: what [`specimen_rows`] reduces
/// [`DEFAULT_FONT_SAMPLE`] to. This is the card's content everywhere the
/// user's sample text used to be configurable.
pub fn default_specimen_rows() -> [String; 3] {
    std::array::from_fn(|i| SPECIMEN_FALLBACKS[i].to_string())
}

/// Split a specimen text into the three card rows — Latin, CJK, digits —
/// keeping first occurrences in order and dropping everything else (spaces,
/// punctuation, other scripts). A category the sample does not cover falls
/// back to its built-in line, so a fresh specimen always reads Latin / CJK /
/// digits from top to bottom.
///
/// Shared by the rasterized font card and the app's live specimen cells, so
/// both paths show the same three rows.
pub fn specimen_rows(sample: &str) -> [String; 3] {
    let mut picked: [Vec<char>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut seen = [
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    ];
    for ch in sample.chars() {
        if let Some(row) = script_row(ch) {
            let i = row as usize;
            if seen[i].insert(ch) {
                picked[i].push(ch);
            }
        }
    }
    let mut rows: [String; 3] = Default::default();
    for (i, row) in rows.iter_mut().enumerate() {
        *row = if picked[i].is_empty() {
            SPECIMEN_FALLBACKS[i].to_string()
        } else {
            picked[i].iter().collect()
        };
    }
    rows
}

/// Render a "font specimen card" for a font blob: three rows stacked like a
/// type founder's specimen — Latin letters on top, CJK in the middle, digits
/// at the bottom — taken from the built-in sample text and set in the font
/// itself on a light card. Characters the font does not cover are dropped,
/// and a row left empty by that disappears so the remaining rows re-centre.
/// Returns `None` when the bytes are not a parseable TTF/OTF (the asset
/// keeps its icon).
fn write_font_card(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(blob_path).ok()?;
    let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()?;

    let (w, h) = FONT_CARD_SIZE;
    let mut card = image::RgbaImage::from_pixel(w, h, image::Rgba([0xF7, 0xF6, 0xF3, 0xFF]));
    let ink = [0x20_u8, 0x21, 0x24];
    let lines = default_specimen_rows();

    // Rasterize every usable glyph up front: a row is dropped entirely when
    // the font covers none of its characters, so the visible rows can share
    // the card height evenly instead of leaving an empty band behind. Rows
    // are collected at the smallest per-row size first — how many survive
    // decides the final pixel size, and the survivors are re-rasterized at
    // it (a cheap second pass over a handful of glyphs).
    let collect = |px: f32| -> Vec<Vec<(fontdue::Metrics, Vec<u8>)>> {
        let mut rows = Vec::new();
        for line in &lines {
            let mut glyphs = Vec::new();
            let mut pen_x = 0.0_f32;
            for ch in line.chars() {
                if font.lookup_glyph_index(ch) == 0 {
                    continue;
                }
                // Overflowing rows are truncated by advance width, keeping
                // the horizontal margins clear on both sides.
                if pen_x + px > w as f32 - 2.0 * FONT_CARD_MARGIN {
                    break;
                }
                let (metrics, bitmap) = font.rasterize(ch, px);
                if metrics.width == 0 || metrics.height == 0 {
                    continue;
                }
                pen_x += metrics.advance_width;
                glyphs.push((metrics, bitmap));
            }
            if !glyphs.is_empty() {
                rows.push(glyphs);
            }
        }
        rows
    };
    let mut rows = collect(FONT_CARD_PX[3]);
    let px = FONT_CARD_PX[rows.len().clamp(1, 3)];
    if px != FONT_CARD_PX[3] {
        rows = collect(px);
    }

    let band = h as f32 / rows.len() as f32;
    let blend = |c: u8, ink: u8, a: u32| ((ink as u32 * a + c as u32 * (255 - a)) / 255) as u8;
    for (row, glyphs) in rows.iter().enumerate() {
        // The row's ink extent relative to its baseline (fontdue works
        // y-up), used to centre the band and the line itself.
        let ink_top = glyphs
            .iter()
            .map(|(m, _)| m.ymin as f32 + m.height as f32)
            .fold(f32::MIN, f32::max);
        let ink_bottom = glyphs
            .iter()
            .map(|(m, _)| m.ymin as f32)
            .fold(f32::MAX, f32::min);
        let ink_left = glyphs
            .iter()
            .scan(0.0_f32, |pen, (m, _)| {
                let left = *pen + m.xmin as f32;
                *pen += m.advance_width;
                Some(left)
            })
            .fold(f32::MAX, f32::min);
        let ink_right = glyphs
            .iter()
            .scan(0.0_f32, |pen, (m, _)| {
                let right = *pen + m.xmin as f32 + m.width as f32;
                *pen += m.advance_width;
                Some(right)
            })
            .fold(f32::MIN, f32::max);

        let baseline = band * (row as f32 + 0.5) + (ink_top + ink_bottom) * 0.5;
        let offset = (w as f32 - (ink_right - ink_left)) * 0.5 - ink_left;

        let mut pen_x = 0.0_f32;
        for (metrics, bitmap) in glyphs {
            let left = (pen_x + metrics.xmin as f32 + offset).round() as i32;
            let top = (baseline - (metrics.ymin as f32 + metrics.height as f32)).round() as i32;
            for row_px in 0..metrics.height {
                for col in 0..metrics.width {
                    let a = bitmap[row_px * metrics.width + col] as u32;
                    if a == 0 {
                        continue;
                    }
                    let x = left + col as i32;
                    let y = top + row_px as i32;
                    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                        continue;
                    }
                    let p = card.get_pixel_mut(x as u32, y as u32);
                    *p = image::Rgba([
                        blend(p[0], ink[0], a),
                        blend(p[1], ink[1], a),
                        blend(p[2], ink[2], a),
                        255,
                    ]);
                }
            }
            pen_x += metrics.advance_width;
        }
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let tmp = out.with_extension(format!("tmp-{}.jpg", crate::model::new_id().simple()));
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

/// Landscape size of a model card, in pixels.
const MODEL_CARD_SIZE: (u32, u32) = (512, 384);

/// Render a "model card" for a mesh blob: the geometry framed from the
/// default three-quarter camera and shaded by the CPU rasterizer.
///
/// Cards are built during import, off the UI thread, where no GPU device is
/// available — the interactive viewport renders on the GPU instead, and both
/// paths share the same camera and shading parameters. Returns `None` when
/// the bytes are not an OBJ/STL/PLY we can parse, so the asset keeps its icon.
fn write_model_card(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    use crate::media::{formats, render3d};

    // The card is a preview, not the interactive viewport: the cheapest way to
    // get *some* geometry out of the file is the right one, and for a file too
    // large to parse at all that means reading a sample of it.
    let size = std::fs::metadata(blob_path).map(|m| m.len()).unwrap_or(0);
    let mesh = match card_source(blob_path, size) {
        CardSource::Whole => formats::load(blob_path).ok()?,
        CardSource::Chunked => {
            let config = crate::media::chunked::LodConfig {
                memory_budget: 32 << 20,
            };
            crate::media::chunked::load_ply_chunked(blob_path, config).ok()?
        }
        CardSource::Sampled => {
            // A few thousand reads off the front of nothing: a twenty-
            // gigabyte scan cannot be walked at import time to draw a
            // 512×384 card. The points come with their colours, so a scan's
            // card is not grey.
            let (mut streamer, _) =
                crate::media::formats::streaming::PointStreamer::open(blob_path).ok()?;
            let sampled = streamer.sample_points(CARD_SAMPLES)?;
            crate::media::formats::types::Mesh::finish_points(
                sampled.positions,
                Vec::new(),
                sampled.colors,
            )?
        }
    };
    let (w, h) = MODEL_CARD_SIZE;
    // Supersample once: this runs a single time per asset, and the grid is
    // where a jagged silhouette would be most obvious.
    let frame = render3d::render(&mesh, &render3d::Camera::default(), w, h, 2, 1.0);

    let mut card = image::RgbImage::new(w, h);
    for (pixel, src) in card.pixels_mut().zip(frame.bgra.as_chunks::<4>().0.iter()) {
        // The rasterizer emits BGRA (that is what gpui wants); JPEG wants RGB.
        *pixel = image::Rgb([src[2], src[1], src[0]]);
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let tmp = out.with_extension(format!("tmp-{}.jpg", crate::model::new_id().simple()));
    match image::DynamicImage::ImageRgb8(card).save_with_format(&tmp, image::ImageFormat::Jpeg) {
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
    let tmp = out.with_extension(format!("tmp-{}.jpg", crate::model::new_id().simple()));
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
        "heic" | "heif" | "avif" => crate::media::probe::heif_to_image(blob_path),
        "jxl" => render_jxl(blob_path),
        _ if crate::media::probe::is_raw_ext(&ext) => render_raw(blob_path),
        _ => decode_raster(blob_path),
    }
}

/// Decode an ordinary raster (JPEG/PNG/WebP/TIFF/…) with the EXIF
/// orientation applied. `image::open` hands back the raw sensor orientation,
/// so a portrait phone photo would keep lying on its side — thumbnails and
/// palettes would both be wrong (the RAW path applies orientation via
/// rawler in [`render_raw`]; this is the same fix for the plain formats).
fn decode_raster(path: &Path) -> Option<image::DynamicImage> {
    use image::{ImageDecoder, ImageReader};
    let mut decoder = ImageReader::open(path).ok()?.into_decoder().ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

/// Decode a JPEG-XL file through `jxl-oxide` (pure Rust; the `image`
/// integration hands back a plain [`image::DynamicImage`]).
fn render_jxl(path: &Path) -> Option<image::DynamicImage> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = jxl_oxide::integration::JxlDecoder::new(file).ok()?;
    image::DynamicImage::from_decoder(decoder).ok()
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
    let tmp = out.with_extension(format!("tmp-{}.jpg", crate::model::new_id().simple()));
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

    /// The card's three rows: the sample text splits by script (Latin, CJK,
    /// digits), duplicates collapse, non-script characters drop, and a
    /// category the sample never mentions falls back to its built-in line.
    #[test]
    fn specimen_rows_split_by_script() {
        let [latin, cjk, digits] = specimen_rows("AaBbGg 永 0123456789");
        assert_eq!(latin, "AaBbGg");
        assert_eq!(cjk, "永");
        assert_eq!(digits, "0123456789");

        // First occurrences win; repeats drop, CJK punctuation travels with
        // the CJK row.
        let [latin, cjk, digits] = specimen_rows("bA aB、漢字 A1");
        assert_eq!(latin, "bAaB");
        assert_eq!(cjk, "、漢字");
        assert_eq!(digits, "1");

        // A category absent from the sample gets its built-in line.
        let [latin, cjk, digits] = specimen_rows("The quick brown fox");
        assert_eq!(latin, "Thequickbrownfx");
        assert_eq!(cjk, "永");
        assert_eq!(digits, "0123456789");
    }

    /// Script classification: digits and Latin by ASCII, the CJK row takes
    /// ideographs, kana, hangul and fullwidth forms, everything else is out.
    #[test]
    fn script_rows_classify_cjk_ranges() {
        assert_eq!(script_row('5'), Some(ScriptRow::Digit));
        assert_eq!(script_row('z'), Some(ScriptRow::Latin));
        assert_eq!(script_row('Z'), Some(ScriptRow::Latin));
        assert_eq!(script_row('永'), Some(ScriptRow::Cjk));
        assert_eq!(script_row('あ'), Some(ScriptRow::Cjk));
        assert_eq!(script_row('한'), Some(ScriptRow::Cjk));
        assert_eq!(script_row('Ａ'), Some(ScriptRow::Cjk)); // fullwidth
        assert_eq!(script_row('é'), None);
        assert_eq!(script_row(' '), None);
        assert_eq!(script_row('!'), None);
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

    /// A tiny solid PNG — the source the two codec tests below encode from.
    /// Generated rather than committed: neither JPEG-XL nor AVIF has a
    /// pure-Rust encoder we could ship as a fixture.
    fn sample_png(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("src.png");
        let mut img = image::RgbImage::new(8, 6);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgb([220, 90, 40]);
        }
        img.save_with_format(&path, image::ImageFormat::Png)
            .expect("png written");
        path
    }

    /// Encode `src` to `out` with ffmpeg; `false` when ffmpeg is missing or
    /// was built without the encoder we asked for.
    fn ffmpeg_encode(src: &std::path::Path, out: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(src)
            .args(args)
            .arg(out)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn heif_dec_available() -> bool {
        std::process::Command::new("heif-dec")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// JPEG-XL decodes through jxl-oxide with no external tool at runtime —
    /// ffmpeg is only here to build the sample.
    #[test]
    fn jxl_thumbnail_decodes_when_a_sample_can_be_built() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-jxl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = sample_png(&dir);
        let jxl = dir.join("pic.jxl");
        if !ffmpeg_encode(&src, &jxl, &["-c:v", "libjxl"]) {
            eprintln!("skipping: ffmpeg has no JPEG-XL encoder");
            return;
        }

        let out = ensure(&dir, "a".repeat(64).as_str(), AssetKind::Image, &jxl);
        assert!(
            out.is_some_and(|p| p.is_file()),
            "JPEG-XL should produce a thumbnail"
        );
        let dims = crate::media::probe::image_dimensions(&jxl).expect("jxl dimensions");
        assert_eq!((dims.width, dims.height), (8, 6));
    }

    /// AVIF rides the libheif path (same container as HEIC, AV1 payload), so
    /// it only works where libheif was built with an AV1 decoder.
    #[test]
    fn avif_thumbnail_decodes_when_libheif_supports_av1() {
        if !ffmpeg_available() || !heif_dec_available() {
            eprintln!("skipping: ffmpeg or heif-dec not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-avif-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = sample_png(&dir);
        let avif = dir.join("pic.avif");
        if !ffmpeg_encode(&src, &avif, &["-c:v", "libaom-av1", "-still-picture", "1"]) {
            eprintln!("skipping: ffmpeg has no AV1 encoder");
            return;
        }
        if crate::media::probe::heif_to_image(&avif).is_none() {
            eprintln!("skipping: libheif has no AV1 decoder");
            return;
        }

        let out = ensure(&dir, "b".repeat(64).as_str(), AssetKind::Image, &avif);
        assert!(
            out.is_some_and(|p| p.is_file()),
            "AVIF should produce a thumbnail"
        );
    }

    /// A portrait phone photo is stored as landscape sensor data plus an EXIF
    /// orientation tag (6 = rotate 90° CW) — the tag is the only thing saying
    /// so. `image::open` ignores it, so `decode_image` and `image_dimensions`
    /// must apply it themselves or every thumbnail of such a photo lies on
    /// its side.
    #[test]
    fn jpeg_exif_orientation_is_applied() {
        use exif::{In, Tag, Value, experimental::Writer as ExifWriter};

        let dir = std::env::temp_dir().join(format!("trove-orient-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut img = image::RgbImage::new(8, 6);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgb([220, 90, 40]);
        }
        let plain = dir.join("plain.jpg");
        img.save_with_format(&plain, image::ImageFormat::Jpeg)
            .expect("jpeg written");

        // kamadak-exif 0.5 has no JPEG inserter: it emits a bare TIFF
        // payload, so wrap it in a JPEG APP1 segment ("Exif\0\0" + TIFF) and
        // splice it in right after the SOI marker.
        let field = exif::Field {
            tag: Tag::Orientation,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![6]),
        };
        let mut writer = ExifWriter::new();
        writer.push_field(&field);
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, false).expect("exif tiff written");
        let tiff = tiff.into_inner();

        let mut jpeg = std::fs::read(&plain).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "source must start with SOI");
        let mut app1 = vec![0xFF, 0xE1];
        // The segment length counts its own two length bytes.
        app1.extend_from_slice(&((tiff.len() + 6 + 2) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        jpeg.splice(2..2, app1);
        let sideways = dir.join("sideways.jpg");
        std::fs::write(&sideways, &jpeg).unwrap();

        // Decoding rotates the pixels: 8×6 sensor data comes back 6×8.
        let decoded = decode_image(&sideways).expect("jpeg decodes");
        assert_eq!(decoded.dimensions(), (6, 8));

        // The probed dimensions swap as well, or the grid would still size
        // the tile like a landscape photo.
        let dims = crate::media::probe::image_dimensions(&sideways).expect("dimensions");
        assert_eq!((dims.width, dims.height), (6, 8));

        // The twin without the tag must keep reporting 8×6.
        let dims = crate::media::probe::image_dimensions(&plain).expect("dimensions");
        assert_eq!((dims.width, dims.height), (8, 6));

        std::fs::remove_dir_all(&dir).ok();
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

    /// The card path is chosen by size and by what the chunked loader can
    /// actually read: a large OBJ must not be sent to a PLY-only parser, and a
    /// twenty-gigabyte PLY must not be walked at import time.
    #[test]
    fn model_card_source_follows_size_and_format() {
        let ply = Path::new("/tmp/a.ply");
        let obj = Path::new("/tmp/a.obj");
        assert_eq!(card_source(ply, 1 << 20), CardSource::Whole);
        assert_eq!(card_source(ply, CARD_WHOLE_MAX), CardSource::Whole);
        assert_eq!(card_source(ply, CARD_WHOLE_MAX + 1), CardSource::Chunked);
        assert_eq!(card_source(ply, CARD_CHUNKED_MAX), CardSource::Chunked);
        assert_eq!(card_source(ply, CARD_CHUNKED_MAX + 1), CardSource::Sampled);
        assert_eq!(card_source(ply, 20 << 30), CardSource::Sampled);
        // Not PLY: the chunked loader cannot read it, so it is parsed whole
        // (and refused if it is past the loader's own cap).
        assert_eq!(card_source(obj, 100 << 20), CardSource::Whole);
        assert_eq!(card_source(obj, 20 << 30), CardSource::Whole);
        assert_eq!(
            card_source(Path::new("/tmp/a.PLY"), 20 << 30),
            CardSource::Sampled
        );
    }

    #[test]
    fn model_import_gets_a_rendered_card() {
        let dir = std::env::temp_dir().join(format!("trove-modelcard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // A tetrahedron, small enough to write inline.
        let obj = dir.join("tetra.obj");
        std::fs::write(
            &obj,
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 0 0 1\n\
             f 1 2 3\nf 1 2 4\nf 1 3 4\nf 2 3 4\n",
        )
        .unwrap();

        let out = ensure(&dir, "c".repeat(64).as_str(), AssetKind::Model, &obj);
        let card = out.expect("model card written");
        let img = image::open(&card).unwrap();
        assert_eq!(img.dimensions(), MODEL_CARD_SIZE);
        // The card must show geometry, not just the empty backdrop.
        let pixels: Vec<_> = img.to_rgb8().into_raw();
        assert!(
            pixels.as_chunks::<3>().0.iter().any(|p| p[1] < 160),
            "the card should contain shaded geometry"
        );

        // A file that is not a mesh leaves the asset without a card.
        let junk = dir.join("broken.stl");
        std::fs::write(&junk, b"solid nothing\n").unwrap();
        assert!(ensure(&dir, "d".repeat(64).as_str(), AssetKind::Model, &junk).is_none());

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
