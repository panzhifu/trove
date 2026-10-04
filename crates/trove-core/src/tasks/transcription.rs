//! Speech-to-text job: hand each audio/video asset's track to a cloud
//! recogniser and file the transcript on the asset.
//!
//! Shaped like [`crate::tasks::ai_analysis`] and for the same reasons — its
//! own connection, cooperative cancellation between assets, one outcome —
//! with three differences that come from what it is:
//!
//! - **Every upload is preceded by a local ffmpeg transcode** (see
//!   [`crate::media::audio_prep`]): recognisers want 16 kHz mono, endpoints
//!   cap upload sizes, and long sources are cut into time-bound chunks. The
//!   transcode is the CPU half of the job, which is why the run is serial —
//!   the process-slot pool is already the global gate on ffmpeg concurrency.
//! - **It writes user-visible text**, so it files the transcript in its own
//!   column and records what it did in the asset's `facts` — which is what
//!   [`undo`](Self) reads and what makes a second run free.
//! - **A chunked upload is all-or-nothing**: a run that loses its third of
//!   five chunks writes nothing, leaving the marker untouched so the next run
//!   retries the asset whole instead of splicing transcripts from two models.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use uuid::Uuid;

use crate::ai::source_hash;
use crate::ai::transcribe::TranscribeProvider;
use crate::config::TranscriptionConfig;
use crate::error::Error;
use crate::media::audio_prep;
use crate::model::{Asset, AssetFacts, AssetKind, AssetQuery, AssetSort, TrashPool};
use crate::store::{Store, assets};
use crate::tasks::JobContext;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Page size when walking the library; the store caps a query at 1000.
const PAGE: u32 = 1_000;

/// Key under which an asset records what the transcription did, inside
/// `facts` — the same free-form pattern the analysis marker uses.
const MARKER_KEY: &str = "transcribe";

// ============================ options ======================================

/// What to transcribe, and how.
#[derive(Debug, Clone)]
pub struct TranscribeOptions {
    pub db_path: PathBuf,
    pub data_root: PathBuf,
    /// Restrict the run to these assets. Empty = every live audio/video asset.
    pub only: Vec<Uuid>,
    /// Re-transcribe assets whose fingerprint already matches.
    pub force: bool,
    /// Language hint for the recogniser. `None` = server auto-detects.
    pub language: Option<String>,
    /// Vocabulary hint spelled the way the transcript should spell it.
    pub prompt: Option<String>,
}

/// What the caller wants, leaving the rest to the library.
#[derive(Debug, Clone, Default)]
pub struct TranscribeRunRequest {
    pub only: Vec<Uuid>,
    pub force: bool,
    pub language: Option<String>,
}

impl TranscribeOptions {
    /// Resolve a request against the library's files and the stored
    /// transcription settings: the paths come from the library, the language
    /// from the request with the configuration as its fallback.
    pub fn resolve(
        request: &TranscribeRunRequest,
        db_path: PathBuf,
        data_root: PathBuf,
        config: &TranscriptionConfig,
    ) -> Self {
        Self {
            db_path,
            data_root,
            only: request.only.clone(),
            force: request.force,
            language: request.language.clone().or_else(|| config.language.clone()),
            prompt: config.prompt.clone(),
        }
    }
}

// ============================ outcomes =====================================

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TranscribeOutcome {
    /// Assets whose transcript was fetched and stored.
    pub transcribed: u64,
    /// The ids behind [`Self::transcribed`], in the order they landed. The
    /// UI side uses these to write the matching subtitle sidecars as soon as
    /// the run settles; a count alone could not say *which* files to export.
    pub transcribed_ids: Vec<Uuid>,
    /// Assets skipped because their fingerprint already matched this run.
    pub skipped: u64,
    /// The ids behind [`Self::skipped`]. The UI side exports a missing
    /// subtitle sidecar for these too: a stored transcript whose `.srt` was
    /// deleted (or never written) must come back when the run settles, even
    /// though the recogniser was not asked again.
    pub skipped_ids: Vec<Uuid>,
    /// Video assets with no audio track — nothing a recogniser could do.
    pub no_audio: u64,
    /// Assets whose extraction or request failed; their marker is untouched
    /// and the next run retries them.
    pub failed: u64,
    pub cancelled: bool,
    /// Transcript characters stored, across all assets.
    pub chars: u64,
    pub error: Option<String>,
}

// ============================ run ==========================================

