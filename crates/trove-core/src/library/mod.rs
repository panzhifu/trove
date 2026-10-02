//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::history::undo::{self, Flip, Op, OpAction, OpDesc, UndoHistory};
use crate::media;
use crate::model::{AssetLocation, AssetQuery};
use crate::services::collect;
use crate::store::{Store, assets, batch, collections, rows, smart, smart_collections, tags};

/// Outcome of permanently deleting a batch of assets.
#[derive(Debug, Clone, Default)]
pub struct PurgeReport {
    pub purged: u64,
    pub blobs_removed: u64,
    pub thumbs_removed: u64,
    /// Files Trove removed from its own inbox along with their records.
    pub sources_removed: u64,
    /// Linked files outside the inbox deleted along with their records —
    /// only ever at the user's request, via the library's
    /// `purge_delete_sources` setting.
    pub source_files_removed: u64,
}

/// Outcome of a batch image edit. `skipped` counts assets the batch had no
/// business touching (not an image, in the trash, no stored blob);
/// `failures` carries per-asset errors so one bad file cannot sink the
/// whole batch.
#[derive(Debug, Clone, Default)]
pub struct BatchEditReport {
    pub edited: u64,
    pub skipped: u64,
    pub failures: Vec<(Uuid, String)>,
}

/// Outcome of a metadata export run.
#[derive(Debug, Clone, Default)]
pub struct XmpExportReport {
    pub written: u64,
    pub skipped: u64,
}

/// A Trove library on disk. Two roots, because they have different lifetimes:
///
/// ```text
/// <data root>/            worth keeping — and worth backing up
/// ├── library.db
/// ├── library.json        this library's own preferences
/// ├── backups/…           database snapshots
/// └── media/…             blobs, for assets stored before linking was the
///                        only import mode
///
/// <cache root>/           regenerable — deleting it costs only time
/// ├── thumbs/…
/// └── search_index/
/// ```
///
/// The roots come from [`crate::paths`]: `data/libraries/<slug>` and
/// `cache/libraries/<slug>`.
#[derive(Clone)]
pub struct Library {
    store: Store,
    root: PathBuf,
    /// Thumbnails and the full-text index. Always a different directory from
    /// `root`; see [`Library::cache`].
    cache: PathBuf,
    /// Undo/redo operation history for invertible metadata mutations (see
    /// [`crate::history`]). Bounded; the cap comes from the app config.
    undo: UndoHistory,
    /// Background jobs (imports, maintenance, preview work) owned by this
    /// library; see [`crate::tasks`]. Cheap to share: `Arc` inside.
    tasks: std::sync::Arc<crate::tasks::TaskManager>,
    /// Jobs the task journal recorded as still running or paused when the
    /// previous process exited. Read once at open, before this library records
    /// anything new; see [`Library::interrupted_tasks`].
    interrupted: Vec<crate::store::task_journal::JournalEntry>,
    /// The Tantivy full-text index under `<cache root>/search_index` — a
    /// disposable derivative of the asset rows, fed by the search_queue
    /// outbox (schema triggers) and drained here.
    text_index: crate::search::TextIndex,
    /// The in-memory embedding index for the model the app last searched
    /// semantically, cached so consecutive queries do not reload the vector
    /// table. Keyed by (model, space) and rebuilt when the provider changes;
    /// the index itself re-checks the table fingerprint per search, so a
    /// finished backfill is visible to the next query.
    vector_index: std::cell::RefCell<
        Option<(
            String,
            crate::model::EmbeddingSpace,
            crate::search::vector::VectorIndex,
        )>,
    >,
}

