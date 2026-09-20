//! The import job: stage files on this thread, commit in batched
//! transactions over the job's own database connection.
//!
//! This is the whole import pipeline minus the UI: directory expansion,
//! staging (hash / blob / thumbnail / metadata — see
//! [`crate::media::import`]) and the database commits. It runs on a plain
//! thread managed by [`super::TaskManager`]; the UI thread only receives
//! progress events and the final [`ImportOutcome`].
//!
//! Commit batching: the app previously committed one file per transaction on
//! the main thread. Here every [`commit_batch`] files share one transaction
//! (one fsync per batch instead of per file), with a savepoint per file so a
//! bad file still skips without poisoning its batch.
//!
//! Staging runs in [`stage_window`]-sized windows rather than over the whole
//! batch: the job used to build every [`import::StagedFile`] before opening its
//! first transaction, which held the metadata of the entire drop in memory and
//! made a cancellation wait for the last file. Windowed, both are bounded by
//! the window.
//!
//! Directory expansion also belongs here rather than to the caller: the walk is
//! I/O-bound and a dropped folder can take seconds to enumerate, so the job
//! does it on its own thread and reports the total when it knows it
//! (`total == 0` until then).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use super::JobContext;
use crate::media::import::{self, ImportReport, ImportStorage};
use crate::model::AssetPatch;
use crate::store::assets;

/// Files per transaction. Bigger batches amortize syncs further but widen
/// the window between progress updates and hold write locks longer. The
/// default is calibrated on a real terminal (see `docs/IMPORT-PIPELINE.md`
/// §4); `TROVE_COMMIT_BATCH` overrides it the same way `TROVE_STAGE_THREADS`
/// pins the pool width.
fn commit_batch() -> usize {
    static BATCH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BATCH.get_or_init(|| {
        std::env::var("TROVE_COMMIT_BATCH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| (1..=4096).contains(v))
            .unwrap_or(COMMIT_BATCH_DEFAULT)
    })
}

/// The calibrated default for [`commit_batch`].
const COMMIT_BATCH_DEFAULT: usize = 64;

/// How many files are staged before a round of commits.
///
/// Staging used to run over the whole batch at once: every [`StagedFile`] was
/// built in memory before the first transaction opened, so a 100k-file drop
/// held 100k metadata records and could not be cancelled until the last file
/// had been staged. Windowed staging bounds both — the window is a few
/// pool-widths worth of work, so the commit side never waits long, and a
/// cancellation takes effect within one window instead of one drop. Peak
/// staging memory is bounded by the pool's own decodes, which is where it
/// already was.
///
/// Sized from the *floor* pool width ([`stage_thread_count`]) while staging
/// itself may run wider on a batch of large sources; that only makes the window
/// a finer slice of the pool's work, which is the safe direction for both the
/// commit lag and cancellation latency this exists to bound.
///
/// [`stage_thread_count`]: crate::media::import::stage_thread_count
fn stage_window() -> usize {
    crate::media::import::stage_thread_count() * commit_batch() * 2
}

/// SQLite busy timeout for the job connection: the UI thread keeps reading
/// the same database while this job writes.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How deep a directory walk descends. Watch roots / dropped folders are
/// user folders; going deeper is more likely to crawl something huge than to
/// find real assets.
const MAX_DEPTH: usize = 6;

/// Where an import job takes its files from.
#[derive(Debug, Clone)]
pub enum ImportSource {
    /// Explicit paths (drop, dialog, convert output). Directories are
    /// expanded with [`expand_dirs`].
    Paths {
        paths: Vec<PathBuf>,
        into_collection: Option<uuid::Uuid>,
    },
    /// Files the local collect service received: they are already sitting in
    /// the incoming directory, each with an optional `*.meta.json` sidecar
    /// naming the page they came from. The sidecar is deleted once the asset
    /// is committed; the file itself stays, because the library links it.
    CollectInbox {
        items: Vec<(PathBuf, Option<PathBuf>)>,
    },
}

/// Everything a job needs besides the cancellation/progress context.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// The open library's data root: where its database lives, and where a
    /// copied blob would land. Linking writes nothing here.
    pub data_root: PathBuf,
    /// The open library's cache root: thumbnails are written under it.
    pub cache_root: PathBuf,
    /// How sources are stored. User imports link; the one copy mode is for
    /// sources Trove owns and is about to delete.
    pub storage: ImportStorage,
    pub source: ImportSource,
}