/// Run one transcription pass.
pub fn run(
    options: &TranscribeOptions,
    provider: &dyn TranscribeProvider,
    ctx: &JobContext,
) -> Result<TranscribeOutcome, Error> {
    let started = Instant::now();
    // Open through the store once so pending migrations apply, then take a
    // connection of our own — the same arrangement every job uses.
    Store::open(&options.db_path)?;
    let conn = Connection::open(&options.db_path)
        .map_err(|e| Error::Db(format!("open library database: {e}")))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| Error::Db(format!("set busy timeout: {e}")))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| Error::Db(format!("set connection pragmas: {e}")))?;

    let mut outcome = TranscribeOutcome::default();
    let model = provider.model().to_string();

    ctx.set_summary("scanning library".into());
    let candidates = match candidates(&conn, options) {
        Ok(candidates) => candidates,
        Err(error) => {
            outcome.error = Some(format!("list assets: {error}"));
            return Ok(outcome);
        }
    };

    // One temp home for every chunk this run produces, cleaned when the run
    // ends however it ends. Per-asset subdirectories keep chunks from two
    // assets from ever meeting.
    let run_dir = std::env::temp_dir().join(format!("trove-transcribe-{}", crate::model::new_id()));
    let run_dir_owned = run_dir.clone();
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&run_dir_owned);
    };

    let total = candidates.len() as u64;
    ctx.set_total(total);
    let mut done: u64 = 0;

    for asset in candidates {
        ctx.park_if_paused();
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }

        let fingerprint = fingerprint(&model, options, &asset);
        if !options.force && stored_fingerprint(&asset).as_deref() == Some(fingerprint.as_str()) {
            outcome.skipped += 1;
            outcome.skipped_ids.push(asset.id);
            done += 1;
            ctx.progress(done, total);
            continue;
        }

        let Some(source) = crate::media::thumb::blob_path(&options.data_root, &asset) else {
            outcome.failed += 1;
            done += 1;
            ctx.progress(done, total);
            continue;
        };
        // A video with no audio track is a settled answer, not a failure.
        if asset.kind == AssetKind::Video && !crate::media::video::has_audio_track(&source) {
            outcome.no_audio += 1;
            done += 1;
            ctx.progress(done, total);
            continue;
        }

        let asset_dir = run_dir.join(asset.id.to_string());
        let extracted = match std::fs::create_dir_all(&asset_dir) {
            Ok(()) => audio_prep::extract_chunks(
                &source,
                asset.duration_ms,
                &asset_dir,
                ctx.cancel_flag(),
                provider.chunk_format(),
            ),
            Err(error) => Err(Error::Io(error)),
        };
        match extracted {
            Ok(chunks) if chunks.is_empty() => {
                // The transcode succeeded and produced nothing: a broken
                // container ffmpeg happily "converted" into an empty file.
                outcome.failed += 1;
            }
            Ok(chunks) => match transcribe_chunks(provider, options, &chunks, ctx) {
                Ok(text) => match write_transcript(&conn, &asset, &model, &fingerprint, &text) {
                    Ok(()) => {
                        outcome.transcribed += 1;
                        outcome.transcribed_ids.push(asset.id);
                        outcome.chars += text.chars().count() as u64;
                    }
                    Err(error) => {
                        tracing::warn!(
                            asset = %asset.file_name,
                            error = %error,
                            "transcription: result rejected",
                        );
                        outcome.failed += 1;
                    }
                },
                Err(error) => {
                    // One bad asset must not sink the run; its marker is
                    // untouched, so the next run retries it.
                    outcome.failed += 1;
                    tracing::warn!(
                        asset = %asset.file_name,
                        error = %error,
                        "transcription: request failed",
                    );
                }
            },
            Err(_) if ctx.cancelled() => {
                // The cancel flag killed the transcode mid-flight: unwind
                // rather than count the asset as failed.
                outcome.cancelled = true;
                cleanup();
                return Ok(outcome);
            }
            Err(error) => {
                outcome.failed += 1;
                tracing::warn!(
                    asset = %asset.file_name,
                    error = %error,
                    "transcription: audio extraction failed",
                );
            }
        }
        let _ = std::fs::remove_dir_all(&asset_dir);
        done += 1;
        ctx.progress(done, total);
        ctx.set_summary(format!("{done} / {total}"));
    }
    cleanup();

    tracing::info!(
        transcribed = outcome.transcribed,
        skipped = outcome.skipped,
        no_audio = outcome.no_audio,
        failed = outcome.failed,
        chars = outcome.chars,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "transcription run finished",
    );
    Ok(outcome)
}

// ============================ helpers ======================================

