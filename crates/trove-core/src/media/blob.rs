//! Content-addressed blob storage: stream a source file into the library while
//! hashing it, then place it under `media/<hash[:2]>/<hash>.<ext>`.
//!
//! The hashing itself lives in [`super::hash`]; this module owns the *layout*
//! (where a blob goes, and how it gets there without ever being observable
//! half-written) and the deduplication that falls out of it: the name of a
//! blob is the hash of its bytes, so a blob that is already there already
//! holds the same content.

use std::path::{Path, PathBuf};

use super::hash;
use crate::error::Result;

/// Result of staging one source file.
#[derive(Debug)]
pub struct StagedBlob {
    /// Hex BLAKE3 of the content.
    pub content_hash: String,
    /// Original bytes size.
    pub size: u64,
    /// Library-relative path, e.g. `media/ab/cdef….png`.
    pub rel_path: String,
    /// True when the blob already existed (deduplicated; nothing was written).
    pub existed: bool,
}

/// Streaming copy of `src` into the media store under a content-addressed
/// name, hashing while copying so the source is read exactly once.
///
/// The file is first written to a temporary name next to its final location;
/// on success it is renamed into place. If the final blob already exists the
/// temporary file is discarded and `existed` is set.
pub fn stage(src: &Path, root: &Path, ext: &str) -> Result<StagedBlob> {
    stage_with(src, root, ext, None)
}

/// [`stage`] for a caller that already knows the content hash — the hash
/// cache or the dedup pre-check told it, and re-reading the source to hash it
/// again would be paying twice for one answer.
///
/// The bytes still have to be copied (this is the mode for sources Trove is
/// about to delete), but they are copied without a hasher: one pass, no
/// digest computed. A wrong `known_hash` would be a wrong *name*, which is
/// why the only callers are the ones that measured it or remembered it.
pub fn stage_with(
    src: &Path,
    root: &Path,
    ext: &str,
    known_hash: Option<&str>,
) -> Result<StagedBlob> {
    let media_dir = root.join("media");
    std::fs::create_dir_all(&media_dir)?;
    let tmp = tmp_path(root);
    let result = stage_into(src, root, ext, &tmp, known_hash);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn stage_into(
    src: &Path,
    root: &Path,
    ext: &str,
    tmp: &Path,
    known_hash: Option<&str>,
) -> Result<StagedBlob> {
    let (content_hash, size) = match known_hash {
        Some(hash) => (hash.to_string(), hash::copy(src, tmp)?),
        None => hash::copy_and_hash(src, tmp)?,
    };
    let rel_path = rel_path(&content_hash, ext);
    let final_path = root.join(&rel_path);

    if final_path.exists() {
        // Already stored (deduplicated) — drop the redundant copy.
        let _ = std::fs::remove_file(tmp);
        return Ok(StagedBlob {
            content_hash,
            size,
            rel_path,
            existed: true,
        });
    }

    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(tmp, &final_path)?;

    Ok(StagedBlob {
        content_hash,
        size,
        rel_path,
        existed: false,
    })
}

/// `media/ab/<hash>.<ext>`
pub fn rel_path(hash: &str, ext: &str) -> String {
    let (a, b) = hash.split_at(2);
    if ext.is_empty() {
        format!("media/{a}/{b}")
    } else {
        format!("media/{a}/{b}.{ext}")
    }
}

fn tmp_path(root: &Path) -> PathBuf {
    root.join("media")
        .join(format!(".tmp-{}", uuid::Uuid::new_v4()))
}

/// Streaming content hash of an existing file without copying it. Used by the
/// linked-import mode, where the source file stays where it is.
///
/// Thin on purpose: the algorithm, the adaptive buffer and the parallel path
/// are [`super::hash`]'s business, and every other hasher in the crate goes
/// through the same functions — so there is one definition of "the hash of
/// this file" rather than three that can drift apart.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    hash::hash_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "trove-blob-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn staging_places_the_blob_under_its_content_hash() {
        let root = temp_root("place");
        let src = root.join("pic.png");
        std::fs::write(&src, PNG_1X1).unwrap();

        let staged = stage(&src, &root, "png").unwrap();
        assert_eq!(staged.content_hash, hash::hash_bytes(PNG_1X1));
        assert_eq!(staged.size, PNG_1X1.len() as u64);
        assert!(!staged.existed);
        assert_eq!(staged.rel_path, rel_path(&staged.content_hash, "png"));
        assert!(root.join(&staged.rel_path).is_file());

        // Same content again: deduplicated, nothing written.
        let again = stage(&src, &root, "png").unwrap();
        assert!(again.existed);
        assert_eq!(again.content_hash, staged.content_hash);
        // No temporary files left behind either way.
        assert_eq!(std::fs::read_dir(root.join("media")).unwrap().count(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A caller that already knows the hash skips the hasher, not the copy:
    /// the bytes still land, and they land under the hash it was given.
    #[test]
    fn a_known_hash_still_copies_the_bytes() {
        let root = temp_root("known");
        let src = root.join("pic.png");
        std::fs::write(&src, PNG_1X1).unwrap();
        let known = hash::hash_bytes(b"the caller said so");

        let staged = stage_with(&src, &root, "png", Some(&known)).unwrap();
        assert_eq!(staged.content_hash, known);
        assert_eq!(staged.size, PNG_1X1.len() as u64, "the size is still read");
        assert_eq!(staged.rel_path, rel_path(&known, "png"));
        assert_eq!(std::fs::read(root.join(&staged.rel_path)).unwrap(), PNG_1X1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_source_fails_and_leaves_no_temporary_behind() {
        let root = temp_root("missing");
        let gone = root.join("gone.png");
        assert!(stage(&gone, &root, "png").is_err());
        let media = root.join("media");
        if media.is_dir() {
            assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hashing_through_the_blob_facade_agrees_with_the_content() {
        let root = temp_root("hashfile");
        let src = root.join("pic.png");
        std::fs::write(&src, PNG_1X1).unwrap();
        let (digest, size) = hash_file(&src).unwrap();
        assert_eq!(digest, hash::hash_bytes(PNG_1X1));
        assert_eq!(size, PNG_1X1.len() as u64);
        std::fs::remove_dir_all(&root).ok();
    }
}