impl ImportOptions {
    /// The library database this job commits into.
    pub fn db_path(&self) -> PathBuf {
        self.data_root.join("library.db")
    }
}

/// What the import job produced.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub report: ImportReport,
    /// True when cancellation was requested mid-run: fewer files were
    /// committed than staged and inbox cleanup was skipped.
    pub cancelled: bool,
    /// The first transaction-level failure of the run, when one happened.
    /// The run keeps going past a failed batch — later batches get a fresh
    /// transaction — but the user should hear that the database started
    /// refusing writes rather than see a bare "N skipped".
    pub error: Option<String>,
}

/// Expand directories in `paths` into their contained files ([`all_files`]'s
/// walk); plain files pass through untouched. Files already present in the
/// list — a dropped folder plus one of its own files, or the same folder
/// twice — are kept once. Missing paths are left in so staging reports them
/// as skips like any other per-file failure.
///
/// Entries the library cannot represent — symlinks, names that are not
/// UTF-8 — are *reported* rather than silently dropped: a folder whose
/// contents quietly half-arrive reads as a bug, not a policy.
pub fn expand_dirs(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<import::ImportSkip>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for path in paths {
        if path.is_dir() {
            dirs.push(path);
        } else {
            files.push(path);
        }
    }
    if dirs.is_empty() {
        return (files, Vec::new());
    }

    let mut skipped = Vec::new();
    let mut seen: HashSet<PathBuf> = files.iter().cloned().collect();
    for file in all_files(&dirs, &mut skipped) {
        if seen.insert(file.clone()) {
            files.push(file);
        }
    }
    (files, skipped)
}

/// Every regular file below `roots`, skipping hidden entries (dot files and
/// dot directories) at any depth. Symlinks and non-UTF-8 names are reported
/// into `skipped` — see [`expand_dirs`].
pub fn all_files(roots: &[PathBuf], skipped: &mut Vec<import::ImportSkip>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        walk(root, 0, &mut out, skipped);
    }
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, skipped: &mut Vec<import::ImportSkip>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            // A name the library cannot record; keeping it out is right,
            // doing it silently is not.
            skipped.push(import::ImportSkip {
                path: path.clone(),
                reason: "file name is not valid UTF-8".into(),
            });
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(&path, depth + 1, out, skipped),
            Ok(ft) if ft.is_file() => out.push(path),
            // Symlinks are left out on purpose (a link can point at the
            // directory being walked), but the leave-out is on the record.
            Ok(_) => skipped.push(import::ImportSkip {
                path,
                reason: "symbolic links are not imported".into(),
            }),
            Err(error) => skipped.push(import::ImportSkip {
                path,
                reason: error.to_string(),
            }),
        }
    }
}

