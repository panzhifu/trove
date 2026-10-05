//! The local embedder's models: what can be fetched, where the downloaded
//! copy lives, and what is already on this machine.
//!
//! Two architecture families are supported by the loader (see
//! `ai::embed_local`): BERT — the BAAI BGE v1.5 checkpoints — and
//! XLM-RoBERTa — `bge-m3`, the multilingual checkpoint. For BERT, the
//! checkpoint size is the quality/space dial the settings page exposes:
//! `bge-small-zh-v1.5` is the default (~100 MB), `bge-base-zh-v1.5` and
//! `bge-large-zh-v1.5` trade disk and speed for better Chinese retrieval,
//! and the `-en` checkpoints serve English-primary libraries. `bge-m3` is
//! the one to pick when a library is *both* Chinese and English: it is
//! trained on 100+ languages and is strong in each, at the cost of a much
//! larger download and slower CPU inference.
//!
//! ~100 MB and up of safetensors fetched once and read from the disk
//! forever after; the files ride the same mirrors and `.part` discipline
//! as the transcriber's download (see [`super::model_fetch`]). There is no
//! system-model scan here, unlike the Whisper service: BGE has no
//! OS-provided or commonly pre-installed distribution to scan for, and a
//! manually downloaded copy can simply be dropped into the managed
//! directory.
//!
//! Each entry lives in its own directory under `<data>/models/`, so two
//! models can sit on disk side by side and the settings page's delete
//! control frees one without touching the other. The vectors a model
//! produced are keyed by its identity (`<id> (local)`) — deleting the
//! *files* leaves the rows; deleting the vectors is the settings page's
//! other button.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::model_fetch::{FETCH_ATTEMPTS, content_length, fetch_file};
use crate::error::{Error, Result};

/// One downloadable local embedding model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingModel {
    /// Stable id: the managed directory name, the config's
    /// `local_model` value, and the prefix of the vector identity.
    pub id: &'static str,
    /// HuggingFace repo the three files come from.
    pub repo: &'static str,
    /// Approximate download size, for the dialog that asks before
    /// spending it and the dropdown's size annotation.
    pub download_mb: u64,
    /// The weights file this entry fetches: `model.safetensors` where the
    /// upstream repo carries a conversion, `pytorch_model.bin` (the torch
    /// pickle) where it never got one — the loader reads both formats.
    pub weights_file: &'static str,
    /// A completed weights file must exceed this — anything smaller
    /// is a partial download. (The small text files are not size-checked;
    /// a truncated `config.json` fails loudly at load, in one place.)
    pub weights_min_bytes: u64,
}

impl EmbeddingModel {
    /// The three files that make a directory a usable copy of this model —
    /// every detection path (the managed download, a hand-placed copy) ends
    /// here. The two text files are shared by every entry; the weights are
    /// the entry's own.
    pub fn files(&self) -> [&'static str; 3] {
        ["config.json", "tokenizer.json", self.weights_file]
    }
}

/// The model the local engine uses until the user picks another one.
pub const DEFAULT_MODEL_ID: &str = "bge-small-zh-v1.5";

/// The models this build knows how to fetch, in the order the settings
/// dropdown lists them: Chinese first (the strongest use case), then the
/// bilingual checkpoint, then English.
pub const MODELS: [EmbeddingModel; 6] = [
    EmbeddingModel {
        id: "bge-small-zh-v1.5",
        repo: "BAAI/bge-small-zh-v1.5",
        download_mb: 100,
        weights_file: "model.safetensors",
        weights_min_bytes: 64 * 1024 * 1024,
    },
    // The zh base/large checkpoints have no upstream safetensors conversion —
    // they ship the torch pickle only, which the loader reads just as well.
    EmbeddingModel {
        id: "bge-base-zh-v1.5",
        repo: "BAAI/bge-base-zh-v1.5",
        download_mb: 400,
        weights_file: "pytorch_model.bin",
        weights_min_bytes: 256 * 1024 * 1024,
    },
    EmbeddingModel {
        id: "bge-large-zh-v1.5",
        repo: "BAAI/bge-large-zh-v1.5",
        download_mb: 1_300,
        weights_file: "pytorch_model.bin",
        weights_min_bytes: 900 * 1024 * 1024,
    },
    // The bilingual one: XLM-RoBERTa, 100+ languages, strong in both Chinese
    // and English. A much bigger download and slower on CPU, which is why it
    // is an option rather than the default. Pickle weights like the zh
    // base/large entries.
    EmbeddingModel {
        id: "bge-m3",
        repo: "BAAI/bge-m3",
        download_mb: 2_270,
        weights_file: "pytorch_model.bin",
        weights_min_bytes: 2_100 * 1024 * 1024,
    },
    EmbeddingModel {
        id: "bge-small-en-v1.5",
        repo: "BAAI/bge-small-en-v1.5",
        download_mb: 130,
        weights_file: "model.safetensors",
        weights_min_bytes: 90 * 1024 * 1024,
    },
    EmbeddingModel {
        id: "bge-base-en-v1.5",
        repo: "BAAI/bge-base-en-v1.5",
        download_mb: 430,
        weights_file: "model.safetensors",
        weights_min_bytes: 300 * 1024 * 1024,
    },
];

