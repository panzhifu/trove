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
    if kind != AssetKind::Image {
        return None;
    }
    let out = abs_path(root, sha);
    if out.is_file() {
        return Some(out);
    }
    write_thumb(blob_path, &out)
}

/// Regenerate a thumbnail unconditionally, overwriting any existing file.
/// Returns the thumbnail path on success, or `None` when the blob is not a
/// decodable image. Used by maintenance to rebuild a corrupt cache entry.
pub fn regenerate(root: &Path, sha: &str, kind: AssetKind, blob_path: &Path) -> Option<PathBuf> {
    if kind != AssetKind::Image {
        return None;
    }
    let out = abs_path(root, sha);
    write_thumb(blob_path, &out)
}

/// Decode `blob_path` and atomically write its downscaled JPEG to `out`
/// (via a temporary file + rename). Missing/corrupt thumbs never fail the
/// caller — they mean "no thumbnail yet".
fn write_thumb(blob_path: &Path, out: &Path) -> Option<PathBuf> {
    let image = image::open(blob_path).ok()?;
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
    let tmp = out.with_extension("jpg.tmp");
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
