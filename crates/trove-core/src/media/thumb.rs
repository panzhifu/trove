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
        _ => None,
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
}
