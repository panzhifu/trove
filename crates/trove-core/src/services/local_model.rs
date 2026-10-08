//! What a local model is, where it lives, where to get it, and what already
//! counts as one on this machine.
//!
//! candle is an inference engine — it ships no weights. Every local model is
//! therefore a small declaration ([`ModelSpec`]) plus the fetch machinery in
//! [`super::model_fetch`]: the Whisper checkpoint the recogniser reads, and
//! the U²-Net graph the background remover reads. A spec names its files, the
//! sources they come from, which of them carries the weights (and how small a
//! complete copy may be), and the directories other tools are known to put
//! their own copies in.
//!
//! That last part is why a download is not the only way in: a model already on
//! disk counts, wherever it came from — a manual download, another tool's
//! cache, a prior run. There is no OS-provided equivalent on any of the
//! desktop platforms this runs on, so "system model" here means exactly this
//! scan.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::model_fetch::{content_length, fetch_file};
use crate::error::{Error, Result};

/// A model this build knows how to fetch and how to recognise.
pub struct ModelSpec {
    /// Directory name under `<data>/models/`, and the name the scan looks for.
    pub id: &'static str,
    /// Approximate download size, for the dialog that asks before spending it.
    pub download_mb: u64,
    /// The files that make a directory a usable model. Every detection path —
    /// the managed download, a system scan — ends here.
    pub files: &'static [&'static str],
    /// (source, file) for each piece, in download order. A source is either a
    /// HuggingFace-style `owner/repo`, resolved against the mirror list, or an
    /// absolute URL used as-is — the latter is how weights published somewhere
    /// other than the Hub come off their own release rather than from whoever
    /// mirrored them.
    pub sources: &'static [(&'static str, &'static str)],
    /// Which entry of `files` carries the weights. A truncated weights file is
    /// the one corruption that reads as success everywhere else.
    pub weights: usize,
    /// The smallest size a real copy of the weights can have.
    pub min_weights_bytes: u64,
    /// Directory names, under the usual data/cache/home roots, where another
    /// tool may already have left a copy.
    pub system_dirs: &'static [&'static str],
}

/// OpenAI's Whisper base, multilingual: the smallest of the multilingual
/// checkpoints whose Mandarin survives contact with real recordings, ~290 MB
/// of safetensors fetched once and read from the disk forever after. Pinned by
/// directory name — a different checkpoint under a different name is still
/// discoverable by the scan.
pub const WHISPER: ModelSpec = ModelSpec {
    id: "whisper-base",
    download_mb: 300,
    files: &[
        "config.json",
        "tokenizer.json",
        "model.safetensors",
        "mel_filters.safetensors",
    ],
    sources: &[
        ("openai/whisper-base", "config.json"),
        ("openai/whisper-base", "tokenizer.json"),
        ("openai/whisper-base", "model.safetensors"),
        // The 80-bin mel filter bank, published with the candle project's own
        // whisper demo. Weighs 64 KB; the tensor inside is `mel_80`, [80, 201].
        ("spaces/lmz/candle-whisper", "mel_filters.safetensors"),
    ],
    weights: 2,
    min_weights_bytes: 100 * 1024 * 1024,
    system_dirs: &["sherpa-onnx", "whisper"],
};

/// U²-Net's saliency graph — the checkpoint `rembg` ships as its default
/// background remover, one 168 MB ONNX file. The official bytes are on the
/// `rembg` GitHub release; the Hub copy is the fallback for networks where
/// that release is unreachable (measured on one such machine: the mirror held
/// ~1.2 MB/s while the release asset crawled far below it).
pub const U2NET: ModelSpec = ModelSpec {
    id: "u2net",
    download_mb: 168,
    files: &["u2net.onnx"],
    sources: &[
        (
            "https://github.com/danielgatis/rembg/releases/download/v0.0.0",
            "u2net.onnx",
        ),
        ("frankminors123/U2Net_ONNX", "u2net.onnx"),
    ],
    weights: 0,
    min_weights_bytes: 100 * 1024 * 1024,
    // `rembg`'s own cache directory: a machine that has run it already has the
    // weights and should not be asked to download them again.
    system_dirs: &[".u2net", "u2net"],
};

/// Where the managed copy of `spec` lives: `<data>/models/<id>/`.
pub fn managed_model_dir(spec: &ModelSpec) -> PathBuf {
    crate::paths::data_dir().join("models").join(spec.id)
}