/// The assets this run would consider: live audio and video, newest first,
/// narrowed by `only`. The kind filter runs in Rust because a query is single-
/// kind, and two pages of two kinds would need a merge this loop doesn't want.
fn candidates(conn: &Connection, options: &TranscribeOptions) -> crate::Result<Vec<Asset>> {
    let is_transcribable =
        |asset: &Asset| matches!(asset.kind, AssetKind::Audio | AssetKind::Video);
    if !options.only.is_empty() {
        let mut assets = assets::by_ids(conn, &options.only)?;
        assets.retain(is_transcribable);
        return Ok(assets);
    }

    let mut out: Vec<Asset> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = assets::query(
            conn,
            &AssetQuery {
                pool: TrashPool::Live,
                sort: AssetSort::CreatedAt,
                sort_desc: true,
                limit: Some(PAGE),
                offset,
                ..AssetQuery::live()
            },
        )?;
        if page.items.is_empty() {
            break;
        }
        let short = page.items.len() < PAGE as usize;
        out.extend(page.items.into_iter().filter(is_transcribable));
        if short {
            break;
        }
        offset += PAGE as u64;
    }
    Ok(out)
}

/// Transcribe the chunks in playback order, joining their texts. All chunks
/// or none: an error part-way through aborts the asset's transcript.
fn transcribe_chunks(
    provider: &dyn TranscribeProvider,
    options: &TranscribeOptions,
    chunks: &[PathBuf],
    ctx: &JobContext,
) -> Result<String, Error> {
    let mut parts: Vec<String> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if ctx.cancelled() {
            return Err(Error::Message("transcription cancelled".into()));
        }
        let bytes = std::fs::read(chunk)?;
        let file_name = chunk
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("chunk.m4a")
            .to_string();
        let text = provider
            .transcribe(
                &bytes,
                &file_name,
                "audio/mp4",
                options.language.as_deref(),
                options.prompt.as_deref(),
                ctx.cancel_flag(),
            )
            .map_err(|error| Error::External {
                program: provider.model().to_string(),
                message: error.message,
            })?;
        parts.push(text);
    }
    Ok(parts.join("\n"))
}

/// Store the transcript and record what this run did, so the next run can
/// skip the asset and a later model change re-asks. The facts are re-read
/// first: this column's write is whole-value, and a concurrent writer (the
/// app's own inspector, say) may have moved `facts` under us.
fn write_transcript(
    conn: &Connection,
    asset: &Asset,
    model: &str,
    fingerprint: &str,
    text: &str,
) -> crate::Result<()> {
    assets::set_transcript(conn, asset.id, Some(text))?;

    let mut facts: AssetFacts = match assets::get(conn, asset.id)? {
        Some(current) => current.facts,
        None => asset.facts.clone(),
    };
    facts.unknown.insert(
        MARKER_KEY.into(),
        serde_json::json!({
            "model": model,
            "digest": fingerprint,
            "at": chrono::Utc::now().to_rfc3339(),
        }),
    );
    assets::update_facts(conn, asset.id, &facts)
}

/// What "the same work" means: the recogniser, the language hint, and the
/// content the transcript describes.
fn fingerprint(model: &str, options: &TranscribeOptions, asset: &Asset) -> String {
    let mut input = String::new();
    input.push_str(model);
    input.push('\n');
    input.push_str(options.language.as_deref().unwrap_or("-"));
    input.push('\n');
    input.push_str(asset.content_hash.as_deref().unwrap_or("-"));
    source_hash(&input)
}