/// Run one import to completion. Synchronous and self-contained: tests call
/// it directly, [`super::TaskManager`] runs it on a thread.
pub fn run(options: &ImportOptions, ctx: &JobContext) -> Result<ImportOutcome, String> {
    let started = std::time::Instant::now();
    let (mut paths, into_collection, walk_skips) = match &options.source {
        ImportSource::Paths {
            paths,
            into_collection,
        } => {
            // The directory walk belongs here, on the job's thread. The UI
            // thread used to run it only to learn the total, then this job ran
            // it again — twice the I/O for one number, and a folder of 200k
            // files froze the window for the whole walk. Progress is
            // indeterminate (`total == 0`) until the list exists.
            let (expanded, skipped) = expand_dirs(paths.clone());
            (expanded, *into_collection, skipped)
        }
        ImportSource::CollectInbox { items } => (
            items.iter().map(|(p, _)| p.clone()).collect(),
            None,
            Vec::new(),
        ),
    };
    if ctx.cancelled() {
        return Ok(ImportOutcome {
            report: ImportReport::default(),
            cancelled: true,
            error: None,
        });
    }

    // Open through the store once so pending schema migrations apply, then
    // reopen a plain connection for the job (transactions need &mut, and the
    // store keeps its connection behind a RefCell).
    let db_path = options.db_path();
    crate::store::Store::open(&db_path).map_err(|e| format!("open library database: {e}"))?;
    let mut conn = Connection::open(&db_path).map_err(|e| format!("open library database: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("set busy timeout: {e}"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -16000;",
    )
    .map_err(|e| format!("set connection pragmas: {e}"))?;

    let mut report = ImportReport {
        skipped: walk_skips,
        ..Default::default()
    };

    // The collect inbox keeps its files (they are linked, not copied), so a
    // sweep over a long-lived inbox would re-stage — re-hash — its whole
    // history every time anything new lands. Files the library already holds
    // (same name and size) are dropped before staging and counted separately
    // from real skips: nothing was wrong with them, they are just in already.
    if matches!(options.source, ImportSource::CollectInbox { .. }) {
        let known = assets::known_keys(&conn);
        let before = paths.len();
        paths.retain(|path| match assets::known_key(path) {
            Some(key) => !known.contains(&key),
            None => true,
        });
        report.already_imported = (before - paths.len()) as u64;
    }

    let total = paths.len() as u64;
    ctx.set_total(total);

    // The sidecar lookup used to be a linear scan of the item list per
    // imported file; for an inbox of thousands that is quadratic.
    let sidecars: HashMap<PathBuf, Option<PathBuf>> = match &options.source {
        ImportSource::CollectInbox { items } => items.iter().cloned().collect(),
        _ => HashMap::new(),
    };

    let mut done: u64 = 0;
    let mut cancelled = false;
    let mut error: Option<String> = None;
    let window = stage_window();

    'windows: for slice in paths.chunks(window) {
        if ctx.cancelled() {
            cancelled = true;
            break;
        }
        let staged = import::stage_all(
            &options.data_root,
            &options.cache_root,
            slice,
            options.storage,
            ctx.cancel_flag(),
        );
        for chunk in staged.chunks(commit_batch()) {
            if ctx.cancelled() {
                cancelled = true;
                break 'windows;
            }
            if let Err(batch_error) =
                commit_chunk(&mut conn, chunk, into_collection, &sidecars, &mut report)
            {
                // The whole chunk rolled back together. Record every one of
                // its files as skipped and move on: a later batch gets a
                // fresh transaction, and a report that died here would hide
                // everything already committed.
                tracing::error!(error = %batch_error, "import batch failed; its files are reported as skipped");
                if error.is_none() {
                    error = Some(batch_error.clone());
                }
                for item in chunk {
                    let path = match item {
                        Ok(file) => file.path.clone(),
                        Err(skip) => skip.path.clone(),
                    };
                    report.skipped.push(import::ImportSkip {
                        path,
                        reason: batch_error.clone(),
                    });
                }
            }
            done += chunk.len() as u64;
            ctx.progress(done, total);
        }
    }

    if !cancelled && matches!(options.source, ImportSource::CollectInbox { .. }) {
        cleanup_inbox(&sidecars);
    }

    ctx.set_summary(format!(
        "{} imported, {} skipped",
        report.imported_count(),
        report.skipped_count()
    ));
    crate::metrics::note_import_run(
        report.imported_count(),
        report.skipped_count(),
        error.is_some(),
        started.elapsed(),
    );
    Ok(ImportOutcome {
        report,
        cancelled,
        error,
    })
}

