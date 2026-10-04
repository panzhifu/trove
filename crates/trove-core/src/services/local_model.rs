//! The local recogniser's model: where it lives, where to get it, and what
//! is already on this machine.
//!
//! candle is an inference engine — it ships no weights. The model this
//! feature uses is OpenAI's Whisper base, multilingual: the smallest of the
//! multilingual checkpoints whose Mandarin survives contact with real
//! recordings, ~290 MB of safetensors fetched once and read from the disk
//! forever after. The download pulls four files straight off a
//! HuggingFace-style resolve URL — `config.json`, `tokenizer.json`,
//! `model.safetensors`, plus the 80-bin mel filter bank the candle example
//! ships as a data file — with the China-friendly mirror tried first and
//! huggingface.co as the fallback.
//!
//! A model already on disk counts too, wherever it came from (a manual
//! download, another tool): [`find_system_models`] scans the usual roots for
//! a directory carrying the signature files, which is what makes a copy
//! fetched from ModelScope or a prior install usable without a second
//! download. There is no OS-provided speech model on any of the desktop
//! platforms this runs on — "system model" here means exactly this scan.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::model_fetch::{content_length, fetch_file};
use crate::error::{Error, Result};

/// The model this build knows how to fetch: Whisper base, multilingual,
/// f32 safetensors. Pinned by directory name — the extracted layout is what
/// the recogniser is pointed at, and a different checkpoint under a
/// different name is still discoverable by the scan.
pub const MODEL_ID: &str = "whisper-base";

/// Approximate download size, for the dialog that asks before spending it.
pub const MODEL_DOWNLOAD_MB: u64 = 300;

/// The four files that make a directory a usable local model. Every
/// detection path — the managed download, a system scan — ends here.
pub const MODEL_FILES: [&str; 4] = [
    "config.json",
    "tokenizer.json",
    "model.safetensors",
    "mel_filters.safetensors",
];

/// Where the managed model lives: `<data>/models/<model-id>/`.
pub fn managed_model_dir() -> PathBuf {
    crate::paths::data_dir().join("models").join(MODEL_ID)
}

/// (repo, file) for each piece of the model.
const SOURCES: [(&str, &str); 4] = [
    ("openai/whisper-base", "config.json"),
    ("openai/whisper-base", "tokenizer.json"),
    ("openai/whisper-base", "model.safetensors"),
    // The 80-bin mel filter bank, published with the candle project's own
    // whisper demo. Weighs 64 KB; the tensor inside is `mel_80`, [80, 201].
    ("spaces/lmz/candle-whisper", "mel_filters.safetensors"),
];

/// Where a usable model is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// A complete model directory is on disk.
    Ready { path: PathBuf },
    /// Nothing usable on this machine.
    Missing,
}

/// Is a usable model present? The managed directory first, then the scan.
pub fn status() -> ModelStatus {
    if let Some(path) = usable(&managed_model_dir()) {
        return ModelStatus::Ready { path };
    }
    find_system_models()
        .into_iter()
        .next()
        .map(|path| ModelStatus::Ready { path })
        .unwrap_or(ModelStatus::Missing)
}

/// A directory is usable when it holds all four model files and the weights
/// are not truncated — the safetensors are hundreds of MB; anything smaller
/// is a partial download. (The small text files are not size-checked; a
/// truncated `config.json` fails loudly at load, in one place.)
fn usable(dir: &Path) -> Option<PathBuf> {
    let present = MODEL_FILES
        .iter()
        .map(|file| dir.join(file))
        .all(|file| file.is_file());
    let weights_ok = fs::metadata(dir.join(MODEL_FILES[2]))
        .map(|meta| meta.len() > 100 * 1024 * 1024)
        .unwrap_or(false);
    (present && weights_ok).then(|| dir.to_path_buf())
}

/// Scan the usual places for a model some other tool (or a manual download)
/// already put down. Bounded: each root, one level in, and two levels in —
/// enough for `<root>/<model-id>/` and `<root>/<group>/<model-id>/`, and
/// nowhere near enough to walk a whole home directory.
pub fn find_system_models() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![crate::paths::data_dir().join("models")];
    if let Some(data) = dirs::data_dir() {
        roots.push(data.join("sherpa-onnx"));
        roots.push(data.join("whisper"));
    }
    if let Some(cache) = dirs::cache_dir() {
        roots.push(cache.join("sherpa-onnx"));
        roots.push(cache.join("whisper"));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("sherpa-onnx"));
        roots.push(home.join("whisper"));
    }

    let mut found = Vec::new();
    for root in roots {
        for dir in candidate_dirs(&root) {
            if let Some(model) = usable(&dir)
                && !found.contains(&model)
            {
                found.push(model);
            }
        }
    }
    found
}

/// The directories to probe under one root: the root itself, its children,
/// and its grandchildren.
fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let Ok(entries) = fs::read_dir(root) else {
        return dirs;
    };
    for child in entries.flatten() {
        let child = child.path();
        if !child.is_dir() {
            continue;
        }
        dirs.push(child.clone());
        if let Ok(grandchildren) = fs::read_dir(&child) {
            dirs.extend(grandchildren.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    dirs
}

/// Download and unpack the model into [`managed_model_dir`], reporting
/// progress as `(received, total)` bytes — `total == 0` while the size is
/// unknown. Files already present are kept (a re-run after a failure picks
/// up where the last one left off), and each file downloads to a `.part`
/// sibling first so a killed run never leaves a half file pretending to be
/// whole.
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
        total += size.unwrap_or(if file.ends_with(".safetensors") { 290_000_000 } else { 1 << 20 });
    }
    let mut received: u64 = 0;
    progress(received, total);

    for ((repo, file), size) in SOURCES.iter().zip(sizes) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::External {
                program: "local-model".into(),
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
                program: "local-model".into(),
                message: format!("{file} downloaded {written} bytes, expected {size}"),
            });
        }
        fs::rename(&partial, &dest)?;
    }
    match usable(&dir) {
        Some(path) => Ok(path),
        None => Err(Error::External {
            program: "local-model".into(),
            message: "the downloaded model failed its completeness check".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signature the whole detection story rests on: a directory holding
    /// the four files with plausible weights reads as ready, a missing or
    /// under-sized one does not.
    #[test]
    fn usability_needs_all_files_and_plausible_weights() {
        let dir = std::env::temp_dir().join(format!("trove-model-test-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(usable(&dir), None, "an empty directory is not a model");

        for file in &MODEL_FILES[..2] {
            fs::write(dir.join(file), b"{}").unwrap();
        }
        fs::write(dir.join(MODEL_FILES[3]), b"{}").unwrap();
        fs::write(dir.join(MODEL_FILES[2]), vec![0u8; 1024]).unwrap();
        assert_eq!(usable(&dir), None, "truncated weights are not a model");

        fs::write(dir.join(MODEL_FILES[2]), vec![0u8; 101 * 1024 * 1024]).unwrap();
        assert_eq!(usable(&dir), Some(dir.clone()));

        let _ = fs::remove_dir_all(&dir);
    }

    /// The managed directory's name is what the recogniser is pointed at
    /// and what the scan finds; the two must never drift.
    #[test]
    fn managed_dir_lives_under_the_models_root() {
        assert_eq!(
            managed_model_dir(),
            crate::paths::data_dir().join("models").join(MODEL_ID)
        );
        assert_eq!(managed_model_dir().file_name().unwrap(), MODEL_ID);
    }
}
