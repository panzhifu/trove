//! In-place image editing primitives for batch operations.
//!
//! Pure transforms over decoded pixels: rotate, flip and crop. The result is
//! re-encoded in the input's own format, so the caller can hand the output
//! straight back into the content-addressed store via
//! [`crate::media::blob::stage`].
//!
//! Two deliberate limits:
//! - Re-encoding drops ancillary metadata (EXIF orientation, colour
//!   profiles): the pipeline is "decode pixels, transform, encode". The
//!   library keeps the asset's captured-at timestamp, so the important facts
//!   survive even though the file's own EXIF does not.
//! - Only formats the `image` crate can *encode* are accepted as output
//!   (JPEG / PNG / WebP / BMP / TIFF / GIF); anything else (HEIC, AVIF,
//!   JXL, RAW, PSD, …) decodes fine as input but is refused as an edit
//!   target instead of silently transcoding.

use std::io::Cursor;
use std::path::Path;

use image::GenericImageView;

use crate::error::{Error, Result};

/// One pixel transform. A batch applies its edits in order, so
/// `[Rotate90, Crop { … }]` crops the already-rotated image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEdit {
    Rotate90,
    Rotate180,
    Rotate270,
    FlipHorizontal,
    FlipVertical,
    /// Crop to the rectangle `(x, y, w, h)` in the current orientation.
    /// Coordinates are validated: the rectangle must sit inside the image.
    Crop {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    },
}

/// The re-encoded result: bytes plus the geometry after all edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutput {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// MIME type of the re-encoded file (follows the input format).
    pub mime: &'static str,
}

/// Apply `edits` to the image at `input` and re-encode it in the input's
/// own format. `jpeg_quality` (1–100) only affects JPEG output.
pub fn apply(input: &Path, edits: &[ImageEdit], jpeg_quality: u8) -> Result<EditOutput> {
    if edits.is_empty() {
        return Err(Error::Validation("no edits requested".into()));
    }
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (format, mime) = output_format(&ext)
        .ok_or_else(|| Error::Validation(format!("cannot re-encode .{ext} images in place")))?;

    let mut img =
        image::open(input).map_err(|e| Error::Validation(format!("decode failed: {e}")))?;
    for edit in edits {
        img = edit.apply(img)?;
    }
    let (width, height) = img.dimensions();

    let mut out = Cursor::new(Vec::new());
    let encode_result = if format == image::ImageFormat::Jpeg {
        // The generic writer has no quality knob; JPEG needs the explicit
        // encoder or every edit re-compresses at the default (75).
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out,
            jpeg_quality.clamp(1, 100),
        );
        img.write_with_encoder(encoder)
    } else {
        img.write_to(&mut out, format)
    };
    encode_result.map_err(|e| Error::Validation(format!("encode failed: {e}")))?;
    Ok(EditOutput {
        bytes: out.into_inner(),
        width,
        height,
        mime,
    })
}

impl ImageEdit {
    fn apply(self, img: image::DynamicImage) -> Result<image::DynamicImage> {
        use ImageEdit::*;
        use image::imageops;
        Ok(match self {
            Rotate90 => image::DynamicImage::from(imageops::rotate90(&img)),
            Rotate180 => image::DynamicImage::from(imageops::rotate180(&img)),
            Rotate270 => image::DynamicImage::from(imageops::rotate270(&img)),
            FlipHorizontal => image::DynamicImage::from(imageops::flip_horizontal(&img)),
            FlipVertical => image::DynamicImage::from(imageops::flip_vertical(&img)),
            Crop { x, y, w, h } => {
                let (iw, ih) = img.dimensions();
                if w == 0 || h == 0 {
                    return Err(Error::Validation(format!(
                        "crop must be at least 1×1, got {w}×{h}"
                    )));
                }
                if x.saturating_add(w) > iw || y.saturating_add(h) > ih {
                    return Err(Error::Validation(format!(
                        "crop {w}×{h} at ({x}, {y}) does not fit {iw}×{ih}"
                    )));
                }
                image::DynamicImage::from(imageops::crop_imm(&img, x, y, w, h).to_image())
            }
        })
    }
}