/// Commit one batch: one transaction, a savepoint per file so a bad file
/// skips without poisoning its batch.
///
/// Any transaction-level failure — begin, savepoint, commit — fails the whole
/// chunk with its files rolled back together, and comes back as an `Err`
/// instead of being thrown past the report: whatever earlier batches already
/// committed must still reach the user.
fn commit_chunk(
    conn: &mut Connection,
    chunk: &[std::result::Result<import::StagedFile, import::ImportSkip>],
    into_collection: Option<uuid::Uuid>,
    sidecars: &HashMap<PathBuf, Option<PathBuf>>,
    report: &mut ImportReport,
) -> Result<(), String> {
    let mut tx = conn
        .transaction()
        .map_err(|e| format!("begin batch: {e}"))?;
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for item in chunk {
        match item {
            Ok(file) => {
                let sp = tx.savepoint().map_err(|e| format!("savepoint: {e}"))?;
                match import::commit_staged(&sp, into_collection, file) {
                    Ok(imported_item) => {
                        stamp_collect_source(&sp, sidecars, file, &imported_item);
                        sp.commit().map_err(|e| format!("commit file: {e}"))?;
                        imported.push(imported_item);
                    }
                    Err(e) => {
                        // Dropped savepoint = rolled back file.
                        skipped.push(import::ImportSkip {
                            path: file.path.clone(),
                            reason: e.to_string(),
                        });
                    }
                }
            }
            Err(skip) => skipped.push(skip.clone()),
        }
    }
    tx.commit().map_err(|e| format!("commit batch: {e}"))?;
    // Only on a successful commit do the results enter the report — a failed
    // commit rolls the batch back, so its files must not read as imported.
    report.imported.extend(imported);
    report.skipped.extend(skipped);
    Ok(())
}

/// Collect-inbox imports stamp the sidecar's `source_url` onto the fresh
/// asset, inside the same savepoint as the insert.
fn stamp_collect_source(
    conn: &Connection,
    sidecars: &HashMap<PathBuf, Option<PathBuf>>,
    file: &import::StagedFile,
    imported: &import::ImportItem,
) {
    let Some(Some(sidecar)) = sidecars.get(&file.path) else {
        return;
    };
    let Ok(meta) = std::fs::read_to_string(sidecar) else {
        return;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta) else {
        return;
    };
    let Some(url) = meta.get("source_url").and_then(|v| v.as_str()) else {
        return;
    };
    let patch = AssetPatch {
        source_url: Some(Some(url.to_string())),
        ..Default::default()
    };
    let _ = assets::update(conn, imported.asset_id, &patch);
}

