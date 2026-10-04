//! The local embedder's model: where it lives, where to get it, and what is
//! already on this machine.
//!
//! The model is BAAI's `bge-small-zh-v1.5` — the small BGE checkpoint tuned
//! for Chinese (and competent at English), the best quality-per-megabyte of
//! the BGE family for a library's short fingerprint texts. ~91 MB of
//! safetensors fetched once and read from the disk forever after; the three
//! files ride the same mirrors and `.part` discipline as the transcriber's
//! download (see [`super::model_fetch`]), which is also what makes the
//! settings row for it a copy of the transcription one.
//!
//! There is no system-model scan here, unlike the Whisper service: BGE has
//! no OS-provided or commonly pre-installed distribution to scan for, and a
//! manually downloaded copy can simply be dropped into the managed
//! directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::model_fetch::{content_length, fetch_file};
use crate::error::{Error, Result};

/// The model this build knows how to fetch: `bge-small-zh-v1.5`, f32
/// safetensors. Pinned by directory name — a different checkpoint under a
/// different name is nobody's business here.
pub const MODEL_ID: &str = "bge-small-zh-v1.5";

/// Approximate download size, for the dialog that asks before spending it.
pub const MODEL_DOWNLOAD_MB: u64 = 100;

/// The three files that make a directory a usable local embedder. Every
/// detection path — the managed download, a hand-placed copy — ends here.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Where the managed model lives: `<data>/models/<model-id>/` — the same
/// models root the transcriber's directory sits under.
pub fn managed_model_dir() -> PathBuf {
    crate::paths::data_dir().join("models").join(MODEL_ID)
}

/// (repo, file) for each piece of the model.
const SOURCES: [(&str, &str); 3] = [
    ("BAAI/bge-small-zh-v1.5", "config.json"),
    ("BAAI/bge-small-zh-v1.5", "tokenizer.json"),
    ("BAAI/bge-small-zh-v1.5", "model.safetensors"),
];

/// Where a usable model is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// A complete model directory is on disk.
    Ready { path: PathBuf },
    /// Nothing usable on this machine.
    Missing,
}

/// Is a usable model present?
pub fn status() -> ModelStatus {
    usable(&managed_model_dir())
        .map(|path| ModelStatus::Ready { path })
        .unwrap_or(ModelStatus::Missing)
}

/// A directory is usable when it holds all three model files and the weights
/// are not truncated — the safetensors are ~91 MB; anything smaller is a
/// partial download. (The small text files are not size-checked; a truncated
/// `config.json` fails loudly at load, in one place.)
fn usable(dir: &Path) -> Option<PathBuf> {
    let present = MODEL_FILES
        .iter()
        .map(|file| dir.join(file))
        .all(|file| file.is_file());
    let weights_ok = fs::metadata(dir.join(MODEL_FILES[2]))
        .map(|meta| meta.len() > 64 * 1024 * 1024)
        .unwrap_or(false);
    (present && weights_ok).then(|| dir.to_path_buf())
}

/// Download the model into [`managed_model_dir`], reporting progress as
/// `(received, total)` bytes — `total == 0` while the size is unknown. Files
/// already present are kept (a re-run after a failure picks up where the
/// last one left off), and each file downloads to a `.part` sibling first so
/// a killed run never leaves a half file pretending to be whole.
pub fn download(cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let dir = managed_model_dir();
    fs::create_dir_all(&dir)?;

    // The weights dominate the transfer; the small files barely register,
    // so the total is their known size plus whatever the server advertises
    // for the rest.
    let mut total: u64 = 0;
    let mut sizes: Vec<Option<u64>> = Vec::new();
    for (repo, file) in SOURCES {
        let size = content_length(repo, file, cancel)?;
        sizes.push(size);
        total += size.unwrap_or(if file.ends_with(".safetensors") {
            MODEL_DOWNLOAD_MB * 1024 * 1024
        } else {
            1 << 20
        });
    }
    let mut received: u64 = 0;
    progress(received, total);

    for ((repo, file), size) in SOURCES.iter().zip(sizes) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::External {
                program: "embed-model".into(),
                message: "cancelled".into(),
            });
        }
        let dest = dir.join(file);
        if dest.is_file() {
            received += size.unwrap_or(0);
            progress(received, total);
            continue;
        }
        let partial = dir.join(format!("{file}.part"));
        let written = fetch_file(repo, file, &partial, cancel, |delta| {
            received += delta;
            progress(received, total);
        })?;
        if let Some(size) = size
            && written != size
        {
            let _ = fs::remove_file(&partial);
            return Err(Error::External {
                program: "embed-model".into(),
                message: format!("{file} downloaded {written} bytes, expected {size}"),
            });
        }
        fs::rename(&partial, &dest)?;
    }
    match usable(&dir) {
        Some(path) => Ok(path),
        None => Err(Error::External {
            program: "embed-model".into(),
            message: "the downloaded model failed its completeness check".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signature the whole detection story rests on: a directory holding
    /// the three files with plausible weights reads as ready, a missing or
    /// under-sized one does not.
    #[test]
    fn usability_needs_all_files_and_plausible_weights() {
        let dir = std::env::temp_dir().join(format!("trove-embed-model-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(usable(&dir), None, "an empty directory is not a model");

        for file in &MODEL_FILES[..2] {
            fs::write(dir.join(file), b"{}").unwrap();
        }
        fs::write(dir.join(MODEL_FILES[2]), vec![0u8; 1024]).unwrap();
        assert_eq!(usable(&dir), None, "truncated weights are not a model");

        fs::write(dir.join(MODEL_FILES[2]), vec![0u8; 65 * 1024 * 1024]).unwrap();
        assert_eq!(usable(&dir), Some(dir.clone()));

        let _ = fs::remove_dir_all(&dir);
    }

    /// The managed directory's name is what the embedder is pointed at; the
    /// two must never drift.
    #[test]
    fn managed_dir_lives_under_the_models_root() {
        assert_eq!(
            managed_model_dir(),
            crate::paths::data_dir().join("models").join(MODEL_ID)
        );
        assert_eq!(managed_model_dir().file_name().unwrap(), MODEL_ID);
    }
}