impl Library {
    /// Open (or create) the library whose data lives under `data_root` and
    /// whose regenerable files live under `cache_root`.
    pub fn open(data_root: impl AsRef<Path>, cache_root: impl AsRef<Path>) -> Result<Self> {
        let root = data_root.as_ref().to_path_buf();
        let cache = cache_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&cache)?;
        let store = Store::open(&root.join("library.db"))?;
        let text_index = crate::search::TextIndex::open(&cache.join("search_index"))?;
        let lib = Self::assemble(root, cache, store, text_index);
        // Reconcile the search index with the asset rows: a fresh, wiped or
        // outdated index re-derives itself from the store here, so `search`
        // never silently returns nothing for assets that predate it.
        lib.reconcile_search_index()?;
        // Daily safety snapshot (24h throttle, rolling 10 files). Best-effort:
        // a failed backup never blocks opening the library.
        crate::services::backup::maybe_auto_backup(&lib.root, lib.store.conn());
        // Trash retention sweep, the other piece of open-time housekeeping —
        // and equally best-effort: a failed purge is logged and retried on the
        // next open, never a reason the library would not open.
        let retention = crate::config::LibraryConfig::load(&lib.root).trash_retention();
        if let Some(days) = retention
            && let Err(error) = lib.purge_expired_trash(days)
        {
            tracing::warn!(%error, days, "trash retention sweep failed");
        }
        Ok(lib)
    }

    /// Open a library for a process that is *not* its sole owner — the CLI,
    /// running while the desktop app holds the same library open.
    ///
    /// Two deliberate differences from [`Library::open`], both of them
    /// consequences of not being alone:
    ///
    /// - The search index is opened read-only *when the lock is taken*. While
    ///   the app holds Tantivy's writer lock, queries are still answered from
    ///   the index on disk instead of failing; when the lock is free this
    ///   handle takes it and behaves exactly like [`Library::open`], including
    ///   reconciling an index that is behind the store. Either way the
    ///   `search_queue` outbox keeps accumulating rows for whoever can drain
    ///   them, so no mutation is lost because a reader was around.
    /// - The daily backup snapshot is skipped: opening a library to read one
    ///   asset is not a reason to write a copy of it.
    pub fn open_read_only(
        data_root: impl AsRef<Path>,
        cache_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let root = data_root.as_ref().to_path_buf();
        let cache = cache_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&cache)?;
        let store = Store::open(&root.join("library.db"))?;
        let text_index = crate::search::TextIndex::open_read_only(&cache.join("search_index"))?;
        let lib = Self::assemble(root, cache, store, text_index);
        // Housekeeping belongs to whoever owns the index. With the lock in
        // hand this process is the library's de facto owner and owes it the
        // same reconciliation `open` does — without which a CLI run after a
        // cache wipe would search an index that knows only about the assets
        // the outbox still remembered. Without the lock it touches nothing.
        if lib.text_index().is_writable() {
            lib.reconcile_search_index()?;
        }
        Ok(lib)
    }

    /// Shared tail of the two constructors: the fields, plus the size gauge
    /// the health endpoint leads with. A failed count is not worth failing
    /// the open over — the gauge just stays at zero.
    fn assemble(
        root: PathBuf,
        cache: PathBuf,
        store: Store,
        text_index: crate::search::TextIndex,
    ) -> Self {
        // One config read for the whole assembly: undo depth, which plugins are
        // off, and therefore which custom task kinds may be scheduled.
        let config = crate::config::AppConfig::load();
        // Open a separate SQLite connection for the task journal. The main
        // Store connection is thread-confined (Rc<RefCell<…>), so the journal
        // needs its own. WAL mode lets both coexist on the same database file.
        let mut tasks = crate::tasks::TaskManager::new();
        tasks.declare_task_kinds(&crate::plugins::task_kinds(&config.disabled_plugins));
        let mut interrupted = Vec::new();
        if let Ok(journal_conn) = rusqlite::Connection::open(root.join("library.db")) {
            journal_conn
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=2000;")
                .ok();
            // Retire before reading: an earlier build journalled the resident
            // watch service, whose terminal row no quit ever writes, so every
            // launch it opened left one more `running` row that
            // `load_interrupted` below reports as interrupted work. The write
            // failure is logged rather than swallowed — if it does not land,
            // those false positives are back on screen and this is the only
            // place that knows why.
            match crate::store::task_journal::retire_resident_runs(&journal_conn) {
                Ok(0) => {}
                Ok(retired) => tracing::info!(
                    retired,
                    "retired journal rows left `running` by the resident watch service"
                ),
                Err(error) => tracing::warn!(
                    %error,
                    "could not retire the resident services' journal rows; they will read as interrupted"
                ),
            }
            // Read before handing the connection to the manager: work that was
            // running when this process died is surfaced (see
            // `Library::interrupted_tasks`) before anything new is recorded.
            interrupted = crate::store::task_journal::load_interrupted(&journal_conn)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "task journal unread; no interrupted work to report");
                    Vec::new()
                });
            tasks.set_journal(journal_conn);
        }
        let lib = Self {
            store,
            root,
            cache,
            undo: UndoHistory::with_cap(config.undo_cap()),
            tasks: std::sync::Arc::new(tasks),
            interrupted,
            text_index,
            vector_index: std::cell::RefCell::new(None),
        };
        // "Opening" the history is two cleanups: the steps a previous session
        // undid and walked away from (redo deliberately does not cross a
        // restart), and any row this build cannot read. Failing here does not
        // fail the open — it means the history stays as it is on disk, and the
        // rows this build cannot apply are then refused one at a time by
        // `UndoHistory::newest` rather than silently vanishing from a panel the
        // user may already be looking at.
        if let Err(error) = lib.undo.open_session(lib.store.conn()) {
            tracing::warn!(
                %error,
                "undo history could not be prepared; leaving it untouched"
            );
        }
        let assets =
            rows::query_count(lib.store.conn(), "SELECT COUNT(*) FROM assets", vec![]).unwrap_or(0);
        crate::metrics::set_library_open(assets.max(0) as u64);
        lib
    }
}

// The `impl Library` blocks live in one file per domain; the methods, the
// `pub` surface and the behaviour are unchanged by the move.
mod lifecycle;
mod metadata;
mod organize;
mod search;
mod storage;

pub use lifecycle::TextSaveOutcome;

#[cfg(test)]
mod tests;