/// Drop the sidecars of a processed inbox batch. The files themselves stay:
/// staging *linked* them, so removing one would leave the asset pointing at
/// nothing. Only runs on uncancelled completions so a stopped job leaves its
/// files queued for the next sweep.
fn cleanup_inbox(sidecars: &HashMap<PathBuf, Option<PathBuf>>) {
    for sidecar in sidecars.values().flatten() {
        let _ = std::fs::remove_file(sidecar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempdir::Temp;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn imports_in_batches_and_reports() {
        let root = Temp::new("task-import");
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let mut paths = Vec::new();
        for i in 0..(commit_batch() * 2 + 1) {
            let p = src.join(format!("img{i}.png"));
            // Distinct content per file so nothing dedupes away.
            fs::write(&p, [PNG_1X1, &[i as u8][..]].concat()).unwrap();
            paths.push(p);
        }

        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::Paths {
                paths,
                into_collection: None,
            },
        };
        let outcome = run(&options, &JobContext::for_tests(false)).unwrap();
        assert!(
            outcome.report.skipped.is_empty(),
            "first skip: {:?}",
            outcome.report.skipped.first().map(|s| (&s.path, &s.reason))
        );
        assert_eq!(outcome.report.imported_count(), commit_batch() * 2 + 1);
        assert!(!outcome.cancelled);
    }

    #[test]
    fn expand_dirs_skips_hidden_and_dedupes() {
        let root = Temp::new("task-expand");
        let folder = root.path().join("folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("a.png"), b"x").unwrap();
        fs::write(folder.join(".hidden.png"), b"x").unwrap();
        let top = root.path().join("top.gif");
        fs::write(&top, b"x").unwrap();

        let (expanded, skipped) =
            expand_dirs(vec![folder.clone(), top.clone(), folder.join("a.png")]);
        assert!(expanded.contains(&top));
        assert!(expanded.contains(&folder.join("a.png")));
        assert_eq!(
            expanded
                .iter()
                .filter(|p| **p == folder.join("a.png"))
                .count(),
            1
        );
        assert!(
            !expanded
                .iter()
                .any(|p| p.file_name().unwrap().to_string_lossy().starts_with('.'))
        );
        // Hidden entries are policy (not offered), but they are not
        // "skipped" either — only entries a walk *found* and cannot use
        // belong in the report.
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    /// A symlink inside a dropped folder is reported, not silently dropped:
    /// a folder whose contents quietly half-arrive reads as a bug.
    ///
    /// Unix-only: creating a symlink on Windows needs a privilege the test
    /// environment does not have, and without one the assertions below have
    /// nothing to say.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_dropped_folder_is_reported_as_skipped() {
        let root = Temp::new("task-symlink");
        let folder = root.path().join("folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("a.png"), b"x").unwrap();
        std::os::unix::fs::symlink(folder.join("a.png"), folder.join("link.png")).unwrap();

        let (expanded, skipped) = expand_dirs(vec![folder.clone()]);
        assert!(expanded.contains(&folder.join("a.png")));
        assert!(!expanded.iter().any(|p| p.ends_with("link.png")));
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].path.ends_with("link.png"));
    }

    /// A dropped folder reaches the job unexpanded: the walk is the job's own
    /// work now. The UI thread used to run it purely to learn the total, and
    /// the job then ran it again for the same list.
    #[test]
    fn a_directory_source_is_expanded_inside_the_job() {
        let root = Temp::new("task-dir-source");
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..5 {
            fs::write(
                src.join(format!("img{i}.png")),
                [PNG_1X1, &[i as u8][..]].concat(),
            )
            .unwrap();
        }

        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::Paths {
                paths: vec![src.clone()],
                into_collection: None,
            },
        };
        let outcome = run(&options, &JobContext::for_tests(false)).unwrap();
        assert_eq!(
            outcome.report.imported_count(),
            5,
            "{:?}",
            outcome.report.skipped
        );
    }

    #[test]
    fn cancel_stops_before_the_next_batch() {
        let root = Temp::new("task-cancel");
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let mut paths = Vec::new();
        for i in 0..(commit_batch() * 3) {
            let p = src.join(format!("img{i}.png"));
            fs::write(&p, [PNG_1X1, &[i as u8][..]].concat()).unwrap();
            paths.push(p);
        }

        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::Paths {
                paths,
                into_collection: None,
            },
        };
        // The flag is already set: the job stops before its first batch.
        let outcome = run(&options, &JobContext::for_tests(true)).unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.report.imported_count(), 0);
    }

    /// A collected file has to outlive its own import: the pipeline links it,
    /// so deleting the inbox copy — which the copy-based pipeline used to do —
    /// would leave the asset pointing at nothing. Only the sidecar goes.
    #[test]
    fn a_collected_file_survives_its_own_import() {
        let root = Temp::new("task-inbox");
        let inbox = root.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let file = inbox.join("shot.png");
        fs::write(&file, PNG_1X1).unwrap();
        let sidecar = inbox.join("shot.png.meta.json");
        fs::write(&sidecar, r#"{"source_url":"https://example.com/a.png"}"#).unwrap();

        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::CollectInbox {
                items: vec![(file.clone(), Some(sidecar.clone()))],
            },
        };
        let outcome = run(&options, &JobContext::for_tests(false)).unwrap();
        assert_eq!(
            outcome.report.imported_count(),
            1,
            "{:?}",
            outcome.report.skipped
        );
        assert!(file.is_file(), "the collected file must survive the import");
        assert!(!sidecar.exists(), "the sidecar is consumed");

        let store = crate::store::Store::open(&options.db_path()).unwrap();
        let all = assets::query(store.conn(), &crate::model::AssetQuery::default()).unwrap();
        let asset = &all.items[0];
        assert_eq!(
            asset.facts.source_path.as_deref(),
            Some(file.display().to_string().as_str())
        );
        assert_eq!(
            asset.source_url.as_deref(),
            Some("https://example.com/a.png")
        );
    }

    /// The whole point of the batch-error change: a commit that fails must
    /// come back as an `Err` carrying the reason, and the report must keep
    /// whatever earlier batches put in it — "failed, 0 imported 0 skipped"
    /// used to throw all of that away. The connection already being inside a
    /// transaction is the deterministic stand-in for any database refusing a
    /// batch at the same point (write lock held past the busy timeout, disk
    /// full at commit).
    #[test]
    fn a_batch_that_cannot_begin_fails_without_touching_the_report() {
        let root = Temp::new("task-batch-lock");
        let db = root.path().join("library.db");
        crate::store::Store::open(&db).unwrap();
        let mut conn = Connection::open(&db).unwrap();
        // No busy timeout to wait out: the failure must be immediate.
        conn.busy_timeout(Duration::from_millis(1)).unwrap();

        conn.execute_batch("BEGIN;").unwrap();
        let mut report = ImportReport::default();
        let outcome = commit_chunk(&mut conn, &[], None, &HashMap::new(), &mut report);
        conn.execute_batch("ROLLBACK;").unwrap();

        let error = outcome.expect_err("the nested batch must fail");
        assert!(error.contains("begin batch"), "{error}");
        assert!(report.imported.is_empty());
        assert!(report.skipped.is_empty());
    }

    /// The inbox keeps its files, so a sweep re-runs over its whole history:
    /// a file the library already holds (same name and size) is recognised
    /// and counted as "already imported" instead of being staged — and
    /// re-hashed — a second time.
    #[test]
    fn an_inbox_file_the_library_already_holds_is_not_staged_again() {
        let root = Temp::new("task-inbox-dedup");
        let inbox = root.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let file = inbox.join("shot.png");
        fs::write(&file, PNG_1X1).unwrap();

        // First import, as a plain drop.
        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::Paths {
                paths: vec![file.clone()],
                into_collection: None,
            },
        };
        let first = run(&options, &JobContext::for_tests(false)).unwrap();
        assert_eq!(first.report.imported_count(), 1);

        // The same file, now arriving as an inbox sweep item (as it does
        // every sweep for as long as the inbox keeps its files).
        let options = ImportOptions {
            data_root: root.path().to_path_buf(),
            cache_root: root.path().join("cache"),
            storage: ImportStorage::Link,
            source: ImportSource::CollectInbox {
                items: vec![(file.clone(), None)],
            },
        };
        let second = run(&options, &JobContext::for_tests(false)).unwrap();
        assert_eq!(second.report.imported_count(), 0);
        assert_eq!(
            second.report.already_imported, 1,
            "recognized as already in the library: {second:?}"
        );
        assert!(second.report.skipped.is_empty());

        let store = crate::store::Store::open(&options.db_path()).unwrap();
        let all = assets::query(store.conn(), &crate::model::AssetQuery::default()).unwrap();
        assert_eq!(all.items.len(), 1, "no duplicate row appeared");
    }

    // Small helpers so tests can build contexts without a manager thread.
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
}
