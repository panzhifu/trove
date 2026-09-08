//! Content-addressed blob storage: stream a source file into the library while
//! hashing it, then place it under `media/<hash[:2]>/<hash>.<ext>`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Result;

/// Result of staging one source file.
#[derive(Debug)]
pub struct StagedBlob {
    /// Hex sha-256 of the content.
    pub sha256: String,
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
    let media_dir = root.join("media");
    std::fs::create_dir_all(&media_dir)?;
    let tmp = tmp_path(root);
    let result = stage_into(src, root, ext, &tmp);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn stage_into(src: &Path, root: &Path, ext: &str, tmp: &Path) -> Result<StagedBlob> {
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(tmp)?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 256 * 1024];
    let mut copied: u64 = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        output.write_all(&buffer[..n])?;
        copied += n as u64;
    }
    output.flush()?;
    drop(output);

    let sha256 = hex(&hasher.finalize());
    let rel_path = rel_path(&sha256, ext);
    let final_path = root.join(&rel_path);

    if final_path.exists() {
        // Already stored (deduplicated) — drop the redundant copy.
        let _ = std::fs::remove_file(tmp);
        return Ok(StagedBlob {
            sha256,
            size: copied,
            rel_path,
            existed: true,
        });
    }

    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(tmp, &final_path)?;

    Ok(StagedBlob {
        sha256,
        size: copied,
        rel_path,
        existed: false,
    })
}

/// `media/ab/<sha256>.<ext>`
pub fn rel_path(sha256: &str, ext: &str) -> String {
    let (a, b) = sha256.split_at(2);
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

/// Streaming SHA-256 of an existing file without copying it. Used by the
/// linked-import mode, where the source file stays where it is.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok((hex(&hasher.finalize()), meta.len()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write to string cannot fail");
    }
    s
}