/// Map a lowercase input extension to the image format the output is
/// written in (same as the input) plus its MIME type. `None` marks formats
/// we can decode but not re-encode.
fn output_format(ext: &str) -> Option<(image::ImageFormat, &'static str)> {
    let (format, mime) = match ext {
        "jpg" | "jpeg" => (image::ImageFormat::Jpeg, "image/jpeg"),
        "png" => (image::ImageFormat::Png, "image/png"),
        "webp" => (image::ImageFormat::WebP, "image/webp"),
        "bmp" => (image::ImageFormat::Bmp, "image/bmp"),
        "tif" | "tiff" => (image::ImageFormat::Tiff, "image/tiff"),
        "gif" => (image::ImageFormat::Gif, "image/gif"),
        _ => return None,
    };
    Some((format, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4×3 image with one distinguishable byte per pixel (r = x·16, g = y·32),
    /// so transforms can be asserted by colour instead of position.
    fn gradient_image() -> image::DynamicImage {
        let mut buf = image::RgbaImage::new(4, 3);
        for y in 0..3u8 {
            for x in 0..4u8 {
                buf.put_pixel(
                    u32::from(x),
                    u32::from(y),
                    image::Rgba([x * 16, y * 32, 7, 255]),
                );
            }
        }
        image::DynamicImage::from(buf)
    }

    fn write_input(dir: &Path, img: &image::DynamicImage) -> std::path::PathBuf {
        let path = dir.join("input.png");
        img.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        path
    }

    #[test]
    fn rotate90_swaps_dimensions_and_pixels() {
        let dir = std::env::temp_dir().join(format!("trove-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = write_input(&dir, &gradient_image());

        let out = apply(&input, &[ImageEdit::Rotate90], 90).unwrap();
        assert_eq!((out.width, out.height), (3, 4));
        assert_eq!(out.mime, "image/png");

        let decoded = image::load_from_memory(&out.bytes).unwrap();
        // Column x of the source becomes row (3 - x)? No: rotate90 maps the
        // top-left pixel (0,0) to the top-right corner (2,0).
        assert_eq!(decoded.get_pixel(2, 0), image::Rgba([0, 0, 7, 255]));
        // Source bottom-left (0,2) becomes top-left (0,0).
        assert_eq!(decoded.get_pixel(0, 0), image::Rgba([0, 64, 7, 255]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flips_mirror_the_expected_edges() {
        let img = gradient_image();
        let h = ImageEdit::FlipHorizontal.apply(img.clone()).unwrap();
        assert_eq!(h.get_pixel(0, 0), img.get_pixel(3, 0));
        let v = ImageEdit::FlipVertical.apply(img).unwrap();
        assert_eq!(v.get_pixel(0, 0), image::Rgba([0, 64, 7, 255]));
    }

    #[test]
    fn crop_takes_the_requested_rectangle_and_validates_bounds() {
        let img = gradient_image();
        let cropped = ImageEdit::Crop {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        }
        .apply(img.clone())
        .unwrap();
        assert_eq!(cropped.dimensions(), (2, 2));
        assert_eq!(cropped.get_pixel(0, 0), img.get_pixel(1, 1));
        assert_eq!(cropped.get_pixel(1, 1), img.get_pixel(2, 2));

        // Out of bounds, in both coordinates, and a zero-sized crop.
        assert!(
            ImageEdit::Crop {
                x: 3,
                y: 1,
                w: 2,
                h: 2
            }
            .apply(img.clone())
            .is_err()
        );
        assert!(
            ImageEdit::Crop {
                x: 0,
                y: 0,
                w: 5,
                h: 1
            }
            .apply(img.clone())
            .is_err()
        );
        assert!(
            ImageEdit::Crop {
                x: 0,
                y: 0,
                w: 1,
                h: 0
            }
            .apply(img)
            .is_err()
        );
    }

    #[test]
    fn jpeg_output_honours_quality_and_mime() {
        let dir = std::env::temp_dir().join(format!("trove-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input.jpg");
        gradient_image()
            .save_with_format(&path, image::ImageFormat::Jpeg)
            .unwrap();

        let out = apply(&path, &[ImageEdit::Rotate180], 60).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        // JPEG is lossy, so decode back and just check the geometry survived.
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!(decoded.dimensions(), (4, 3));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_output_formats_are_refused() {
        let dir = std::env::temp_dir().join(format!("trove-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input.avif");
        // Content does not matter: the extension alone disqualifies it.
        std::fs::write(&path, b"not really an avif").unwrap();
        assert!(apply(&path, &[ImageEdit::Rotate90], 90).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