/// The catalog entry for `model_id`, falling back to the default for an
/// unknown id — a config written by a newer build (or hand-edited) must
/// still resolve to something downloadable rather than dead-end the
/// feature.
pub fn resolve(model_id: &str) -> &'static EmbeddingModel {
    MODELS
        .iter()
        .find(|model| model.id == model_id)
        .unwrap_or(&MODELS[0])
}

/// Where the managed copy of `model` lives: `<data>/models/<model-id>/` —
/// the same models root the transcriber's directory sits under.
pub fn managed_model_dir(model: &str) -> PathBuf {
    crate::paths::data_dir()
        .join("models")
        .join(resolve(model).id)
}

/// Where a usable copy of `model` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// A complete model directory is on disk.
    Ready { path: PathBuf },
    /// Nothing usable on this machine.
    Missing,
}

/// Is a usable copy of `model` present?
pub fn status(model: &str) -> ModelStatus {
    let entry = resolve(model);
    usable(&managed_model_dir(model), entry)
        .map(|path| ModelStatus::Ready { path })
        .unwrap_or(ModelStatus::Missing)
}

/// A directory is usable when it holds all three of the entry's files and
/// the weights are not truncated — the threshold is the entry's own, since
/// the checkpoints range from ~100 MB to ~2.3 GB.
fn usable(dir: &Path, entry: &EmbeddingModel) -> Option<PathBuf> {
    let present = entry
        .files()
        .iter()
        .map(|file| dir.join(file))
        .all(|file| file.is_file());
    let weights_ok = fs::metadata(dir.join(entry.weights_file))
        .map(|meta| meta.len() > entry.weights_min_bytes)
        .unwrap_or(false);
    (present && weights_ok).then(|| dir.to_path_buf())
}

/// Download `model` into [`managed_model_dir`], reporting progress as
/// `(received, total)` bytes — `total == 0` while the size is unknown.
/// Files already present are kept (a re-run after a failure picks up
/// where the last one left off), and each file downloads to a `.part`
/// sibling first so a killed run never leaves a half file pretending to
/// be whole.
pub fn download(model: &str, cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let entry = resolve(model);
    let dir = crate::paths::data_dir().join("models").join(entry.id);
    fs::create_dir_all(&dir)?;
    tracing::info!(model = entry.id, dir = %dir.display(), "model download: starting");

    let sources = entry.files().map(|file| (entry.repo, file));
    // The weights dominate the transfer; the small files barely register,
    // so the total is their known size plus whatever the server advertises
    // for the rest.
    let mut total: u64 = 0;
    let mut sizes: Vec<Option<u64>> = Vec::new();
    for (repo, file) in sources {
        let size = content_length(repo, file, cancel)?;
        sizes.push(size);
        total += size.unwrap_or(if file == entry.weights_file {
            entry.download_mb * 1024 * 1024
        } else {
            1 << 20
        });
    }
    let mut received: u64 = 0;
    progress(received, total);

    for ((repo, file), size) in sources.iter().zip(sizes) {
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
        // The redirected CDN stalls and resets mid-stream on constrained
        // networks, so one pull is not a promise — each file gets a few
        // attempts before the download gives up. A stream that ends short
        // of (or past) the advertised size is a failed attempt too, not a
        // success: the CDN truncates cleanly sometimes, and a silent size
        // mismatch here used to kill the whole download without a single
        // log line. An attempt restarts the file (the `.part` is
        // truncated), and the progress bar rewinds with it: `received`
        // only counts what the wire delivered for the winning attempt.
        let mut written = 0u64;
        for attempt in 1..=FETCH_ATTEMPTS {
            let mut attempt_received: u64 = 0;
            let result = fetch_file(repo, file, &partial, cancel, |delta| {
                attempt_received += delta;
                progress(received + attempt_received, total);
            });
            let outcome = result.and_then(|bytes| match size {
                Some(expected) if bytes != expected => Err(Error::External {
                    program: "embed-model".into(),
                    message: format!("{file} downloaded {bytes} bytes, expected {expected}"),
                }),
                _ => Ok(bytes),
            });
            match outcome {
                Ok(bytes) => {
                    written = bytes;
                    break;
                }
                Err(error) if cancel.load(Ordering::Relaxed) => return Err(error),
                Err(error) => {
                    let _ = fs::remove_file(&partial);
                    if attempt == FETCH_ATTEMPTS {
                        return Err(error);
                    }
                    tracing::warn!(file, attempt, %error, "model fetch: attempt failed, retrying");
                    std::thread::sleep(std::time::Duration::from_secs(2 * attempt as u64));
                }
            }
        }
        received += written;
        progress(received, total);
        fs::rename(&partial, &dest)?;
    }
    match usable(&dir, entry) {
        Some(path) => Ok(path),
        None => Err(Error::External {
            program: "embed-model".into(),
            message: "the downloaded model failed its completeness check".into(),
        }),
    }
}