/// Where a usable model is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// A complete model directory is on disk.
    Ready { path: PathBuf },
    /// Nothing usable on this machine.
    Missing,
}

/// Is a usable model present? The managed directory first, then the scan.
pub fn status(spec: &ModelSpec) -> ModelStatus {
    if let Some(path) = usable(spec, &managed_model_dir(spec)) {
        return ModelStatus::Ready { path };
    }
    find_system_models(spec)
        .into_iter()
        .next()
        .map(|path| ModelStatus::Ready { path })
        .unwrap_or(ModelStatus::Missing)
}

/// A directory is usable when it holds all of the model's files and the
/// weights are not truncated — they are hundreds of MB; anything smaller is a
/// partial download. (The small files are not size-checked; a truncated
/// `config.json` fails loudly at load, in one place.)
fn usable(spec: &ModelSpec, dir: &Path) -> Option<PathBuf> {
    let present = spec.files.iter().all(|file| dir.join(file).is_file());
    let weights_ok = fs::metadata(dir.join(spec.files[spec.weights]))
        .map(|meta| meta.len() > spec.min_weights_bytes)
        .unwrap_or(false);
    (present && weights_ok).then(|| dir.to_path_buf())
}

/// Scan the usual places for a model some other tool (or a manual download)
/// already put down. Bounded: each root, one level in, and two levels in —
/// enough for `<root>/<model-id>/` and `<root>/<group>/<model-id>/`, and
/// nowhere near enough to walk a whole home directory.
pub fn find_system_models(spec: &ModelSpec) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![crate::paths::data_dir().join("models")];
    for name in spec.system_dirs {
        if let Some(data) = dirs::data_dir() {
            roots.push(data.join(name));
        }
        if let Some(cache) = dirs::cache_dir() {
            roots.push(cache.join(name));
        }
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(name));
        }
    }

    let mut found = Vec::new();
    for root in roots {
        for dir in candidate_dirs(&root) {
            if let Some(model) = usable(spec, &dir)
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
            dirs.extend(
                grandchildren
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir()),
            );
        }
    }
    dirs
}

/// Download the model into [`managed_model_dir`], reporting progress as
/// `(received, total)` bytes — `total == 0` while the size is unknown. Files
/// already present are kept (a re-run after a failure picks up where the last
/// one left off), and each file downloads to a `.part` sibling first so a
/// killed run never leaves a half file pretending to be whole.
///
/// A file may be served by more than one source — an official release with a
/// Hub copy behind it — and the next one is tried only when the previous
/// failed outright, so a mirror that is merely slow cannot pre-empt the
/// primary.
pub fn download(
    spec: &ModelSpec,
    cancel: &AtomicBool,
    progress: &dyn Fn(u64, u64),
) -> Result<PathBuf> {
    let dir = managed_model_dir(spec);
    fs::create_dir_all(&dir)?;

    // One entry per declared file: which sources serve it, and the size the
    // first of them that answered advertised. The weights dominate the
    // transfer; the small files barely register, so an unanswered probe is
    // estimated rather than dropped — the total only drives a progress bar.
    let mut total: u64 = 0;
    let mut plan: Vec<(&str, Vec<&str>, Option<u64>)> = Vec::new();
    for (index, file) in spec.files.iter().enumerate() {
        let bases: Vec<&str> = spec
            .sources
            .iter()
            .filter(|(_, source)| source == file)
            .map(|(base, _)| *base)
            .collect();
        if bases.is_empty() {
            return Err(Error::External {
                program: "local-model".into(),
                message: format!("no source serves {file}"),
            });
        }
        let mut size: Option<u64> = None;
        for base in &bases {
            match content_length(base, file, cancel) {
                Ok(advertised) => {
                    size = advertised;
                    break;
                }
                Err(error) => {
                    tracing::warn!(base, file, %error, "size probe failed, trying the next source");
                }
            }
        }
        total += size.unwrap_or(if index == spec.weights {
            spec.download_mb * 1_000_000
        } else {
            1 << 20
        });
        plan.push((file, bases, size));
    }
    let mut received: u64 = 0;
    progress(received, total);

    for (file, bases, size) in plan {
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
        let start = received;
        let mut written: Option<u64> = None;
        let mut last_error: Option<Error> = None;
        for base in &bases {
            let mut got: u64 = 0;
            match fetch_file(base, file, &partial, cancel, |delta| {
                got += delta;
                progress(start + got, total);
            }) {
                Ok(bytes) => {
                    written = Some(bytes);
                    break;
                }
                Err(error) => {
                    if cancel.load(Ordering::Relaxed) {
                        let _ = fs::remove_file(&partial);
                        return Err(error);
                    }
                    tracing::warn!(base, file, %error, "source failed, trying the next");
                    last_error = Some(error);
                }
            }
        }
        let Some(bytes) = written else {
            let _ = fs::remove_file(&partial);
            return Err(last_error.unwrap_or_else(|| Error::External {
                program: "local-model".into(),
                message: format!("{file}: no source answered"),
            }));
        };
        received = start + bytes;
        progress(received, total);
        if let Some(size) = size
            && bytes != size
        {
            let _ = fs::remove_file(&partial);
            return Err(Error::External {
                program: "local-model".into(),
                message: format!("{file} downloaded {bytes} bytes, expected {size}"),
            });
        }
        fs::rename(&partial, &dest)?;
    }
    match usable(spec, &dir) {
        Some(path) => Ok(path),
        None => Err(Error::External {
            program: "local-model".into(),
            message: "the downloaded model failed its completeness check".into(),
        }),
    }
}