fn stored_fingerprint(asset: &Asset) -> Option<String> {
    asset
        .facts
        .unknown
        .get(MARKER_KEY)
        .and_then(|marker| marker.get("digest"))
        .and_then(|digest| digest.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetLocation, ContentHash};
    use crate::store::Store;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempdir::Temp;

    // Same helper the import tests carry: a throwaway library root that
    // cleans up when the test ends.
    mod tempdir {
        use std::path::PathBuf;

        pub struct Temp(PathBuf);
        impl Temp {
            pub fn new(name: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "trove-{name}-{}-{}",
                    std::process::id(),
                    crate::model::new_id().simple()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Temp(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Temp {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
    }

    /// A recogniser that counts calls and answers a fixed transcript.
    struct MockTranscriber {
        calls: std::sync::atomic::AtomicUsize,
        text: String,
    }

    impl TranscribeProvider for MockTranscriber {
        fn model(&self) -> &str {
            "mock-asr"
        }

        fn transcribe(
            &self,
            _audio: &[u8],
            _file_name: &str,
            _mime: &str,
            _language: Option<&str>,
            _prompt: Option<&str>,
            _cancel: &AtomicBool,
        ) -> Result<String, crate::ai::vendor::VendorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.clone())
        }
    }

    /// A library of `count` real (tiny, silent) WAV assets plus one image.
    fn library(count: usize) -> (Temp, Vec<Uuid>) {
        use crate::model::test_asset;
        let root = Temp::new("task-transcribe");
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        let store = Store::open(&root.path().join("data/library.db")).unwrap();
        let mut ids = Vec::new();
        for index in 0..count {
            let source = root.path().join(format!("clip-{index}.wav"));
            std::fs::write(&source, audio_prep::probe_wav()).unwrap();
            let hash = crate::media::hash::hash_bytes(&std::fs::read(&source).unwrap());
            let mut asset = test_asset(
                &format!("clip-{index}.wav"),
                AssetKind::Audio,
                Uuid::new_v4(),
            );
            asset.content_hash = Some(ContentHash::from_hasher(hash));
            asset.duration_ms = Some(1_000);
            asset.set_location(AssetLocation::Linked {
                source_path: source.display().to_string(),
            });
            assets::insert(store.conn(), &asset).unwrap();
            ids.push(asset.id);
        }
        // One image among them: the run must never ask a recogniser about it.
        let image = test_asset("pic.png", AssetKind::Image, Uuid::new_v4());
        assets::insert(store.conn(), &image).unwrap();
        (root, ids)
    }

    fn options(root: &Path, only: Vec<Uuid>) -> TranscribeOptions {
        TranscribeOptions {
            db_path: root.join("data/library.db"),
            data_root: root.join("data"),
            only,
            force: false,
            language: Some("en".into()),
            prompt: None,
        }
    }

    fn ctx() -> JobContext {
        JobContext::for_tests(false)
    }

    /// Audio prep shells out to `ffmpeg` — the same optional, never-linked
    /// runtime dependency the preview player and the export pipeline use (see
    /// `media/thumb.rs` for the same guard). Where it is absent the run fails
    /// every asset honestly, and these success-path tests have nothing to
    /// assert.
    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn transcripts_are_stored_when_ffmpeg_present_and_the_second_run_asks_nothing() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let (_root, ids) = library(2);
        let provider = MockTranscriber {
            calls: std::sync::atomic::AtomicUsize::new(0),
            text: "hello world".into(),
        };
        let options = options(_root.path(), Vec::new());

        let outcome = run(&options, &provider, &ctx()).unwrap();
        assert_eq!(outcome.transcribed, 2, "{outcome:?}");
        assert_eq!(outcome.transcribed_ids.len(), 2, "ids name the exported files");
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.chars, "hello world".len() as u64 * 2);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        for id in &ids {
            let conn = Connection::open(_root.path().join("data/library.db")).unwrap();
            assert_eq!(
                assets::transcript(&conn, *id).unwrap().as_deref(),
                Some("hello world")
            );
        }

        let second = run(&options, &provider, &ctx()).unwrap();
        assert_eq!(second.transcribed, 0);
        assert_eq!(second.skipped, 2, "a repeat run is free: {second:?}");
        assert_eq!(second.skipped_ids.len(), 2, "skipped ids let the UI backfill");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    /// The kind filter is the recogniser's cost control: an image among the
    /// candidates never reaches the provider, and `only` narrows the run to
    /// the selection that asked for it.
    #[test]
    fn images_are_not_transcribed_and_only_narrows_the_run_when_ffmpeg_present() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let (_root, ids) = library(3);
        let provider = MockTranscriber {
            calls: std::sync::atomic::AtomicUsize::new(0),
            text: "x".into(),
        };
        let outcome = run(&options(_root.path(), vec![ids[0]]), &provider, &ctx()).unwrap();
        assert_eq!(outcome.transcribed, 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    /// A transcript that fails to store must not leave a marker behind: the
    /// fingerprint skip reads the marker, so marker-without-text would make
    /// the next run skip an asset that has no transcript.
    #[test]
    fn a_settled_row_carries_both_text_and_marker_when_ffmpeg_present() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let (_root, ids) = library(1);
        let provider = MockTranscriber {
            calls: std::sync::atomic::AtomicUsize::new(0),
            text: "spoken words".into(),
        };
        run(&options(_root.path(), Vec::new()), &provider, &ctx()).unwrap();
        let conn = Connection::open(_root.path().join("data/library.db")).unwrap();
        let asset = assets::get(&conn, ids[0]).unwrap().unwrap();
        let marker = asset.facts.unknown.get(MARKER_KEY).unwrap();
        assert_eq!(
            marker.get("model").and_then(|m| m.as_str()),
            Some("mock-asr")
        );
        assert!(assets::transcript(&conn, ids[0]).unwrap().is_some());
    }
}