/// Delete the managed copy of `model`, freeing its disk. The vectors it
/// produced stay in the library — clearing those is the settings page's
/// separate, model-keyed action. A model that is not on disk is already
/// the requested state, not an error.
pub fn delete(model: &str) -> Result<()> {
    remove_model_dir(&managed_model_dir(model))
}

/// The removal behind [`delete`], split out so the completeness contract
/// (a missing directory is success) is testable without touching the
/// real data directory.
fn remove_model_dir(dir: &Path) -> Result<()> {
    match fs::metadata(dir) {
        Ok(_) => fs::remove_dir_all(dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is the settings dropdown's source of truth: ids unique,
    /// every entry resolvable, and the default first — an id that fails
    /// any of these would dead-end the download or the identity the
    /// vectors are keyed by.
    #[test]
    fn the_catalog_is_clean_and_defaults_to_the_first_entry() {
        let mut ids = std::collections::HashSet::new();
        for model in &MODELS {
            assert!(!model.id.is_empty());
            assert!(model.repo.starts_with("BAAI/"), "{}: repo", model.id);
            assert!(model.download_mb > 0);
            assert!(
                model.weights_file == "model.safetensors"
                    || model.weights_file == "pytorch_model.bin",
                "{}: unexpected weights format",
                model.id
            );
            assert_eq!(
                model.files()[2],
                model.weights_file,
                "{}: the weights are the file list's third member",
                model.id
            );
            assert!(
                model.weights_min_bytes < model.download_mb * 1024 * 1024,
                "{}: the truncation gate must be under the advertised size",
                model.id
            );
            assert!(ids.insert(model.id), "{}: duplicate id", model.id);
        }
        assert_eq!(MODELS[0].id, DEFAULT_MODEL_ID);
        assert_eq!(resolve("bge-base-zh-v1.5").id, "bge-base-zh-v1.5");
        // The repos without an upstream safetensors conversion ride the torch
        // pickle; a safetensors entry pointing at one of them would 404 the
        // download before a byte is written.
        assert_eq!(
            resolve("bge-m3").weights_file,
            "pytorch_model.bin",
            "bge-m3 upstream has no model.safetensors"
        );
        assert_eq!(
            resolve("bge-small-zh-v1.5").weights_file,
            "model.safetensors"
        );
        assert_eq!(
            resolve("no-such-model").id,
            DEFAULT_MODEL_ID,
            "an unknown id falls back to the default, never a dead end"
        );
    }

    /// The signature the whole detection story rests on: a directory
    /// holding the three files with plausible weights reads as ready, a
    /// missing or under-sized one does not — with the threshold taken
    /// from the model's own entry. Driven through every entry, since the
    /// pickle-shaped ones carry a different file list; the weights are
    /// sparse files (the check reads the metadata size, not the bytes).
    #[test]
    fn usability_needs_all_files_and_plausible_weights() {
        for entry in &MODELS {
            let dir = std::env::temp_dir()
                .join(format!("trove-embed-model-test-{}", crate::model::new_id()));
            fs::create_dir_all(&dir).unwrap();
            assert_eq!(
                usable(&dir, entry),
                None,
                "an empty directory is not a model"
            );

            for file in &entry.files()[..2] {
                fs::write(dir.join(file), b"{}").unwrap();
            }
            sparse_file(&dir.join(entry.weights_file), 1024);
            assert_eq!(
                usable(&dir, entry),
                None,
                "truncated weights are not a model"
            );

            sparse_file(&dir.join(entry.weights_file), entry.weights_min_bytes + 1);
            assert_eq!(usable(&dir, entry), Some(dir.clone()));

            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// A file whose metadata size is `len` without the bytes behind it —
    /// the completeness check reads `metadata().len()`, and allocating
    /// gigabytes of zeros in a test is pure waste.
    fn sparse_file(path: &Path, len: u64) {
        fs::File::create(path).unwrap().set_len(len).unwrap();
    }

    /// Deleting a managed copy is idempotent: an existing directory goes,
    /// an absent one is already the requested state rather than an error
    /// — the settings button must not toast a failure for doing nothing.
    #[test]
    fn removal_is_idempotent() {
        let dir =
            std::env::temp_dir().join(format!("trove-embed-model-del-{}", crate::model::new_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.safetensors"), b"x").unwrap();
        remove_model_dir(&dir).unwrap();
        assert!(!dir.exists());
        remove_model_dir(&dir).unwrap();
    }
}
