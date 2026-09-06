//! Dominant (quantized) color extraction — the one piece of image metadata that
//! is universal: unlike EXIF, it is present for every decodable raster.
//!
//! Best-effort: an undecodable or degenerate image yields an empty palette
//! rather than an error, consistent with the rest of metadata mining.

use std::collections::HashMap;
use std::path::Path;

use image::GenericImageView;

/// Downsample to at most `GRID`x`GRID` pixels before quantizing, so reading a
/// 60 MP photo costs ~600 px, not millions.
const GRID: u32 = 24;

/// How many entries a returned palette may have.
pub const MAX_COLORS: usize = 8;

/// Return the most common colors of the image at `path`, most->least frequent,
/// as `#rrggbb` hex strings (quantized to 3 bits per channel). Empty when the
/// file is not a decodable image.
pub fn dominant_colors(path: &Path) -> Vec<String> {
    let image = match image::open(path) {
        Ok(i) => i,
        Err(_) => return Vec::new(),
    };
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return Vec::new();
    }

    // Bucket every pixel by its top 3 bits per channel (4096 buckets).
    let small = image.thumbnail(GRID, GRID).to_rgb8();
    let mut counts: HashMap<u32, u64> = HashMap::new();
    for px in small.pixels() {
        let key = ((px[0] >> 5) as u32) << 6 | ((px[1] >> 5) as u32) << 3 | (px[2] >> 5) as u32;
        *counts.entry(key).or_insert(0) += 1;
    }

    let mut ranked: Vec<(u32, u64)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.into_iter().take(MAX_COLORS).map(|(key, _)| bucket_hex(key)).collect()
}

/// Map a quantized bucket key back to the midpoint of its RGB channel ranges.
fn bucket_hex(key: u32) -> String {
    let r = (((key >> 6) & 7) << 5) + 16;
    let g = (((key >> 3) & 7) << 5) + 16;
    let b = ((key & 7) << 5) + 16;
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage};
    use std::io::Write as _;

    fn solid_png(color: [u8; 3]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-color-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("solid.png");
        let img = RgbImage::from_pixel(64, 48, image::Rgb(color));
        img.save_with_format(&p, ImageFormat::Png).unwrap();
        p
    }

    fn solid_jpeg(color: [u8; 3]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-color-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("solid.jpg");
        let img = RgbImage::from_pixel(64, 48, image::Rgb(color));
        img.save_with_format(&p, ImageFormat::Jpeg).unwrap();
        p
    }

    #[test]
    fn solid_image_has_that_color_dominant() {
        // (200,30,10): 200>>5=6, 30>>5=0, 10>>5=0 -> center #d01010.
        let colors = dominant_colors(&solid_png([200, 30, 10]));
        assert!(!colors.is_empty());
        assert_eq!(colors[0], "#d01010");
        assert!(colors[0].starts_with('#'));
    }

    #[test]
    fn jpeg_and_png_both_yield_palette() {
        assert_eq!(dominant_colors(&solid_png([0, 0, 0])).first().map(String::as_str), Some("#101010"));
        assert!(!dominant_colors(&solid_jpeg([255, 255, 255])).is_empty());
    }

    #[test]
    fn palette_length_is_bounded() {
        let colors = dominant_colors(&solid_png([10, 200, 90]));
        assert!(!colors.is_empty());
        assert!(colors.len() <= MAX_COLORS);
    }

    #[test]
    fn undecodable_file_yields_empty_palette() {
        let dir = std::env::temp_dir().join(format!("trove-color-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("garbage.png");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"not an image at all, definitely not decodable").unwrap();
        assert!(dominant_colors(&p).is_empty());
    }
}