//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::history::undo::{self, Flip, Op, OpAction, OpDesc, SharedUndoStack};
use crate::media;
use crate::model::{AssetLocation, AssetQuery, Placement};
use crate::services::collect;
use crate::store::{Store, assets, batch, collections, rows, smart, smart_collections, tags};

/// Serialize the whole metadata catalog of `store` (assets, collections,
/// tags, smart collections) as pretty JSON. Media blobs are not included —
/// the export is a portable catalog, not a backup of the files.
pub fn export_metadata_from_store(store: &Store) -> Result<String> {
    let conn = store.conn();
    let assets = assets::query(conn, &crate::model::AssetQuery::live())?.items;
    let collections = collections::list(conn)?;
    let tags = tags::list(conn)?;
    let smart_collections = smart_collections::list(conn)?;

    // v2: membership tables — without them a restore cannot rebuild the
    // organization (which asset sits in which collection, which tags it
    // carries). Pairs of (asset_id, collection_id) / (asset_id, tag_id).
    let asset_collections: Vec<(Uuid, Uuid)> = rows::query_map(
        conn,
        "SELECT asset_id, collection_id FROM asset_collection ORDER BY asset_id",
        vec![],
        |row| Ok((rows::req_uuid(row, 0)?, rows::req_uuid(row, 1)?)),
    )?;
    let asset_tags: Vec<(Uuid, Uuid)> = rows::query_map(
        conn,
        "SELECT asset_id, tag_id FROM asset_tag ORDER BY asset_id",
        vec![],
        |row| Ok((rows::req_uuid(row, 0)?, rows::req_uuid(row, 1)?)),
    )?;

    // `format`, `version`, `exported_at` and `asset_count` describe the file to
    // whoever opens it; the importer reads the sections below and nothing else.
    let export = serde_json::json!({
        "format": "trove-export",
        "version": 2,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "asset_count": assets.len(),
        "assets": assets,
        "collections": collections,
        "tags": tags,
        "smart_collections": smart_collections,
        "asset_collections": asset_collections,
        "asset_tags": asset_tags,
    });
    Ok(serde_json::to_string_pretty(&export)?)
}

// ---------------------------------------------------------------------------
// Metadata restore
// ---------------------------------------------------------------------------

/// Outcome of [`Library::import_metadata`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetadataImportReport {
    /// Records whose content hash already lives in the library: the
    /// organization was merged onto the existing asset.
    pub assets_linked: u64,
    /// Records created without media (placeholders). Re-importing the file
    /// later links the blob automatically (content-addressed).
    pub assets_placeholder: u64,
    pub collections: u64,
    pub tags: u64,
    pub smart_collections: u64,
    /// Entries that could not be restored (invalid smart queries, memberships
    /// whose asset or collection is missing, …).
    pub skipped: u64,
}

/// Outcome of [`Library::export_media_package`].
#[derive(Debug, Clone, PartialEq)]
pub struct MediaExportReport {
    /// The package directory written.
    pub path: PathBuf,
    /// Blobs copied.
    pub files: u64,
    /// Sum of copied blob sizes.
    pub bytes: u64,
}

/// Outcome of [`Library::import_media_package`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaImportReport {
    pub metadata: MetadataImportReport,
    /// Media files imported through the regular importer.
    pub imported: u64,
    /// Media files skipped by the importer (e.g. duplicates).
    pub skipped: u64,
}

#[derive(serde::Deserialize)]
struct ExportFile {
    #[serde(default)]
    assets: Vec<crate::model::Asset>,
    #[serde(default)]
    collections: Vec<crate::model::Collection>,
    #[serde(default)]
    tags: Vec<crate::model::Tag>,
    /// Raw values, parsed one at a time in the import loop: a rule tree an
    /// older or newer version wrote in a shape this build cannot read is a
    /// saved search to skip, not a reason the whole catalog fails to parse —
    /// which is what a `Vec<SmartCollection>` field would make it, since the
    /// tree is a typed [`crate::model::SmartNode`] now.
    #[serde(default)]
    smart_collections: Vec<serde_json::Value>,
    /// Membership tables. Required rather than defaulted: a file without them
    /// is not something this build's exporter writes, so it is not silently
    /// restored as a library with no memberships.
    asset_collections: Vec<(Uuid, Uuid)>,
    asset_tags: Vec<(Uuid, Uuid)>,
}

/// Insert one exported collection (its parent chain first) and record the
/// id mapping. Cycle-safe via the depth guard.
fn insert_collection_tree(
    conn: &rusqlite::Connection,
    coll: &crate::model::Collection,
    by_id: &std::collections::HashMap<Uuid, &crate::model::Collection>,
    map: &mut std::collections::HashMap<Uuid, Uuid>,
    report: &mut MetadataImportReport,
    depth: usize,
) -> Option<Uuid> {
    if let Some(existing) = map.get(&coll.id) {
        return Some(*existing);
    }
    if depth > 32 {
        return None;
    }
    let parent_new = coll
        .parent_id
        .and_then(|pid| by_id.get(&pid).copied())
        .and_then(|parent| insert_collection_tree(conn, parent, by_id, map, report, depth + 1));
    let created = collections::create(
        conn,
        &crate::model::NewCollection {
            parent_id: parent_new,
            name: coll.name.clone(),
            position: coll.position,
        },
    )
    .ok()?;
    // A restore carries the folder's look too: creation has no appearance
    // parameter (a look is something added later), so it is written here —
    // and a field that is not threaded through this call silently does not
    // survive an export/import round trip.
    if !coll.appearance.is_plain() {
        let _ = collections::set_appearance(conn, created.id, &coll.appearance);
    }
    map.insert(coll.id, created.id);
    report.collections += 1;
    Some(created.id)
}

/// Recursively collect files below `dir`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

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
    undo: SharedUndoStack,
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
            undo: SharedUndoStack::with_cap(config.undo_cap()),
            tasks: std::sync::Arc::new(tasks),
            interrupted,
            text_index,
            vector_index: std::cell::RefCell::new(None),
        };
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

#[cfg(test)]
mod tests;
