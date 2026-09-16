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
//! the main thread. Here every [`COMMIT_BATCH`] files share one transaction
//! (one fsync per batch instead of per file), with a savepoint per file so a
//! bad file still skips without poisoning its batch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use super::JobContext;
use crate::media::import::{self, ImportReport, ImportStorage};
use crate::model::AssetPatch;
use crate::store::assets;

/// Files per transaction. Bigger batches amortize fsyncs further but widen
/// the window between progress updates and hold write locks longer.
const COMMIT_BATCH: usize = 16;

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
}

/// Expand directories in `paths` into their contained files ([`all_files`]'s
/// walk); plain files pass through untouched. Files already present in the
/// list — a dropped folder plus one of its own files, or the same folder
/// twice — are kept once. Missing paths are left in so staging reports them
/// as skips like any other per-file failure.
pub fn expand_dirs(paths: Vec<PathBuf>) -> Vec<PathBuf> {
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
        return files;
    }

    let mut seen: HashSet<PathBuf> = files.iter().cloned().collect();
    for file in all_files(&dirs) {
        if seen.insert(file.clone()) {
            files.push(file);
        }
    }
    files
}

/// Every regular file below `roots`, skipping hidden entries (dot files and
/// dot directories) at any depth.
pub fn all_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        walk(root, 0, &mut out);
    }
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(&path, depth + 1, out),
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// Run one import to completion. Synchronous and self-contained: tests call
/// it directly, [`super::TaskManager`] runs it on a thread.
pub fn run(options: &ImportOptions, ctx: &JobContext) -> Result<ImportOutcome, String> {
    let (paths, into_collection) = match &options.source {
        ImportSource::Paths {
            paths,
            into_collection,
        } => (expand_dirs(paths.clone()), *into_collection),
        ImportSource::CollectInbox { items } => {
            (items.iter().map(|(p, _)| p.clone()).collect(), None)
        }
    };
    let total = paths.len() as u64;
    ctx.set_total(total);

    // Open through the store once so pending schema migrations apply, then
    // reopen a plain connection for the job (transactions need &mut, and the
    // store keeps its connection behind a RefCell).
    let db_path = options.db_path();
    crate::store::Store::open(&db_path).map_err(|e| format!("open library database: {e}"))?;
    let mut conn = Connection::open(&db_path).map_err(|e| format!("open library database: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("set busy timeout: {e}"))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| format!("enable foreign keys: {e}"))?;

    let staged = import::stage_all(
        &options.data_root,
        &options.cache_root,
        &paths,
        options.storage,
    );
    let mut report = ImportReport::default();
    let mut done: u64 = 0;
    let mut cancelled = false;

    for chunk in staged.chunks(COMMIT_BATCH) {
        if ctx.cancelled() {
            cancelled = true;
            break;
        }
        let mut tx = conn
            .transaction()
            .map_err(|e| format!("begin batch: {e}"))?;
        for item in chunk {
            match item {
                Ok(file) => {
                    // Savepoint per file: a mid-file failure rolls back only
                    // that file, keeping the rest of the batch intact.
                    let sp = tx.savepoint().map_err(|e| format!("savepoint: {e}"))?;
                    match import::commit_staged(&sp, into_collection, file) {
                        Ok(imported) => {
                            stamp_collect_source(&sp, &options.source, file, &imported);
                            sp.commit().map_err(|e| format!("commit file: {e}"))?;
                            report.imported.push(imported);
                        }
                        Err(e) => {
                            // Dropped savepoint = rolled back file.
                            report.skipped.push(import::ImportSkip {
                                path: file.path.clone(),
                                reason: e.to_string(),
                            });
                        }
                    }
                }
                Err(skip) => report.skipped.push(skip.clone()),
            }
            done += 1;
            ctx.progress(done, total);
        }
        tx.commit().map_err(|e| format!("commit batch: {e}"))?;
    }

    if !cancelled && let ImportSource::CollectInbox { items } = &options.source {
        cleanup_inbox(items);
    }

    ctx.set_summary(format!(
        "{} imported, {} skipped",
        report.imported_count(),
        report.skipped_count()
    ));
    Ok(ImportOutcome { report, cancelled })
}

/// Collect-inbox imports stamp the sidecar's `source_url` onto the fresh
/// asset, inside the same savepoint as the insert.
fn stamp_collect_source(
    conn: &Connection,
    source: &ImportSource,
    file: &import::StagedFile,
    imported: &import::ImportItem,
) {
    let ImportSource::CollectInbox { items } = source else {
        return;
    };
    let Some((_, Some(sidecar))) = items.iter().find(|(p, _)| *p == file.path) else {
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
fn cleanup_inbox(items: &[(PathBuf, Option<PathBuf>)]) {
    for (_, sidecar) in items {
        if let Some(sidecar) = sidecar {
            let _ = std::fs::remove_file(sidecar);
        }
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
        for i in 0..(COMMIT_BATCH * 2 + 1) {
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
        assert_eq!(outcome.report.imported_count(), COMMIT_BATCH * 2 + 1);
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

        let expanded = expand_dirs(vec![folder.clone(), top.clone(), folder.join("a.png")]);
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
    }

    #[test]
    fn cancel_stops_before_the_next_batch() {
        let root = Temp::new("task-cancel");
        let src = root.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let mut paths = Vec::new();
        for i in 0..(COMMIT_BATCH * 3) {
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