/// Delete the managed copy of a model, freeing its disk. A model found by
/// [`find_system_models`] — a copy some other tool put down — is untouched;
/// `status` keeps reporting it. A model that is not on disk is already the
/// requested state, not an error.
pub fn delete(spec: &ModelSpec) -> Result<()> {
    let dir = managed_model_dir(spec);
    match fs::metadata(&dir) {
        Ok(_) => fs::remove_dir_all(dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("trove-model-test-{}", crate::model::new_id()))
    }

    /// The signature the whole detection story rests on: a directory holding
    /// every file with plausible weights reads as ready, a missing or
    /// under-sized one does not.
    #[test]
    fn usability_needs_all_files_and_plausible_weights() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            usable(&WHISPER, &dir),
            None,
            "an empty directory is not a model"
        );

        for file in &WHISPER.files[..2] {
            fs::write(dir.join(file), b"{}").unwrap();
        }
        fs::write(dir.join(WHISPER.files[3]), b"{}").unwrap();
        fs::write(dir.join(WHISPER.files[2]), vec![0u8; 1024]).unwrap();
        assert_eq!(
            usable(&WHISPER, &dir),
            None,
            "truncated weights are not a model"
        );

        fs::write(dir.join(WHISPER.files[2]), vec![0u8; 101 * 1024 * 1024]).unwrap();
        assert_eq!(usable(&WHISPER, &dir), Some(dir.clone()));

        let _ = fs::remove_dir_all(&dir);
    }

    /// A single-file model goes through the same gate: the weights are the
    /// file, so a partial download of it is the only way to fail.
    #[test]
    fn a_single_file_model_is_its_own_completeness_check() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(usable(&U2NET, &dir), None);
        fs::write(dir.join("u2net.onnx"), vec![0u8; 1024]).unwrap();
        assert_eq!(usable(&U2NET, &dir), None, "1 KiB is not a checkpoint");
        fs::write(dir.join("u2net.onnx"), vec![0u8; 101 * 1024 * 1024]).unwrap();
        assert_eq!(usable(&U2NET, &dir), Some(dir.clone()));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The managed directory's name is what the engine is pointed at and what
    /// the scan finds; the two must never drift.
    #[test]
    fn managed_dir_lives_under_the_models_root() {
        for spec in [&WHISPER, &U2NET] {
            assert_eq!(
                managed_model_dir(spec),
                crate::paths::data_dir().join("models").join(spec.id)
            );
            assert_eq!(managed_model_dir(spec).file_name().unwrap(), spec.id);
        }
    }

    /// The two ways a spec can be wrong without any code noticing: a declared
    /// file nobody serves (the download writes nothing `usable` will find),
    /// and a source pointing at a file the spec never declared (it lands in
    /// the directory and is ignored). The weights index has to be one of them.
    #[test]
    fn every_file_has_a_source_and_every_source_names_a_file() {
        for spec in [&WHISPER, &U2NET] {
            assert!(spec.weights < spec.files.len());
            for file in spec.files {
                assert!(
                    spec.sources.iter().any(|(_, s)| s == file),
                    "{file} has no source"
                );
            }
            for (base, file) in spec.sources {
                assert!(spec.files.contains(file), "{base} serves {file}");
                assert!(!base.is_empty());
            }
        }
    }
}
