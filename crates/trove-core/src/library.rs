//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::history::undo::{self, Op, OpAction, OpDesc, SharedUndoStack};
use crate::media;
use crate::model::AssetLocation;
use crate::services::collect;
use crate::store::{Store, assets, batch, collections, rows, smart, smart_collections, tags};

/// Serialize the whole metadata catalog of `store` (assets, collections,
/// tags, smart collections) as pretty JSON. Media blobs are not included —
/// the export is a portable catalog, not a backup of the files.
pub fn export_metadata_from_store(store: &Store) -> Result<String> {
    let conn = store.conn();
    let assets = assets::query(conn, &crate::model::AssetQuery::default())?.items;
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
    #[serde(default)]
    smart_collections: Vec<crate::model::SmartCollection>,
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

    /// Write a backup snapshot of the database now (also prunes old ones).
    pub fn create_backup(&self) -> Result<std::path::PathBuf> {
        crate::services::backup::create_backup(&self.root, self.store.conn())
    }

    /// Backup snapshots of this library, oldest first.
    pub fn list_backups(&self) -> Vec<std::path::PathBuf> {
        crate::services::backup::list_backups(&self.root)
    }

    /// Library statistics for the settings dashboard.
    pub fn stats(&self) -> Result<crate::store::stats::LibraryStats> {
        crate::store::stats::library_stats(self.store.conn())
    }

    fn reconcile_search_index(&self) -> Result<()> {
        let conn = self.store.conn();
        let assets_total = rows::query_count(conn, "SELECT COUNT(*) FROM assets", vec![])?;
        if self.text_index.num_docs() < assets_total as u64 {
            // Fresh / wiped / outdated index: re-enqueue everything; the
            // queue dedupes and the drain rebuilds incrementally.
            rows::execute(
                conn,
                "INSERT OR IGNORE INTO search_queue(asset_id, deleted) SELECT id, 0 FROM assets",
                vec![],
            )?;
        }
        self.drain_search_queue()
    }

    /// The live text index (passed to browse / smart-rule evaluation).
    pub fn text_index(&self) -> &crate::search::TextIndex {
        &self.text_index
    }

    /// How many outbox rows are waiting to be flushed into the text index — the
    /// gap between what the database holds and what a search can see.
    pub fn pending_index_count(&self) -> Result<u64> {
        crate::search::pending_count(self.store.conn())
    }

    /// Flush the search_queue outbox into the Tantivy index: upsert rows
    /// whose assets still exist, drop documents for purged ones. Cheap when
    /// the queue is empty (one small SELECT); batches of 500 per commit.
    pub fn drain_search_queue(&self) -> Result<()> {
        crate::search::drain(self.store.conn(), &self.text_index)
    }

    /// Rebuild the text index from scratch: wipe the documents, re-enqueue
    /// every asset and drain. Returns the number of indexed documents.
    pub fn rebuild_text_index(&self) -> Result<u64> {
        self.text_index.wipe()?;
        let conn = self.store.conn();
        rows::execute(
            conn,
            "INSERT OR IGNORE INTO search_queue(asset_id, deleted) SELECT id, 0 FROM assets",
            vec![],
        )?;
        self.drain_search_queue()?;
        Ok(self.text_index.num_docs())
    }

    /// An in-memory library whose blobs and cache live under `root` (tests).
    /// The cache gets a subdirectory of its own so tests exercise the same
    /// two-root split the app runs with.
    pub fn open_in_memory(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let cache = root.join("cache");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&cache)?;
        let store = Store::in_memory()?;
        Ok(Self {
            text_index: crate::search::TextIndex::in_ram()?,
            store,
            root,
            cache,
            undo: SharedUndoStack::with_cap(crate::history::undo::DEFAULT_UNDO_CAP),
            tasks: std::sync::Arc::new(crate::tasks::TaskManager::new()),
            interrupted: Vec::new(),
            vector_index: std::cell::RefCell::new(None),
        })
    }

    /// Jobs that were running when the previous process exited, as the task
    /// journal recorded them.
    ///
    /// Surfaced, not resumed: a row carries what the job was and how far it
    /// got, but re-running it needs the inputs that started it, and those live
    /// only in this session's controller. So the panel can tell the user their
    /// import stopped at 320 of 1200 — starting another one is their decision,
    /// not something the library can infer.
    pub fn interrupted_tasks(&self) -> &[crate::store::task_journal::JournalEntry] {
        &self.interrupted
    }

    /// Clear the interrupted list once the user has seen it, so the notice does
    /// not return on every repaint of the panel.
    pub fn clear_interrupted_tasks(&mut self) {
        self.interrupted.clear();
    }

    /// The background task manager. One running job per kind; progress and
    /// lifecycle events are polled from the UI side.
    pub fn tasks(&self) -> &crate::tasks::TaskManager {
        &self.tasks
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// The data root: database, `library.json`, backups, `media/` blobs.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cache root: thumbnails and the full-text index. Every file under
    /// it is derived from the database and the linked originals, so wiping the
    /// directory costs a rebuild and nothing else. Never the same directory as
    /// [`Library::root`].
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// Absolute path of a library-relative path (e.g. a stored `rel_path`).
    pub fn resolve(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The stored record behind `id`, or `None` when the asset is gone.
    ///
    /// The one read every preview, cell and dialog starts from. Handing callers
    /// a connection to make this query themselves is what let a preview decide
    /// its own error policy — three of them silently mapped the failure to
    /// "no asset", which looks exactly like a deleted one.
    pub fn asset(&self, id: Uuid) -> Result<Option<crate::model::Asset>> {
        assets::get(self.store.conn(), id)
    }

    /// The records for `ids`, in no particular order.
    pub fn assets_by_ids(&self, ids: &[Uuid]) -> Result<Vec<crate::model::Asset>> {
        assets::by_ids(self.store.conn(), ids)
    }

    /// One page of the library answering `query`, with the total it matches.
    pub fn query_assets(
        &self,
        query: &crate::model::AssetQuery,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        assets::query(self.store.conn(), query)
    }

    /// Every file extension present in the library, most common first — the
    /// choices the extension filter offers.
    pub fn distinct_exts(&self) -> Result<Vec<String>> {
        assets::distinct_exts(self.store.conn())
    }

    /// The folders linked assets were imported from, with how many each holds —
    /// the rows of the folder browser.
    pub fn source_folders(&self) -> Result<Vec<(String, u64)>> {
        assets::source_folders(self.store.conn())
    }

    /// The 3D look `asset` was last left in, `Ok(None)` while it still uses the
    /// app-wide default.
    pub fn model_look(
        &self,
        asset: Uuid,
    ) -> Result<Option<crate::media::height_color::StoredLook>> {
        crate::store::model_look::get(self.store.conn(), asset)
    }

    /// Remember `look` as `asset`'s own, so reopening the model puts the colours
    /// back the way they were left.
    pub fn set_model_look(
        &self,
        asset: Uuid,
        look: &crate::media::height_color::StoredLook,
    ) -> Result<()> {
        crate::store::model_look::set(self.store.conn(), asset, look)
    }

    /// How many live images carry a visual signature, out of how many could: the
    /// coverage figure the settings dialog reports before offering a backfill.
    pub fn visual_signature_counts(&self) -> Result<(u64, u64)> {
        crate::store::visual_search::signature_counts(self.store.conn())
    }

    /// The live image assets that carry no visual signature yet.
    pub fn assets_needing_signature(&self) -> Result<Vec<Uuid>> {
        crate::store::visual_search::assets_needing_signature(&self.store)
    }

    /// Mine and store one asset's visual signature. `Ok(false)` is a skipped
    /// asset — the file is gone or decodes to nothing — not a failed call.
    pub fn compute_visual_signature(&self, asset_id: Uuid) -> Result<bool> {
        crate::store::visual_search::compute_and_store_signature(&self.store, &self.root, asset_id)
    }

    /// The database file this library's records live in.
    ///
    /// Named here rather than spelled out at each call site, because the file
    /// name is the library's own layout, not the caller's business. A second
    /// connection — a benchmark running raw SQL off the UI thread, a maintenance
    /// pass — opens this path; WAL is what makes two handles over one file safe.
    pub fn db_path(&self) -> PathBuf {
        self.root.join("library.db")
    }

    /// The real file behind `id`: the in-library blob for stored assets, the
    /// linked original (recorded at import) for linked ones. `None` when the
    /// record is missing or the file no longer exists.
    pub fn asset_file(&self, id: Uuid) -> Option<std::path::PathBuf> {
        let asset = assets::get(self.store.conn(), id).ok().flatten()?;
        let path = match asset.location() {
            crate::model::AssetLocation::Stored { rel_path } => self.root.join(rel_path),
            crate::model::AssetLocation::Linked { source_path } => {
                std::path::PathBuf::from(source_path)
            }
            // `None` here means "there is no file to hand over", which is true
            // of both remaining states — but it is true for two different
            // reasons, and callers that want to say which one read
            // `asset.location()` themselves rather than guessing from this.
            crate::model::AssetLocation::Placeholder | crate::model::AssetLocation::Unrecorded => {
                return None;
            }
        };
        path.is_file().then_some(path)
    }

    /// The subset of `paths` the library does not hold yet.
    ///
    /// Keyed on file name plus size — the same loose rule the collect import
    /// skips on ([`assets::known_key`]) — so it can answer "is a drain worth
    /// starting?" without staging a single file. The collect inbox keeps its
    /// files (they are linked, not copied), so most wake-ups over it are
    /// directories whose whole contents are already assets.
    pub fn unimported_paths(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        assets::unimported_paths(self.store.conn(), paths)
    }

    /// Open the file behind `id` with an external application.
    ///
    /// `None` hands the file to the system default program for its type;
    /// `Some(app_path)` opens it with that specific application. Returns the
    /// resolved path on success so callers can report which file was opened.
    pub fn open_in_external(
        &self,
        id: Uuid,
        app_path: Option<&Path>,
    ) -> Result<std::path::PathBuf> {
        let path = self
            .asset_file(id)
            .ok_or(crate::error::Error::NotFound("asset file"))?;
        let target = match app_path {
            Some(p) => crate::services::open_external::OpenTarget::With(p),
            None => crate::services::open_external::OpenTarget::Default,
        };
        crate::services::open_external::open(&path, target)
            .map_err(|e| crate::error::Error::Io(std::io::Error::other(e)))?;
        Ok(path)
    }

    /// Import sources by **linking** them: the files stay where the user keeps
    /// them and the records point at those paths. This is what every
    /// user-facing import does — the library holds no copy of the user's
    /// media, only what it derived from it.
    /// Imported assets are added directly to "All Assets" unless a target
    /// collection is specified. Smart collections automatically capture
    /// matching assets via their rules.
    pub fn link_files(
        &self,
        sources: &[PathBuf],
        into_collection: Option<Uuid>,
    ) -> Result<media::import::ImportReport> {
        media::import::import_files(
            &self.store,
            &self.root,
            &self.cache,
            sources,
            media::import::ImportStorage::Link,
            into_collection,
        )
    }

    /// Import sources by **copying** them into the library's own store. Only
    /// for content Trove owns and is about to delete or overwrite — the
    /// extraction directory of a media package, above all. Linking a file that
    /// is about to disappear would leave the asset pointing at nothing.
    pub fn import_into_store(
        &self,
        sources: &[PathBuf],
        into_collection: Option<Uuid>,
    ) -> Result<media::import::ImportReport> {
        media::import::import_files(
            &self.store,
            &self.root,
            &self.cache,
            sources,
            media::import::ImportStorage::Copy,
            into_collection,
        )
    }

    /// Full-text search across live assets, ordered by relevance. `q` narrows
    /// the ranked set by kind / collection / tags / favorite.
    pub fn search_assets(
        &self,
        text: &str,
        q: &crate::model::AssetQuery,
    ) -> super::error::Result<crate::model::Page<crate::model::Asset>> {
        let prof = std::env::var_os("TROVE_PROFILE_QUERY").is_some();
        let t0 = std::time::Instant::now();
        let conn = self.store.conn();
        // The same grammar the desktop box speaks, so `trove search` and the
        // grid agree: qualifiers filter, terms rank.
        let expr = crate::search::expression::parse(text).into_expression();
        let has_terms = expr.groups.iter().any(|g| !g.atoms.is_empty());
        let mut q = q.clone();
        if !expr.filters.is_empty() {
            q.conditions = crate::model::QueryCondition::fold(
                std::mem::take(&mut q.conditions)
                    .into_iter()
                    .chain(expr.filters.iter().cloned())
                    .collect(),
            );
        }
        if !has_terms {
            // Nothing to rank. Qualifiers alone are a filtered listing, so
            // they still answer; a box holding only syntax characters has
            // neither, which is the empty answer the old splitter gave for an
            // empty box and stays the safe one.
            if q.conditions.is_empty() {
                return Ok(crate::model::Page::new(0, Vec::new()));
            }
            return assets::query(conn, &q);
        }
        // Pending outbox rows flush before the lookup, so a just-committed
        // mutation is visible to the same search.
        self.drain_search_queue()?;
        let t_drain = t0.elapsed();
        // The same pool sizing the grid uses: a query that is about to be
        // filtered down is gathered wider, because the intersection below runs
        // *after* the cap and can otherwise drop a match on the floor.
        let (candidates, ran_out) =
            self.text_index
                .pool_for(text, &expr, None, q.rejects_rows())?;
        let t_index = t0.elapsed();
        // Free text is entirely the index's business; `q` only carries the
        // structural filters, so it goes straight into the SQL intersection.
        let (total, ids) = assets::rank_intersect(conn, &candidates, &q)?;
        let t_rank = t0.elapsed();
        let page = assets::page_assets(&ids, &q, conn)?;
        let t_page = t0.elapsed();
        // The whole query against the slow-query threshold; the drain inside
        // it warns separately through the outbox's own slow-drain log.
        crate::metrics::note_query(t_page);
        if prof {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "[q] cand={} hits={} drain={:.2} index={:.2} rank={:.2} page={:.2}",
                candidates.len(),
                total,
                ms(t_drain),
                ms(t_index - t_drain),
                ms(t_rank - t_index),
                ms(t_page - t_rank),
            );
        }
        let mut result = crate::model::Page::new(total, page);
        // The pool ran out, so `total` is a floor: the library holds at least
        // this many matches, possibly more the caller never saw.
        result.truncated = ran_out;
        Ok(result)
    }

    // -- AI embeddings ---------------------------------------------------------

    /// `(embedded, total)` — how many live assets carry a vector under
    /// `model`, out of all live assets. The settings page's coverage line.
    pub fn embedding_coverage(&self, model: &str) -> Result<(u64, u64)> {
        crate::store::embeddings::coverage(
            self.store.conn(),
            model,
            crate::model::EmbeddingSpace::Text,
        )
    }

    /// Delete every vector stored under `model`, across spaces — the
    /// "switched provider, start over" button. Returns rows removed.
    pub fn delete_embeddings(&self, model: &str) -> Result<u64> {
        self.vector_index.borrow_mut().take();
        crate::store::embeddings::delete_model(self.store.conn(), model)
    }

    /// Start an embedding backfill on a background thread: every live asset
    /// whose source fingerprint moved (or that has no vector yet) is
    /// embedded through `provider` and stored. One backfill at a time
    /// (mutual exclusion is per [`crate::tasks::TaskKind`]); progress and
    /// lifecycle events come off [`Self::tasks`].
    pub fn start_embedding_backfill(
        &self,
        provider: std::sync::Arc<dyn crate::ai::EmbeddingProvider>,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::embed::EmbedOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = crate::tasks::embed::EmbedOptions {
            db_path: self.root.join("library.db"),
            data_root: self.root.clone(),
            cache_root: self.cache.clone(),
        };
        let label = format!("embedding backfill ({})", provider.id());
        // Retried, and at low priority. Every way this job can fail as a whole
        // is in its opening — open the database, set the pragmas, list assets —
        // and each of those is transient when the CLI holds the same library:
        // retrying two steps later gets a lock rather than a dead job. Past that
        // point a per-asset failure is recorded and the run continues, so
        // retrying cannot re-encode what already has an embedding.
        //
        // Low priority because it is a backfill: the user asked for it once and
        // it outranks nothing they are waiting on.
        self.tasks.start_with_retry_and_priority(
            crate::tasks::TaskKind::EmbeddingBackfill,
            label,
            crate::tasks::RetryPolicy::times(2),
            crate::tasks::TaskPriority::Low,
            move || {
                let options = options.clone();
                let provider = provider.clone();
                Box::new(move |ctx| crate::tasks::embed::run(&options, provider.as_ref(), ctx))
            },
        )
    }

    // -- image sequences -----------------------------------------------------

    /// Group `ids` into one image sequence at `fps` frames per second.
    ///
    /// The frames stay ordinary assets — nothing is copied, moved or rewritten
    /// — and this records only which of them form a run and in what order, so
    /// the listing rule can hide every frame but the first. Every refusal names
    /// the thing that went wrong rather than reporting a count: fewer than
    /// three frames, a frame already in another run, a selection spanning more
    /// than one directory, or frames whose dimensions disagree (a run of mixed
    /// sizes animates as a flicker, so it is a mistake rather than a shot).
    ///
    /// Not recorded on the undo stack, and it does not need to be: dissolving
    /// removes the group rows and touches nothing else, so one click reverses
    /// exactly what this did.
    pub fn create_sequence(&self, ids: &[Uuid], fps: f64) -> Result<Uuid> {
        crate::store::sequences::create(self.store.conn(), ids, fps)
    }

    /// Dissolve the named sequences, leaving their frames as the individual
    /// assets they were before grouping.
    pub fn dissolve_sequences(&self, ids: &[Uuid]) -> Result<usize> {
        crate::store::sequences::dissolve(self.store.conn(), ids)
    }

    /// Dissolve every sequence that has one of `asset_ids` as a frame.
    ///
    /// The selection the user makes is of *assets*, and a run is identified by
    /// its own id, so this is the shape the grid's context menu needs: select
    /// any frame — the visible card or a hidden member — and its whole run goes.
    pub fn dissolve_for_assets(&self, asset_ids: &[Uuid]) -> Result<usize> {
        let conn = self.store.conn();
        let mut ids: Vec<Uuid> = Vec::new();
        for asset_id in asset_ids {
            if let Some(m) = crate::store::sequences::membership(conn, *asset_id)?
                && !ids.contains(&m.sequence_id)
            {
                ids.push(m.sequence_id);
            }
        }
        crate::store::sequences::dissolve(conn, &ids)
    }

    /// Change a run's frame rate. The store's `CHECK` bounds it at 1…240 and
    /// says so in words the UI can show.
    pub fn set_sequence_fps(&self, id: Uuid, fps: f64) -> Result<()> {
        crate::store::sequences::set_fps(self.store.conn(), id, fps)
    }

    /// The run this asset is a frame of, if it is one — with its position and
    /// the full frame order, which is what a card carousel or a player walks.
    pub fn sequence_of(
        &self,
        asset_id: Uuid,
    ) -> Result<Option<crate::store::sequences::Membership>> {
        crate::store::sequences::membership(self.store.conn(), asset_id)
    }

    // -- AI analysis ---------------------------------------------------------

    /// Start a multimodal analysis run on a background thread: every live
    /// asset whose fingerprint does not already describe this run is handed
    /// to `provider`, and the description / tags / rating it returns are
    /// written back. Words the library does not have yet are filed under the
    /// configured parent tag.
    ///
    /// One run at a time (mutual exclusion is per [`crate::tasks::TaskKind`]);
    /// progress and lifecycle events come off [`Self::tasks`].
    pub fn start_ai_analysis(
        &self,
        provider: std::sync::Arc<dyn crate::ai::vendor::VendorAdapter>,
        request: crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::ai_analysis::AiAnalysisOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = self.ai_analysis_options(&request);
        let label = format!("ai analysis ({})", provider.model_version());
        // Retried, and cheap to retry: the run is idempotent through the record
        // it writes per asset (see `tasks::ai_analysis`'s module doc — "a second
        // run skips every asset whose fingerprint already matches, which makes
        // re-running free"). So a retry after the job died re-asks nothing that
        // was already answered, and the only failures that reach here are the
        // opening ones — a locked database, unreadable vocabulary — which are
        // exactly the transient kind.
        self.tasks.start_with_retry(
            crate::tasks::TaskKind::AiAnalysis,
            label,
            crate::tasks::RetryPolicy::times(2),
            move || {
                let options = options.clone();
                let provider = provider.clone();
                Box::new(move |ctx| {
                    crate::tasks::ai_analysis::run(&options, provider.as_ref(), ctx)
                })
            },
        )
    }

    /// Detach everything a previous analysis run added.
    ///
    /// The undo stack below is in memory and belongs to whichever process
    /// filled it, so a background run cannot lean on it; the record the run
    /// writes onto each asset instead is what this reads. No provider is
    /// involved — taking tags back asks no model anything. Descriptions and
    /// ratings are left in place: their previous values are not recorded.
    pub fn start_ai_analysis_undo(
        &self,
        request: crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::ai_analysis::UndoOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = self.ai_analysis_options(&request);
        // Retried too, because taking tags back is a repair: detaching a tag
        // that is already detached and clearing a marker that is already clear
        // both do nothing, so a second attempt cannot overshoot.
        self.tasks.start_with_retry(
            crate::tasks::TaskKind::AiAnalysis,
            "ai analysis undo",
            crate::tasks::RetryPolicy::times(2),
            move || {
                let options = options.clone();
                Box::new(move |ctx| crate::tasks::ai_analysis::undo(&options, ctx))
            },
        )
    }

    /// The settings a run would use, resolved against this library's files
    /// and the stored analysis configuration.
    ///
    /// Public so a caller can show what is about to happen — and so a dry run
    /// and the real run agree on exactly which assets are in scope.
    pub fn ai_analysis_options(
        &self,
        request: &crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> crate::tasks::ai_analysis::AiAnalysisOptions {
        let config = crate::config::AppConfig::load();
        let analysis = config.ai_analysis.clone().unwrap_or_default();
        crate::tasks::ai_analysis::AiAnalysisOptions::resolve(
            request,
            self.root.join("library.db"),
            self.root.clone(),
            self.cache.clone(),
            &analysis,
            config.language.as_deref(),
        )
    }

    /// Pure semantic search: embed `query` with `provider`, score the model's
    /// stored vectors by cosine, and narrow the top candidates with `q`'s
    /// structural filters — the same rank-intersect-then-page pipeline the
    /// full-text search uses. An empty query is an empty page, not a scan.
    ///
    /// The workspace does **not** go through here: its search is hybrid,
    /// fusing the text and vector rankings with
    /// [`crate::search::vector::reciprocal_rank_fusion`] (see
    /// `store::browse`), which needs no provider at query time because the
    /// app fetches the query vector ahead of the call. This entry point is
    /// the "vectors only, no text leg" answer — the natural backing for a
    /// mode switch or a CLI query, and the reference for what the fused
    /// ranking started from.
    pub fn semantic_search(
        &self,
        provider: &dyn crate::ai::EmbeddingProvider,
        query: &str,
        q: &crate::model::AssetQuery,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        let started = std::time::Instant::now();
        let query = query.trim();
        if query.is_empty() {
            return Ok(crate::model::Page::new(0, Vec::new()));
        }
        // One query vector, from the same provider that produced the rows —
        // the model identity is the whole comparability contract.
        let vector = provider
            .embed_texts(std::slice::from_ref(&query.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::error::Error::Validation(
                    "embedding provider returned no vector for the query".into(),
                )
            })?;

        let conn = self.store.conn();
        let index = self.cached_vector_index(provider.id(), provider.asset_space());
        let candidates =
            index.search(conn, &vector, crate::search::vector::VECTOR_CANDIDATE_CAP)?;
        let ranked: Vec<Uuid> = candidates.into_iter().map(|m| m.asset_id).collect();
        let (total, ids) = assets::rank_intersect(conn, &ranked, q)?;
        let page = assets::page_assets(&ids, q, conn)?;
        crate::metrics::note_vector_search();
        crate::metrics::note_query(started.elapsed());
        Ok(crate::model::Page::new(total, page))
    }

    /// The cached in-memory index for one model+space, rebuilt when the
    /// identity changes. Drift *inside* one model (a backfill finishing, an
    /// asset deleted) is the index's own fingerprint check, not this cache's.
    ///
    /// Public because the workspace's hybrid ranking needs exactly the index
    /// [`Self::semantic_search`] uses, while holding a query vector fetched
    /// earlier rather than a provider.
    pub fn cached_vector_index(
        &self,
        model: &str,
        space: crate::model::EmbeddingSpace,
    ) -> crate::search::vector::VectorIndex {
        let mut cached = self.vector_index.borrow_mut();
        let stale = match cached.as_ref() {
            Some((cached_model, cached_space, _)) => {
                cached_model != model || *cached_space != space
            }
            None => true,
        };
        if stale {
            *cached = Some((
                model.to_string(),
                space,
                crate::search::vector::VectorIndex::new(model, space),
            ));
        }
        match cached.as_ref() {
            Some((_, _, index)) => index.clone(),
            None => unreachable!("populated immediately above"),
        }
    }

    // -- smart collections ----------------------------------------------------

    /// Create a smart collection from a validated `NewSmartCollection`.
    /// The condition tree is compiled once here (runnability against the
    /// current schema is a storage concern, so it is not part of the model's
    /// own validation).
    pub fn create_smart_collection(
        &self,
        input: &crate::model::NewSmartCollection,
    ) -> Result<crate::model::SmartCollection> {
        input.validate()?;
        smart::validate_json(&input.query)?;
        smart_collections::create(self.store.conn(), input)
    }

    pub fn list_smart_collections(&self) -> Result<Vec<crate::model::SmartCollection>> {
        smart_collections::list(self.store.conn())
    }

    pub fn get_smart_collection(&self, id: Uuid) -> Result<Option<crate::model::SmartCollection>> {
        smart_collections::get(self.store.conn(), id)
    }

    pub fn rename_smart_collection(&self, id: Uuid, name: &str) -> Result<()> {
        smart_collections::rename(self.store.conn(), id, name)
    }

    pub fn delete_smart_collection(&self, id: Uuid) -> Result<()> {
        smart_collections::delete(self.store.conn(), id)
    }

    /// Move a smart collection under `new_parent` (another smart collection
    /// or a regular collection) at `position`; cycles are refused.
    pub fn move_smart_collection(
        &self,
        id: Uuid,
        new_parent: Option<Uuid>,
        position: i64,
    ) -> Result<()> {
        let conn = self.store.conn();
        if smart_collections::get(conn, id)?.is_none() {
            return Err(crate::Error::NotFound("smart_collection"));
        }
        smart_collections::move_to(conn, id, new_parent, position)
    }

    /// Reorder a smart collection to `position` among its siblings, shifting
    /// others to make room. The parent is unchanged.
    pub fn reorder_smart_collection(&self, id: Uuid, position: i64) -> Result<()> {
        let conn = self.store.conn();
        smart_collections::reorder_to(conn, id, position)
    }

    /// How many live assets `node` matches: the badge beside each saved search.
    ///
    /// The rule comes in rather than an id, because the sidebar holds every
    /// smart collection already — looking one row up per badge would be a query
    /// per pixel of chrome.
    pub fn count_smart_rule(&self, node: &crate::model::SmartNode) -> Result<u64> {
        let page = smart::evaluate(self.store.conn(), Some(self.text_index()), node, None, 0)?;
        Ok(page.total)
    }

    /// Freeze a browse into a session: the answer set is decided once here, so
    /// the pages taken from it afterwards cannot disagree about what matches.
    pub fn browse_snapshot(
        &self,
        browse: &crate::store::BrowseContext,
        vector: Option<&crate::search::vector::VectorIndex>,
        count_total: bool,
    ) -> Result<crate::store::BrowseSession> {
        browse.snapshot(self.store.conn(), self.text_index(), vector, count_total)
    }

    /// One window of rows from a frozen browse, in its order.
    pub fn browse_page(
        &self,
        session: &crate::store::BrowseSession,
        offset: usize,
        window: Option<usize>,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        session.page(self.store.conn(), self.text_index(), offset, window)
    }

    /// How many assets each filter choice would return, for the session's whole
    /// answer set rather than the page on screen.
    pub fn browse_facets(
        &self,
        session: &crate::store::BrowseSession,
    ) -> Result<crate::store::facets::FacetCounts> {
        session.compute_facets(self.store.conn())
    }

    /// Evaluate a stored smart collection live, materialising the matching
    /// assets as a paged list. `kind` / `favorite` are extra grid filters
    /// AND-ed onto the tree (the toolbar filters compose with smart
    /// collections too).
    pub fn evaluate_smart_collection(
        &self,
        id: Uuid,
        page: smart::SmartPage,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        let conn = self.store.conn();
        let Some(smart_collection) = smart_collections::get(conn, id)? else {
            return Err(crate::Error::NotFound("smart_collection"));
        };
        let node = smart::node_from_json(&smart_collection.query)?;
        let ids = smart::evaluate_filtered(conn, Some(self.text_index()), &node, page)?;
        let items = assets::by_ids(conn, &ids.items)?;
        Ok(crate::model::Page::new(ids.total, items))
    }

    /// Permanently delete one asset. The database row (and its collection /
    /// tag memberships) is removed; the blob file and thumbnail are deleted
    /// once no other asset references the same content hash, and a file Trove
    /// itself put in the inbox goes with it (see [`Self::purge_assets`]).
    /// A target that is already gone is an error, not a silent no-op.
    pub fn purge_asset(&self, asset_id: Uuid) -> Result<()> {
        if assets::get(self.store.conn(), asset_id)?.is_none() {
            return Err(crate::Error::NotFound("asset"));
        }
        self.purge_assets(std::slice::from_ref(&asset_id))?;
        Ok(())
    }

    /// Permanently delete every trashed asset. Returns the number removed.
    pub fn empty_trash(&self) -> Result<u64> {
        self.empty_trash_against(&collect::inbox_dir())
    }

    /// [`empty_trash`] with the inbox spelled out (tests).
    ///
    /// One batch through [`Self::purge_assets_against`], so an asset emptied
    /// from the trash is treated exactly like one deleted outright — this used
    /// to be a second copy of the purge rules that could drift from the first.
    pub(crate) fn empty_trash_against(&self, inbox: &Path) -> Result<u64> {
        let page = assets::query(
            self.store.conn(),
            &crate::model::AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )?;
        let ids: Vec<Uuid> = page.items.iter().map(|asset| asset.id).collect();
        Ok(self.purge_assets_against(&ids, inbox)?.purged)
    }

    // -- batch asset mutations ------------------------------------------------
    //
    // Every method here records an invertible `undo::Op` (see [`crate::undo`]).
    // Destructive operations that cannot be inverted — purge, empty trash,
    // imports, tag/collection deletes — are deliberately not recorded.

    /// Batch-rename the titles of `ids` (in display order).
    ///
    /// `{n}` in `pattern` expands to the running index starting at
    /// `start_number`; `{name}` expands to the original file stem. Recorded
    /// as one undoable operation.
    pub fn batch_rename(&self, ids: &[Uuid], pattern: &str, start_number: u32) -> Result<u64> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(crate::Error::Validation(
                "rename pattern must not be empty".into(),
            ));
        }
        let conn = self.store.conn();
        let mut before: Vec<(Uuid, Option<String>)> = Vec::with_capacity(ids.len());
        let mut after: Vec<(Uuid, Option<String>)> = Vec::with_capacity(ids.len());
        let mut n = start_number;
        for id in ids {
            let Some(asset) = assets::get(conn, *id)? else {
                continue;
            };
            let stem = asset.file_stem();
            let title = pattern
                .replace("{n}", &n.to_string())
                .replace("{name}", &stem);
            before.push((*id, asset.title.clone()));
            after.push((*id, Some(title)));
            n += 1;
        }
        let count = after.len() as u64;
        if count == 0 {
            return Ok(0);
        }
        let desc = OpDesc::counted(OpAction::Rename, after.len());
        for (id, title) in &after {
            assets::update(
                conn,
                *id,
                &crate::model::AssetPatch {
                    title: Some(title.clone()),
                    ..Default::default()
                },
            )?;
        }
        self.undo.record(Op::SetTitles { before, after }, desc);
        Ok(count)
    }

    /// Export a portable *media package*: `trove-export.json` (full
    /// metadata) plus a `media/` tree with every live blob. The package is a
    /// plain directory — copyable, zip-able, restorable via
    /// [`Self::import_media_package`].
    pub fn export_media_package(&self, dest_parent: &Path) -> Result<MediaExportReport> {
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let pkg = dest_parent.join(format!("trove-media-{stamp}"));
        std::fs::create_dir_all(pkg.join("media"))?;

        let json = self.export_metadata()?;
        std::fs::write(pkg.join("trove-export.json"), json)?;

        let conn = self.store.conn();
        let live = assets::query(conn, &crate::model::AssetQuery::default())?;
        let mut files = 0u64;
        let mut bytes = 0u64;
        for asset in &live.items {
            let AssetLocation::Stored { rel_path: rel } = asset.location() else {
                continue;
            };
            let src = self.root.join(&rel);
            if !src.is_file() {
                continue;
            }
            let dst = pkg.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::copy(&src, &dst)?;
            files += 1;
            bytes += asset.size_bytes;
        }
        Ok(MediaExportReport {
            path: pkg,
            files,
            bytes,
        })
    }

    /// Restore a media package created by [`Self::export_media_package`]:
    /// the metadata first (placeholders + organization), then the media
    /// files through the regular importer — content addressing matches each
    /// blob to its placeholder record, so records heal automatically.
    pub fn import_media_package(&self, pkg: &Path) -> Result<MediaImportReport> {
        let json = std::fs::read_to_string(pkg.join("trove-export.json")).map_err(|_| {
            crate::Error::Validation("not a media package (trove-export.json missing)".into())
        })?;
        let metadata = self.import_metadata(&json)?;

        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&pkg.join("media"), &mut files);
        let mut imported = 0u64;
        let mut skipped = 0u64;
        if !files.is_empty() {
            let report = self.import_into_store(&files, None)?;
            imported = report.imported_count() as u64;
            skipped = report.skipped_count() as u64;
        }
        Ok(MediaImportReport {
            metadata,
            imported,
            skipped,
        })
    }

    /// Group live assets with identical content hash. The UI offers
    /// per-group cleanup; trashing one member is ordinary (undoable) trash.
    pub fn find_duplicates(&self) -> Result<Vec<crate::store::assets::DuplicateGroup>> {
        crate::store::assets::duplicate_groups(self.store.conn())
    }

    /// Trash many assets (single atomic statement).
    pub fn trash_assets(&self, ids: &[Uuid]) -> Result<u64> {
        self.set_assets_trashed(ids, true)
    }

    /// Restore many trashed assets (single atomic statement).
    pub fn restore_assets(&self, ids: &[Uuid]) -> Result<u64> {
        self.set_assets_trashed(ids, false)
    }

    /// Favorite / unfavorite many assets (single atomic statement).
    pub fn set_assets_favorite(&self, ids: &[Uuid], favorite: bool) -> Result<u64> {
        let conn = self.store.conn();
        let before = ids
            .iter()
            .map(|id| {
                Ok((
                    *id,
                    assets::get(conn, *id)?
                        .map(|a| a.is_favorite)
                        .unwrap_or(false),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let changed = batch::set_favorite_many(conn, ids, favorite)?;
        if changed > 0 {
            self.undo.record(
                Op::SetFavorite {
                    before,
                    after: ids.iter().map(|id| (*id, favorite)).collect(),
                },
                OpDesc::counted(
                    if favorite {
                        OpAction::Favorite
                    } else {
                        OpAction::Unfavorite
                    },
                    ids.len(),
                ),
            );
        }
        Ok(changed)
    }

    /// Attach many assets to a collection (idempotent). Only the actual
    /// membership delta is recorded for undo.
    pub fn add_assets_to_collection(&self, collection_id: Uuid, ids: &[Uuid]) -> Result<u64> {
        let conn = self.store.conn();
        let target = collections::get(conn, collection_id)?.map(|c| c.name);
        let members = collections::asset_ids(conn, collection_id)?;
        let changed = batch::add_to_collection_many(conn, collection_id, ids)?;
        let added: Vec<Uuid> = ids
            .iter()
            .filter(|id| !members.contains(id))
            .copied()
            .collect();
        let desc = OpDesc::new(OpAction::AddedToCollection, target, added.len());
        if !added.is_empty() {
            self.undo.record(
                Op::MembershipAdd {
                    collection: collection_id,
                    added,
                },
                desc,
            );
        }
        Ok(changed)
    }

    /// Detach many assets from a collection. Returns the number actually
    /// removed (assets that were not members are ignored).
    pub fn remove_assets_from_collection(
        &self,
        collection_id: Uuid,
        ids: &[Uuid],
    ) -> Result<usize> {
        let conn = self.store.conn();
        let target = collections::get(conn, collection_id)?.map(|c| c.name);
        let members = collections::asset_ids(conn, collection_id)?;
        let removed: Vec<Uuid> = ids
            .iter()
            .filter(|id| members.contains(id))
            .copied()
            .collect();
        for id in &removed {
            collections::remove_asset(conn, collection_id, *id)?;
        }
        let count = removed.len();
        if count > 0 {
            self.undo.record(
                Op::MembershipRemove {
                    collection: collection_id,
                    removed,
                },
                OpDesc::new(OpAction::RemovedFromCollection, target, count),
            );
        }
        Ok(count)
    }

    /// Apply a metadata patch to one asset, recording the full pre-state so
    /// undo restores every editable column (title/description edits also
    /// re-sync the search index through `assets::update`).
    pub fn patch_asset(&self, asset_id: Uuid, patch: &crate::model::AssetPatch) -> Result<()> {
        patch.validate()?;
        let conn = self.store.conn();
        let asset = assets::get(conn, asset_id)?.ok_or(crate::Error::NotFound("asset"))?;
        let before = undo::restore_patch(&asset);
        assets::update(conn, asset_id, patch)?;
        self.undo.record(
            Op::PatchAsset {
                id: asset_id,
                before: Box::new(before),
                after: Box::new(patch.clone()),
            },
            OpDesc::new(OpAction::Edit, Some(asset.file_name), 1),
        );
        Ok(())
    }

    /// Re-point a linked asset at a moved file. The chosen file must hash to
    /// the same content hash as the one recorded at import — relinking
    /// reconnects a *moved* file, it never swaps content (import the new file
    /// instead when the original is truly gone).
    pub fn relink_asset(&self, asset_id: Uuid, new_path: &Path) -> Result<()> {
        let conn = self.store.conn();
        let asset = assets::get(conn, asset_id)?.ok_or(crate::Error::NotFound("asset"))?;
        if !asset.location().is_linked() {
            return Err(crate::Error::Validation(
                "relink requires a linked asset".into(),
            ));
        }
        if !new_path.is_file() {
            return Err(crate::Error::Validation(format!(
                "not a file: {}",
                new_path.display()
            )));
        }
        // Through the hash cache: a file the importer has already read is
        // recognised from its `stat`, and the read that relinking would do
        // otherwise is exactly the read that was already paid for.
        let (hash, _) = crate::media::hash::hash_file_cached(self.cache(), new_path)?;
        let recorded = asset.content_hash.as_deref().unwrap_or_default();
        if !hash.eq_ignore_ascii_case(recorded) {
            return Err(crate::Error::Validation(format!(
                "content mismatch: recorded content hash {recorded}, found {hash}"
            )));
        }
        let mut facts = asset.facts.clone();
        facts.source_path = Some(new_path.to_string_lossy().into_owned());
        assets::update(
            conn,
            asset_id,
            &crate::model::AssetPatch {
                facts: Some(facts),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // In-place image edits & metadata export
    // -----------------------------------------------------------------------

    /// Apply pixel edits (rotate / flip / crop, see
    /// [`media::edit::ImageEdit`]) to a batch of image assets and swap the
    /// re-encoded results in as the assets' new media content. Identity and
    /// organization (id, title, tags, collections, captured-at) survive the
    /// edit; hash, size, dimensions, thumbnail and visual fingerprint are
    /// recomputed. A *stored* asset's new content becomes a library blob; a
    /// *linked* asset's result is written back over the original file it
    /// links to — the caller (and the UI above it) owns that decision, the
    /// backend just keeps the record honest about what the file now is.
    pub fn batch_edit_images(
        &self,
        ids: &[Uuid],
        edits: &[media::edit::ImageEdit],
        jpeg_quality: u8,
    ) -> Result<BatchEditReport> {
        if edits.is_empty() {
            return Err(crate::Error::Validation("no edits requested".into()));
        }
        let mut report = BatchEditReport::default();
        for &id in ids {
            match self.edit_one(id, edits, jpeg_quality) {
                Ok(true) => report.edited += 1,
                Ok(false) => report.skipped += 1,
                Err(e) => report.failures.push((id, e.to_string())),
            }
        }
        Ok(report)
    }

    /// Edit one asset: decode, transform, re-encode, stage the new content
    /// and swap it in. `Ok(false)` marks an asset the batch skips silently.
    fn edit_one(
        &self,
        id: Uuid,
        edits: &[media::edit::ImageEdit],
        jpeg_quality: u8,
    ) -> Result<bool> {
        let conn = self.store.conn();
        let Some(asset) = assets::get(conn, id)? else {
            return Ok(false);
        };
        if asset.trashed_at.is_some() || asset.kind != crate::model::AssetKind::Image {
            return Ok(false);
        }
        let location = asset.location();
        if location.is_linked() {
            return self.edit_linked_in_place(&asset, edits, jpeg_quality);
        }
        // A placeholder has no bytes to edit; it is not an error, it is the
        // state where the user has not re-imported the file yet.
        let AssetLocation::Stored { rel_path } = location else {
            return Ok(false);
        };

        let source = self.root.join(rel_path);
        let out = media::edit::apply(&source, edits, jpeg_quality)?;

        // Park the re-encoded bytes where blob::stage expects a source, then
        // let the normal content-addressing path take over.
        let tmp = self
            .root
            .join("media")
            .join(format!(".edit-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            std::fs::write(&tmp, &out.bytes)?;
            self.replace_media_staged(id, &asset, &tmp, out.width, out.height)
        })();
        let _ = std::fs::remove_file(&tmp);
        result?;
        Ok(true)
    }

    /// Edit a *linked* asset in place: the re-encoded result overwrites the
    /// original file the record links to, and the record's content columns
    /// (hash, size, geometry) follow so the library stays honest about what
    /// that file now is. The link itself is untouched — same path, same
    /// origin. A sibling record linking the same file keeps its old hash
    /// until the integrity check meets the changed file; the rewrite is the
    /// user's explicit act, and this is its one honest consequence.
    fn edit_linked_in_place(
        &self,
        asset: &crate::model::Asset,
        edits: &[media::edit::ImageEdit],
        jpeg_quality: u8,
    ) -> Result<bool> {
        let AssetLocation::Linked { source_path } = asset.location() else {
            // No reachable original (moved, or never recorded): skip the
            // asset — relinking is the fix, not an error toast.
            return Ok(false);
        };
        let source = PathBuf::from(source_path);
        let out = media::edit::apply(&source, edits, jpeg_quality)?;
        let hash = media::hash::hash_bytes(&out.bytes);
        if asset
            .content_hash
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(&hash))
        {
            // The edits produced byte-identical content: the file on disk is
            // already what the record says.
            return Ok(true);
        }

        // Atomic replace: the bytes land on a hidden sibling first and a
        // rename moves them over the original, so a crash mid-write costs at
        // most the previous content, never a truncated file.
        let tmp = source.with_file_name(format!(
            ".{}.trove-edit-{}",
            source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file"),
            uuid::Uuid::new_v4().simple()
        ));
        let write = (|| -> Result<()> {
            std::fs::write(&tmp, &out.bytes)?;
            std::fs::rename(&tmp, &source)?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }

        let old_hash = asset.content_hash.clone().unwrap_or_default();
        assets::set_linked_media_columns(
            self.store.conn(),
            asset.id,
            &hash,
            out.bytes.len() as u64,
            Some(out.width),
            Some(out.height),
        )?;

        // The old thumbnail described content no record references anymore
        // once the last asset on that hash is gone; the new one is rebuilt
        // from the file where it lives.
        if !old_hash.is_empty() && assets::count_by_content_hash(self.store.conn(), &old_hash)? == 0
        {
            media::thumb::remove_derived(self.cache(), &old_hash);
        }
        media::thumb::regenerate(self.cache(), &hash, asset.kind, &source);
        Ok(true)
    }

    /// Swap an asset's media content for the (already transformed) file at
    /// `new_file`. The old blob is deleted once nothing else references it;
    /// thumbnail and visual fingerprint are rebuilt from the new content.
    fn replace_media_staged(
        &self,
        id: Uuid,
        asset: &crate::model::Asset,
        new_file: &Path,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let staged = media::blob::stage(new_file, self.root(), &asset.ext)?;
        let old_hash = asset.content_hash.clone().unwrap_or_default();
        let old_rel = match asset.location() {
            AssetLocation::Stored { rel_path } => Some(rel_path),
            _ => None,
        };
        if staged.content_hash.eq_ignore_ascii_case(&old_hash) {
            // The edits produced byte-identical content: the blob in place
            // is already correct.
            return Ok(());
        }

        let new_blob = self.root.join(&staged.rel_path);
        assets::set_media_columns(
            self.store.conn(),
            id,
            &staged.content_hash,
            &staged.rel_path,
            staged.size,
            Some(width),
            Some(height),
        )?;

        // Free the old content when this was the last reference to it. Its
        // derived files describe the old pixels and go with it; the new
        // content's card is regenerated below.
        if assets::count_by_content_hash(self.store.conn(), &old_hash)? == 0
            && let Some(rel) = &old_rel
        {
            self.remove_blob_file(rel);
            media::thumb::remove_derived(self.cache(), &old_hash);
        }

        // Thumbnail and visual fingerprint describe the old pixels; both
        // must follow the content to its new hash.
        media::thumb::regenerate(self.cache(), &staged.content_hash, asset.kind, &new_blob);
        crate::store::visual_search::compute_and_store_signature(&self.store, &self.root, id)?;
        Ok(())
    }

    /// Export the library metadata of `ids` as XMP sidecars next to each
    /// asset's media file (the stored blob, or the external original for
    /// linked assets). Title, description, tag names and rating are
    /// exported; trashed assets and assets without a file are skipped.
    pub fn export_xmp_sidecars(&self, ids: &[Uuid]) -> Result<XmpExportReport> {
        let conn = self.store.conn();
        let mut report = XmpExportReport::default();
        for &id in ids {
            let Some(asset) = assets::get(conn, id)? else {
                report.skipped += 1;
                continue;
            };
            if asset.trashed_at.is_some() {
                report.skipped += 1;
                continue;
            }
            let target = match asset.location() {
                AssetLocation::Stored { rel_path } => Some(self.root.join(rel_path)),
                AssetLocation::Linked { source_path } => Some(PathBuf::from(source_path)),
                AssetLocation::Placeholder | AssetLocation::Unrecorded => None,
            };
            let Some(target) = target else {
                report.skipped += 1;
                continue;
            };
            let tag_names = tags::for_asset(conn, id)?
                .into_iter()
                .map(|t| t.name)
                .collect();
            let data = crate::services::xmp::XmpData {
                title: asset.title.clone(),
                description: asset.description.clone(),
                tags: tag_names,
                rating: asset.rating,
            };
            if crate::services::xmp::write_sidecar(&target, &data).is_ok() {
                report.written += 1;
            } else {
                report.skipped += 1;
            }
        }
        Ok(report)
    }

    /// Replace one asset's whole tag group (missing tags must already exist —
    /// use [`tags::ensure_named`] at the call site first).
    pub fn set_asset_tags(&self, asset_id: Uuid, tag_ids: &[Uuid]) -> Result<()> {
        let conn = self.store.conn();
        let target = assets::get(conn, asset_id)?.map(|a| a.file_name);
        let before: Vec<Uuid> = tags::for_asset(conn, asset_id)?
            .iter()
            .map(|t| t.id)
            .collect();
        tags::set_for_asset(conn, asset_id, tag_ids)?;
        self.undo.record(
            Op::SetTags {
                asset: asset_id,
                before,
                after: tag_ids.to_vec(),
            },
            OpDesc::new(OpAction::TagSet, target, 1),
        );
        Ok(())
    }

    /// Create the named tag if missing and return it. The frontend uses this
    /// to resolve comma-separated tag names before batch attach/replace.
    pub fn ensure_tag(&self, name: &str) -> Result<crate::model::Tag> {
        tags::ensure_named(self.store.conn(), name)
    }

    /// Delete a tag outright, detaching it from every asset. Not undoable.
    pub fn delete_tag(&self, tag_id: Uuid) -> Result<()> {
        tags::delete(self.store.conn(), tag_id)
    }

    /// Create a tag under an optional parent (hierarchical tags).
    pub fn create_tag(&self, name: &str, parent: Option<Uuid>) -> Result<crate::model::Tag> {
        if let Some(pid) = parent {
            let conn = self.store.conn();
            if tags::get(conn, pid)?.is_none() {
                return Err(crate::Error::NotFound("parent tag"));
            }
        }
        tags::create(
            self.store.conn(),
            &crate::model::NewTag {
                name: name.to_string(),
                color: None,
                parent_id: parent,
            },
        )
    }

    /// Move a tag under `parent` (`None` = root). Undoable; cycles and
    /// self-parenting are rejected by the store.
    pub fn set_tag_parent(&self, tag_id: Uuid, parent: Option<Uuid>) -> Result<()> {
        let conn = self.store.conn();
        let tag = tags::get(conn, tag_id)?.ok_or(crate::Error::NotFound("tag"))?;
        let before = tag.parent_id;
        tags::move_to(conn, tag_id, parent)?;
        self.undo.record(
            Op::TagParent {
                id: tag_id,
                before,
                after: parent,
            },
            OpDesc::new(OpAction::TagMoved, Some(tag.name), 1),
        );
        Ok(())
    }

    /// Attach (`add = true`) or detach one tag on many assets, recording the
    /// per-asset tag-group delta.
    pub fn tag_assets(&self, asset_ids: &[Uuid], tag_id: Uuid, add: bool) -> Result<()> {
        let conn = self.store.conn();
        for asset_id in asset_ids {
            let target = assets::get(conn, *asset_id)?.map(|a| a.file_name);
            let before: Vec<Uuid> = tags::for_asset(conn, *asset_id)?
                .iter()
                .map(|t| t.id)
                .collect();
            let after: Vec<Uuid> = if add {
                if before.contains(&tag_id) {
                    continue;
                }
                let mut v = before.clone();
                v.push(tag_id);
                v
            } else {
                if !before.contains(&tag_id) {
                    continue;
                }
                before.iter().copied().filter(|id| *id != tag_id).collect()
            };
            tags::set_for_asset(conn, *asset_id, &after)?;
            self.undo.record(
                Op::SetTags {
                    asset: *asset_id,
                    before,
                    after,
                },
                OpDesc::new(OpAction::TagSet, target, 1),
            );
        }
        Ok(())
    }

    /// Rename a tag (search index re-synced), recording the previous name.
    pub fn rename_tag(&self, tag_id: Uuid, name: &str) -> Result<()> {
        let conn = self.store.conn();
        let tag = tags::get(conn, tag_id)?.ok_or(crate::Error::NotFound("tag"))?;
        let desc = OpDesc::new(OpAction::TagRenamed, Some(tag.name.clone()), 1);
        tags::rename(conn, tag_id, name)?;
        self.undo.record(
            Op::TagRename {
                id: tag_id,
                before: tag.name,
                after: name.to_string(),
            },
            desc,
        );
        Ok(())
    }

    /// Set (or clear) a tag's display color, recording the previous value.
    pub fn set_tag_color(&self, tag_id: Uuid, color: Option<&str>) -> Result<()> {
        let conn = self.store.conn();
        let tag = tags::get(conn, tag_id)?.ok_or(crate::Error::NotFound("tag"))?;
        tags::set_color(conn, tag_id, color)?;
        self.undo.record(
            Op::TagColor {
                id: tag_id,
                before: tag.color,
                after: color.map(|c| c.to_string()),
            },
            OpDesc::new(OpAction::TagColored, Some(tag.name), 1),
        );
        Ok(())
    }

    /// Every collection in the library.
    pub fn list_collections(&self) -> Result<Vec<crate::model::Collection>> {
        collections::list(self.store.conn())
    }

    /// The collections with no parent: the top of the sidebar tree.
    pub fn collection_roots(&self) -> Result<Vec<crate::model::Collection>> {
        collections::roots(self.store.conn())
    }

    /// The children of `parent`, or the roots when it is `None`. Both in one
    /// method because the tree asks the same question of the root level as of
    /// any folder, and `Option<Uuid>` is already the domain's "no parent".
    pub fn collection_children(
        &self,
        parent: Option<Uuid>,
    ) -> Result<Vec<crate::model::Collection>> {
        collections::children_of(self.store.conn(), parent)
    }

    /// One collection by id, `Ok(None)` when it is gone.
    pub fn collection(&self, id: Uuid) -> Result<Option<crate::model::Collection>> {
        collections::get(self.store.conn(), id)
    }

    /// The collections `asset_id` sits in.
    pub fn collections_for_asset(&self, asset_id: Uuid) -> Result<Vec<crate::model::Collection>> {
        collections::for_asset(self.store.conn(), asset_id)
    }

    /// How many assets are members of `id` itself — its own membership rows, not
    /// a subtree's total, which is what the sidebar's per-folder count means.
    pub fn count_collection_assets(&self, id: Uuid) -> Result<u64> {
        collections::count_assets(self.store.conn(), id)
    }

    /// Create a collection from `input`.
    pub fn create_collection(
        &self,
        input: &crate::model::NewCollection,
    ) -> Result<crate::model::Collection> {
        collections::create(self.store.conn(), input)
    }

    /// Replace a collection's look (icon, colour). Not recorded for undo: an
    /// appearance is decoration on a row that still exists, and the picker
    /// previews the change before committing it, so a stack entry would record
    /// every hover.
    pub fn set_collection_appearance(
        &self,
        id: Uuid,
        appearance: &crate::model::Appearance,
    ) -> Result<()> {
        collections::set_appearance(self.store.conn(), id, appearance)
    }

    /// The same for a smart collection, for the same reason.
    pub fn set_smart_collection_appearance(
        &self,
        id: Uuid,
        appearance: &crate::model::Appearance,
    ) -> Result<()> {
        smart_collections::set_appearance(self.store.conn(), id, appearance)
    }

    /// Replace a smart collection's rule.
    pub fn set_smart_collection_query(&self, id: Uuid, query: &serde_json::Value) -> Result<()> {
        smart_collections::update_query(self.store.conn(), id, query)
    }

    /// Every tag, name-ordered.
    pub fn list_tags(&self) -> Result<Vec<crate::model::Tag>> {
        tags::list(self.store.conn())
    }

    /// The tags an asset carries.
    pub fn tags_for_asset(&self, asset_id: Uuid) -> Result<Vec<crate::model::Tag>> {
        tags::for_asset(self.store.conn(), asset_id)
    }

    /// How many live assets carry each tag — the number beside every row of the
    /// tag panel.
    pub fn tag_counts(&self) -> Result<std::collections::HashMap<Uuid, u64>> {
        tags::counts_by_tag(self.store.conn())
    }

    /// `tag_id` and every tag nested under it.
    pub fn tag_subtree_ids(&self, tag_id: Uuid) -> Result<Vec<Uuid>> {
        tags::subtree_ids(self.store.conn(), tag_id)
    }

    /// How many viewed assets are still live — the recently-viewed row's badge.
    /// Trashed ones drop out of the count without leaving the table, which is
    /// why this is not `store::view_history`'s row count.
    pub fn viewed_count(&self) -> Result<u64> {
        crate::store::view_history::live_count(self.store.conn())
    }

    /// Note that `asset_id` was opened, so the recently-viewed list can rank it.
    pub fn record_view(&self, asset_id: Uuid) -> Result<()> {
        crate::store::view_history::record(self.store.conn(), asset_id)
    }

    /// Empty the recently-viewed list.
    pub fn clear_view_history(&self) -> Result<()> {
        crate::store::view_history::clear(self.store.conn())
    }

    /// Rename a collection, recording the previous name for undo.
    /// Rename a collection, recording the previous name.
    pub fn rename_collection(&self, collection_id: Uuid, name: &str) -> Result<()> {
        let conn = self.store.conn();
        let collection =
            collections::get(conn, collection_id)?.ok_or(crate::Error::NotFound("collection"))?;
        let desc = OpDesc::new(
            OpAction::CollectionRenamed,
            Some(collection.name.clone()),
            1,
        );
        collections::rename(conn, collection_id, name)?;
        self.undo.record(
            Op::CollectionRename {
                id: collection_id,
                before: collection.name,
                after: name.to_string(),
            },
            desc,
        );
        Ok(())
    }

    /// Move a collection under `new_parent` at `position`, recording the
    /// previous placement.
    pub fn move_collection(
        &self,
        collection_id: Uuid,
        new_parent: Option<Uuid>,
        position: i64,
    ) -> Result<()> {
        let conn = self.store.conn();
        let c =
            collections::get(conn, collection_id)?.ok_or(crate::Error::NotFound("collection"))?;
        collections::move_to(conn, collection_id, new_parent, position)?;
        self.undo.record(
            Op::CollectionMove {
                id: collection_id,
                before: (c.parent_id, c.position),
                after: (new_parent, position),
            },
            OpDesc::new(OpAction::CollectionMoved, Some(c.name), 1),
        );
        Ok(())
    }

    /// Delete a managed collection, memberships and all.
    ///
    /// Not recorded for undo, same as [`Self::delete_smart_collection`]: a
    /// collection is its membership list, so undoing a delete means re-creating
    /// rows the undo stack has no snapshot of. The callers that do record —
    /// [`Self::rename_collection`], [`Self::move_collection`] — only ever put
    /// back a field of a row that still exists.
    pub fn delete_collection(&self, id: Uuid) -> Result<()> {
        collections::delete(self.store.conn(), id)
    }

    /// Undo the most recent recorded mutation. Returns `false` when there is
    /// nothing to undo.
    pub fn undo(&self) -> Result<bool> {
        self.undo.undo(self.store.conn())
    }

    /// Redo the most recently undone mutation. Returns `false` when there is
    /// nothing to redo.
    pub fn redo(&self) -> Result<bool> {
        self.undo.redo(self.store.conn())
    }

    pub fn undo_len(&self) -> usize {
        self.undo.undo_len()
    }

    pub fn redo_len(&self) -> usize {
        self.undo.redo_len()
    }

    /// Descriptions of the last `n` undoable operations, most recent first
    /// (status bar).
    pub fn undo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.undo.undo_entries(n)
    }

    /// Descriptions of the last `n` redoable operations, next-first.
    pub fn redo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.undo.redo_entries(n)
    }

    /// Undo up to `steps` operations in sequence; returns how many were
    /// applied (stops early when the history runs out or one fails).
    pub fn undo_steps(&self, steps: usize) -> Result<usize> {
        self.undo.undo_steps(steps, self.store.conn())
    }

    fn set_assets_trashed(&self, ids: &[Uuid], trashed: bool) -> Result<u64> {
        let conn = self.store.conn();
        let before = ids
            .iter()
            .map(|id| {
                Ok((
                    *id,
                    assets::get(conn, *id)?
                        .map(|a| a.trashed_at.is_some())
                        .unwrap_or(false),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let changed = batch::set_trashed_many(conn, ids, trashed)?;
        if changed > 0 {
            self.undo.record(
                Op::SetTrashed {
                    before,
                    after: ids.iter().map(|id| (*id, trashed)).collect(),
                },
                OpDesc::counted(
                    if trashed {
                        OpAction::Trash
                    } else {
                        OpAction::Restore
                    },
                    ids.len(),
                ),
            );
        }
        Ok(changed)
    }

    /// Full metadata export (assets, collections, tags, smart collections) as
    /// pretty JSON. Media blobs are not included — the export is a portable
    /// catalog, not a backup of the files.
    pub fn export_metadata(&self) -> Result<String> {
        export_metadata_from_store(&self.store)
    }

    /// Restore a metadata catalog produced by [`Self::export_metadata`] into
    /// this library. Media files are not part of the export: assets whose
    /// content hash already exists are linked, everything else becomes
    /// a placeholder record that self-heals when the file is re-imported
    /// (content-addressed storage keys both paths by hash).
    pub fn import_metadata(&self, json: &str) -> Result<MetadataImportReport> {
        use crate::model::NewSmartCollection;

        let file: ExportFile = serde_json::from_str(json)
            .map_err(|e| crate::Error::Validation(format!("not a Trove export: {e}")))?;
        let mut report = MetadataImportReport::default();
        let conn = self.store.conn();

        // Tags: names are unique (case-insensitive), so an existing tag with
        // the same name is reused instead of duplicated. Hierarchy is
        // restored in a second pass, and only onto newly created tags so a
        // restore never reshuffles an existing tag tree.
        let mut tag_map: std::collections::HashMap<Uuid, Uuid> = Default::default();
        let mut created_tags: Vec<(Uuid, Option<Uuid>)> = Vec::new();
        for tag in file.tags {
            match tags::get_by_name(conn, &tag.name) {
                Ok(Some(existing)) => {
                    tag_map.insert(tag.id, existing.id);
                }
                Ok(None) => match tags::create(
                    conn,
                    &crate::model::NewTag {
                        name: tag.name.clone(),
                        color: tag.color.clone(),
                        parent_id: None,
                    },
                ) {
                    Ok(created) => {
                        tag_map.insert(tag.id, created.id);
                        created_tags.push((created.id, tag.parent_id));
                        report.tags += 1;
                    }
                    Err(_) => report.skipped += 1,
                },
                Err(_) => report.skipped += 1,
            }
        }
        for (tag_id, exported_parent) in created_tags {
            if let Some(old_parent) = exported_parent
                && let Some(new_parent) = tag_map.get(&old_parent)
            {
                let _ = tags::move_to(conn, tag_id, Some(*new_parent));
            }
        }

        // Collections: parents before children (the exported tree is
        // acyclic — moves are validated at runtime — but a depth guard
        // keeps a corrupt file from recursing forever).
        let by_id: std::collections::HashMap<Uuid, &crate::model::Collection> =
            file.collections.iter().map(|c| (c.id, c)).collect();
        let mut coll_map: std::collections::HashMap<Uuid, Uuid> = Default::default();
        for coll in &file.collections {
            insert_collection_tree(conn, coll, &by_id, &mut coll_map, &mut report, 0);
        }

        // Smart collections: copied with fresh ids (hierarchy restored in a
        // second pass, resolved through the id maps — an exported parent may
        // be a collection or another smart collection). An invalid condition
        // tree (foreign version) is skipped, not fatal.
        let mut sc_map: std::collections::HashMap<Uuid, Uuid> = Default::default();
        let mut created_smarts: Vec<(Uuid, Option<Uuid>, i64)> = Vec::new();
        for sc in file.smart_collections {
            let input = NewSmartCollection {
                parent_id: None,
                name: sc.name.clone(),
                query: sc.query.clone(),
                position: sc.position,
            };
            if input.validate().is_ok() && smart::validate_json(&input.query).is_ok() {
                match smart_collections::create(conn, &input) {
                    Ok(created) => {
                        if !sc.appearance.is_plain() {
                            let _ =
                                smart_collections::set_appearance(conn, created.id, &sc.appearance);
                        }
                        sc_map.insert(sc.id, created.id);
                        created_smarts.push((created.id, sc.parent_id, sc.position));
                        report.smart_collections += 1;
                    }
                    Err(_) => report.skipped += 1,
                }
            } else {
                report.skipped += 1;
            }
        }
        for (sc_id, exported_parent, position) in created_smarts {
            if let Some(old_parent) = exported_parent {
                // Prefer the smart-collection map, fall back to the
                // collection tree; unresolvable parents stay at the root.
                if let Some(new_parent) = sc_map
                    .get(&old_parent)
                    .or_else(|| coll_map.get(&old_parent))
                {
                    let _ = smart_collections::move_to(conn, sc_id, Some(*new_parent), position);
                }
            }
        }

        // Assets: match by content hash, else create a placeholder
        // (rel_path = None, invisible to orphan cleanup until healed).
        let mut asset_map: std::collections::HashMap<Uuid, Uuid> = Default::default();
        for asset in file.assets {
            if let Some(hash) = &asset.content_hash
                && let Some(existing) = assets::find_by_content_hash(conn, hash)?
            {
                asset_map.insert(asset.id, existing.id);
                report.assets_linked += 1;
                continue;
            }
            let id = Uuid::new_v4();
            let placeholder = crate::model::Asset::from_seed(crate::model::AssetSeed {
                id,
                // The exported record's location is deliberately *not* carried
                // over: a restore has the metadata and none of the bytes, which
                // is what a placeholder is. A linked export's recorded path
                // stays in `facts`, where it is provenance — the row is still
                // `stored`-with-nothing, and `location()` reads it that way.
                location: crate::model::AssetLocation::Placeholder,
                file_name: asset.file_name.clone(),
                ext: asset.ext.clone(),
                mime: asset.mime.clone(),
                size_bytes: asset.size_bytes,
                content_hash: asset.content_hash.clone(),
                kind: asset.kind,
                width: asset.width,
                height: asset.height,
                duration_ms: asset.duration_ms,
                captured_at: asset.captured_at,
                title: asset.title.clone(),
                description: asset.description.clone(),
                rating: asset.rating,
                is_favorite: asset.is_favorite,
                source_url: asset.source_url.clone(),
                usage_status: asset.usage_status,
                commercial_use: asset.commercial_use,
                facts: asset.facts.clone(),
                created_at: asset.created_at,
                updated_at: asset.updated_at,
                trashed_at: None,
            });
            assets::insert(conn, &placeholder)?;
            asset_map.insert(asset.id, id);
            report.assets_placeholder += 1;
        }

        // Memberships whose asset or collection is missing from the file (a
        // partial export) count as skipped, which the report surfaces honestly.
        for (old_asset, old_coll) in file.asset_collections {
            match (asset_map.get(&old_asset), coll_map.get(&old_coll)) {
                (Some(a), Some(c)) => {
                    collections::add_asset(conn, *c, *a)?;
                }
                _ => report.skipped += 1,
            }
        }
        for (old_asset, old_tag) in file.asset_tags {
            match (asset_map.get(&old_asset), tag_map.get(&old_tag)) {
                (Some(a), Some(t)) => {
                    tags::add_to_asset(conn, *a, *t)?;
                }
                _ => report.skipped += 1,
            }
        }

        Ok(report)
    }

    /// Permanently delete many assets atomically, freeing any content-addressed
    /// blob (and its thumbnail) once no asset references it left.
    ///
    /// A linked file generally outlives its record — it is the user's, wherever
    /// they keep it — with one exception: a file Trove put in its own inbox (a
    /// screenshot, a collected page, an extension upload) is deleted with the
    /// record. The inbox is a permanent import source and the dedupe key lives
    /// in the very row being deleted, so a file left behind there is imported
    /// again on the next scan: "delete" would undo itself on every restart.
    ///
    /// The other exception is opt-in: when the library's
    /// `purge_delete_sources` setting is on
    /// ([`crate::config::LibraryConfig::purge_delete_sources`]), linked files
    /// outside the inbox are deleted with their records too.
    pub fn purge_assets(&self, ids: &[Uuid]) -> Result<PurgeReport> {
        self.purge_assets_against(ids, &collect::inbox_dir())
    }

    /// [`purge_assets`] with the inbox spelled out, so the rule can be
    /// exercised without relocating the data root.
    pub(crate) fn purge_assets_against(&self, ids: &[Uuid], inbox: &Path) -> Result<PurgeReport> {
        // The setting is read per purge, so a flip in the settings window
        // applies to the very next delete with no restart.
        let delete_sources = crate::config::LibraryConfig::load(&self.root).purge_delete_sources();
        // Track (rel, hash) for every content hash left unreferenced by this
        // purge, so the file is deleted exactly once even when several deleted
        // assets shared it. Linked sources are collected the same way, then
        // tried against the inbox once the records are gone. Every purged
        // hash — referenced or not — has its derived files taken with it.
        let mut freed: Vec<(String, String)> = Vec::new();
        let mut sources: Vec<PathBuf> = Vec::new();
        let mut derived: Vec<String> = Vec::new();
        let purged = self.store.transaction(|tx| {
            let mut freed_tx: Vec<(String, String)> = Vec::new();
            let mut sources_tx: Vec<PathBuf> = Vec::new();
            let mut derived_tx: Vec<String> = Vec::new();
            for id in ids {
                let Some(asset) = assets::get(tx, *id)? else {
                    continue;
                };
                let location = asset.location();
                if let AssetLocation::Linked { source_path } = &location {
                    sources_tx.push(PathBuf::from(source_path));
                }
                let hash = asset.content_hash.clone();
                let rel = match &location {
                    AssetLocation::Stored { rel_path } => Some(rel_path.clone()),
                    _ => None,
                };
                assets::delete(tx, *id)?;
                if let Some(hash) = &hash
                    && !derived_tx.contains(hash)
                {
                    derived_tx.push(hash.clone());
                }
                if let (Some(hash), Some(rel)) = (hash, rel)
                    && assets::count_by_content_hash(tx, &hash)? == 0
                {
                    freed_tx.push((rel, hash));
                }
            }
            freed = freed_tx;
            sources = sources_tx;
            derived = derived_tx;
            Ok(ids.len() as u64)
        })?;

        let mut report = PurgeReport {
            purged,
            ..Default::default()
        };
        // Derived files go with every deleted record, whatever else shares
        // the content: the cache exists to serve the records, and whatever a
        // surviving twin still needs regenerates on its next view.
        for hash in &derived {
            media::thumb::remove_derived(&self.cache, hash);
            report.thumbs_removed += 1;
        }
        for (rel, _hash) in freed {
            if rel.starts_with("media/") {
                report.blobs_removed += 1;
                self.remove_blob_file(&rel);
            }
        }
        // Inbox files always go with their record (see [`Self::purge_assets`]);
        // linked files everywhere else only when the setting asks for it.
        for source in sources {
            if collect::is_in_inbox(inbox, &source) {
                if collect::remove_inbox_file(&source) {
                    report.sources_removed += 1;
                    tracing::info!(
                        path = %source.display(),
                        "purge: removed the inbox file along with its asset"
                    );
                }
            } else if delete_sources {
                match std::fs::remove_file(&source) {
                    Ok(()) => {
                        report.source_files_removed += 1;
                        tracing::info!(
                            path = %source.display(),
                            "purge: removed the linked source at the user's request"
                        );
                    }
                    // A second purged asset sharing this source got there first.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => tracing::warn!(
                        path = %source.display(),
                        %error,
                        "purge: could not remove a linked source"
                    ),
                }
            }
        }
        Ok(report)
    }

    /// Best-effort removal of a content-addressed blob, once no record
    /// references the content any more. The derived files went already —
    /// see the purge loop.
    fn remove_blob_file(&self, rel: &str) {
        if rel.starts_with("media/") {
            let _ = std::fs::remove_file(self.root.join(rel));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Library;
    use crate::media::thumb;
    use crate::model::{AssetKind, AssetLocation, AssetQuery, NewCollection, NewSmartCollection};
    use crate::store::{assets, collections, tags};
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    /// A minimal valid 1x1 PNG.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn temp_library(name: &str) -> (Library, PathBuf) {
        let root = std::env::temp_dir().join(format!("trove-lib-{name}-{}", Uuid::new_v4()));
        let lib = Library::open(&root, root.join("cache")).unwrap();
        (lib, root)
    }

    /// The blob path a stored record names, with a failure that says what the
    /// record actually was.
    ///
    /// Tests reach for this because a location is one value now: an `unwrap` on
    /// the old optional column could only report "none", while a stored record
    /// with no path is a *named* state — a placeholder — and telling those two
    /// apart is the whole point of a restore test.
    fn stored_rel(asset: &crate::model::Asset) -> String {
        match asset.location() {
            crate::model::AssetLocation::Stored { rel_path } => rel_path,
            other => panic!("expected a stored asset, found {other:?}"),
        }
    }

    fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn relink_asset_repoints_a_moved_file() {
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

        let (lib, root) = temp_library("relink");
        let outside = std::env::temp_dir().join(format!("trove-relink-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        let src = outside.join("linked.png");
        std::fs::write(&src, PNG_1X1).unwrap();

        // Import as linked (file stays in place).
        let staged = stage_all(
            &root,
            &root.join("cache"),
            std::slice::from_ref(&src),
            ImportStorage::Link,
            &std::sync::atomic::AtomicBool::new(false),
        );
        let report = commit_staged_all(lib.store().conn(), None, staged);
        assert_eq!(report.imported_count(), 1);
        let conn = lib.store().conn();
        let all = assets::query(conn, &AssetQuery::default()).unwrap();
        let id = all.items[0].id;
        assert!(all.items[0].location().is_linked());

        // Move the file elsewhere, then reconnect the record to it.
        let moved = outside.join("moved-elsewhere.png");
        std::fs::rename(&src, &moved).unwrap();
        lib.relink_asset(id, &moved).unwrap();
        let asset = assets::get(conn, id).unwrap().unwrap();
        assert_eq!(
            asset.facts.source_path.as_deref(),
            Some(moved.display().to_string().as_str())
        );
        assert_eq!(
            asset.content_hash.as_deref(),
            all.items[0].content_hash.as_deref()
        );

        // Different content is rejected — relinking never swaps content.
        let other = outside.join("other.png");
        std::fs::write(&other, b"not the same").unwrap();
        assert!(lib.relink_asset(id, &other).is_err());

        // Stored assets cannot be relinked.
        let stored = write_source(&root, "stored.txt", b"stored content");
        lib.import_into_store(std::slice::from_ref(&stored), None)
            .unwrap();
        let all2 = assets::query(conn, &AssetQuery::default()).unwrap();
        let stored_id = all2
            .items
            .iter()
            .find(|a| !a.location().is_linked())
            .expect("stored import")
            .id;
        assert!(lib.relink_asset(stored_id, &stored).is_err());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn auto_import_groups_into_collections() {
        // Imports go directly to "All Assets" without creating collections.
        let (lib, root) = temp_library("auto-source");
        let folder = root.join("Vacation");
        std::fs::create_dir_all(&folder).unwrap();
        let src = write_source(&folder, "photo.png", PNG_1X1);

        let report = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        assert_eq!(report.imported_count(), 1);
        let _item = &report.imported[0];

        // No auto-created collections — asset goes to "All Assets".
        let roots = collections::roots(lib.store().conn()).unwrap();
        assert_eq!(roots.len(), 0);

        // Total asset count is 1.
        let page = assets::query(lib.store().conn(), &crate::model::AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);

        // Re-importing identical content dedupes.
        let report2 = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        assert_eq!(report2.imported_count(), 1);
        assert!(report2.imported[0].reused);
    }
    #[test]
    fn imports_png_and_generates_thumbnail() {
        let (lib, root) = temp_library("png");
        let src = write_source(&root, "photo.png", PNG_1X1);

        let report = lib.import_into_store(&[src], None).unwrap();
        assert_eq!(report.imported_count(), 1);
        assert_eq!(report.skipped_count(), 0);
        let item = &report.imported[0];
        assert!(!item.reused);
        assert_eq!(item.kind, AssetKind::Image);

        let page = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);
        let asset = &page.items[0];
        assert_eq!(asset.mime, "image/png");
        assert_eq!(asset.width, Some(1));
        assert_eq!(asset.height, Some(1));
        assert!(asset.content_hash.is_some());
        assert_eq!(asset.file_name, "photo.png");

        // The blob exists on disk under a content-addressed name.
        let rel = stored_rel(asset);
        assert!(lib.resolve(&rel).is_file());

        // A JPEG thumbnail was generated next to it.
        let thumb_path = thumb::abs_path(lib.cache(), asset.content_hash.as_deref().unwrap());
        assert!(
            thumb_path.is_file(),
            "thumbnail missing at {}",
            thumb_path.display()
        );
    }

    #[test]
    fn identical_content_is_deduplicated_and_reused() {
        let (lib, root) = temp_library("dedup");
        let src = write_source(&root, "same.png", PNG_1X1);

        let first = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let second = lib.import_into_store(&[src], None).unwrap();
        assert!(!first.imported[0].reused);
        assert!(second.imported[0].reused);
        assert_eq!(first.imported[0].asset_id, second.imported[0].asset_id);

        let page = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);
    }

    #[test]
    fn import_into_collection_and_membership() {
        let (lib, root) = temp_library("collection");
        let c = collections::create(
            lib.store().conn(),
            &NewCollection {
                parent_id: None,
                name: "album".into(),
                position: 0,
            },
        )
        .unwrap();

        let src = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_into_store(&[src], Some(c.id)).unwrap();
        assert_eq!(report.imported_count(), 1);
        assert_eq!(
            collections::count_assets(lib.store().conn(), c.id).unwrap(),
            1
        );

        // Importing into a missing collection fails up front.
        let err = lib
            .import_into_store(&[root.join("nope.png")], Some(Uuid::new_v4()))
            .unwrap_err();
        assert!(err.to_string().contains("collection"));
    }

    #[test]
    fn plain_files_and_skipped_paths() {
        let (lib, root) = temp_library("plain");
        let txt = write_source(&root, "notes.txt", b"hello world");
        let report = lib.import_into_store(&[txt], None).unwrap();
        let item = &report.imported[0];
        assert_eq!(item.kind, AssetKind::Document);

        // A non-existent path is reported, not fatal.
        let missing = root.join("missing.bin");
        let report = lib
            .import_into_store(std::slice::from_ref(&missing), None)
            .unwrap();
        assert_eq!(report.imported_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped[0].path, missing);
    }

    #[test]
    fn purge_removes_blob_only_when_unreferenced() {
        let (lib, root) = temp_library("purge");
        // One imported record…
        let a = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_into_store(&[a], None).unwrap();
        assert_eq!(report.imported_count(), 1);
        let first_id = report.imported[0].asset_id;
        let stored = assets::get(lib.store().conn(), first_id).unwrap().unwrap();
        let blob = lib.resolve(&stored_rel(&stored));

        // …moved to trash, then re-imported with a different file name but the
        // same bytes: a second record sharing the same blob.
        assert!(assets::set_trashed(lib.store().conn(), first_id, true).unwrap());
        let b = write_source(&root, "b.png", PNG_1X1);
        let report2 = lib.import_into_store(&[b], None).unwrap();
        assert_eq!(report2.imported_count(), 1);
        assert!(
            !report2.imported[0].reused,
            "trashed content is re-imported fresh"
        );
        let second_id = report2.imported[0].asset_id;

        // Purging the live record keeps the blob (trashed record references it).
        lib.purge_asset(second_id).unwrap();
        assert!(
            blob.is_file(),
            "blob must survive while a trashed record exists"
        );

        // Purging the trashed record removes blob and thumbnail.
        lib.purge_asset(first_id).unwrap();
        assert!(
            !blob.exists(),
            "blob removed after last reference is purged"
        );
        let hash = stored.content_hash.unwrap();
        assert!(!thumb::abs_path(lib.cache(), &hash).exists());
    }

    /// Import one file as a *linked* asset — the shape screenshots and
    /// collected pages have, where the file stays where it is.
    fn import_linked(lib: &Library, root: &Path, source: &Path) -> Uuid {
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

        let staged = stage_all(
            root,
            &root.join("cache"),
            std::slice::from_ref(&source.to_path_buf()),
            ImportStorage::Link,
            &std::sync::atomic::AtomicBool::new(false),
        );
        let report = commit_staged_all(lib.store().conn(), None, staged);
        assert_eq!(report.imported_count(), 1);
        report.imported[0].asset_id
    }

    /// A screenshot's record and its file go together: the inbox keeps its
    /// files forever and is a permanent import source, so a file left behind
    /// would be imported again on the next scan and the delete would undo
    /// itself at every restart.
    #[test]
    fn purging_an_asset_removes_its_file_from_the_inbox() {
        let (lib, root) = temp_library("purge-inbox");
        let inbox = root.join("incoming");
        std::fs::create_dir_all(&inbox).unwrap();
        let shot = write_source(&inbox, "screenshot-1.png", PNG_1X1);
        let sidecar = inbox.join("screenshot-1.png.meta.json");
        std::fs::write(&sidecar, b"{}").unwrap();

        let id = import_linked(&lib, &root, &shot);
        let report = lib.purge_assets_against(&[id], &inbox).unwrap();

        assert_eq!(report.purged, 1);
        assert_eq!(report.sources_removed, 1);
        assert!(!shot.exists(), "the file goes with the record");
        assert!(!sidecar.exists(), "and so does its sidecar");
        assert!(assets::get(lib.store().conn(), id).unwrap().is_none());
    }

    /// Everywhere else the file is the user's: purging the record leaves it
    /// exactly where it was.
    #[test]
    fn purging_a_linked_file_outside_the_inbox_leaves_it_alone() {
        let (lib, root) = temp_library("purge-outside");
        let inbox = root.join("incoming");
        let pictures = root.join("pictures");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&pictures).unwrap();
        let photo = write_source(&pictures, "holiday.png", PNG_1X1);

        let id = import_linked(&lib, &root, &photo);
        let report = lib.purge_assets_against(&[id], &inbox).unwrap();

        assert_eq!(report.purged, 1);
        assert_eq!(report.sources_removed, 0);
        assert!(photo.is_file(), "a user's own file is never deleted");
    }

    /// The opt-in exception: with the library's `purge_delete_sources`
    /// setting on, the linked file outside the inbox goes with the record.
    #[test]
    fn purging_a_linked_file_deletes_it_when_the_setting_asks() {
        let (lib, root) = temp_library("purge-delete-source");
        let inbox = root.join("incoming");
        let pictures = root.join("pictures");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&pictures).unwrap();
        let photo = write_source(&pictures, "holiday.png", PNG_1X1);

        crate::config::LibraryConfig {
            purge_delete_sources: Some(true),
            ..Default::default()
        }
        .save(&root)
        .unwrap();

        let id = import_linked(&lib, &root, &photo);
        let report = lib.purge_assets_against(&[id], &inbox).unwrap();

        assert_eq!(report.purged, 1);
        assert_eq!(report.source_files_removed, 1);
        assert!(
            !photo.exists(),
            "the setting turns the user's own file deletable"
        );
        assert!(assets::get(lib.store().conn(), id).unwrap().is_none());
    }

    /// The setting never reroutes inbox files onto the plain-delete path:
    /// they follow their own rule (record plus sidecars) whatever it says.
    #[test]
    fn purging_an_inbox_file_ignores_the_source_deletion_setting() {
        let (lib, root) = temp_library("purge-inbox-setting");
        let inbox = root.join("incoming");
        std::fs::create_dir_all(&inbox).unwrap();
        let shot = write_source(&inbox, "screenshot-3.png", PNG_1X1);
        let sidecar = inbox.join("screenshot-3.png.meta.json");
        std::fs::write(&sidecar, b"{}").unwrap();

        crate::config::LibraryConfig {
            purge_delete_sources: Some(true),
            ..Default::default()
        }
        .save(&root)
        .unwrap();

        let id = import_linked(&lib, &root, &shot);
        let report = lib.purge_assets_against(&[id], &inbox).unwrap();

        assert_eq!(report.sources_removed, 1);
        assert_eq!(report.source_files_removed, 0);
        assert!(!shot.exists());
        assert!(!sidecar.exists(), "the sidecar goes with its file");
    }

    /// Soft delete stays reversible, so it must not touch the file at all —
    /// only a purge does, and emptying the trash counts as one.
    #[test]
    fn trashing_an_inbox_file_leaves_it_for_the_restore() {
        let (lib, root) = temp_library("trash-inbox");
        let inbox = root.join("incoming");
        std::fs::create_dir_all(&inbox).unwrap();
        let shot = write_source(&inbox, "screenshot-2.png", PNG_1X1);

        let id = import_linked(&lib, &root, &shot);
        lib.trash_assets(&[id]).unwrap();
        assert!(shot.is_file(), "the trash is undoable, so the file stays");

        assert_eq!(lib.empty_trash_against(&inbox).unwrap(), 1);
        assert!(!shot.exists(), "emptying the trash is what removes it");
    }

    #[test]
    fn empty_trash_removes_all_and_frees_blobs() {
        let (lib, root) = temp_library("empty-trash");
        let one = write_source(&root, "one.png", PNG_1X1);
        let txt = write_source(&root, "notes.txt", b"bye");
        let r = lib.import_into_store(&[one, txt], None).unwrap();
        assert_eq!(r.imported_count(), 2);
        for item in &r.imported {
            assert!(assets::set_trashed(lib.store().conn(), item.asset_id, true).unwrap());
        }
        let removed = lib.empty_trash().unwrap();
        assert_eq!(removed, 2);
        let trash = assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(trash.items.is_empty());
    }

    #[test]
    fn tags_are_case_insensitive_and_attach_to_assets() {
        let (lib, root) = temp_library("tags");
        let src = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_into_store(&[src], None).unwrap();
        let asset_id = report.imported[0].asset_id;

        let red = tags::ensure_named(lib.store().conn(), "Red").unwrap();
        // Case-insensitive find reuses the same tag.
        let again = tags::ensure_named(lib.store().conn(), "red").unwrap();
        assert_eq!(red.id, again.id);
        assert_eq!(red.name, "Red");

        tags::add_to_asset(lib.store().conn(), asset_id, red.id).unwrap();
        let on_asset = tags::for_asset(lib.store().conn(), asset_id).unwrap();
        assert_eq!(on_asset.len(), 1);
        assert_eq!(tags::count_assets(lib.store().conn(), red.id).unwrap(), 1);

        // Replacing the tag set drops membership.
        let blue = tags::ensure_named(lib.store().conn(), "blue").unwrap();
        tags::set_for_asset(lib.store().conn(), asset_id, &[blue.id]).unwrap();
        assert!(tags::for_asset(lib.store().conn(), asset_id).unwrap()[0].name == "blue");

        // Tag filter in asset queries.
        let page = assets::query(
            lib.store().conn(),
            &AssetQuery {
                tag_ids: vec![blue.id],
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 1);

        // Deleting a tag removes its membership rows.
        tags::delete(lib.store().conn(), red.id).unwrap();
        assert!(tags::for_asset(lib.store().conn(), asset_id).unwrap()[0].name == "blue");
    }

    #[test]
    fn trash_then_reimport_creates_fresh_record() {
        let (lib, root) = temp_library("trash");
        let src = write_source(&root, "x.png", PNG_1X1);
        let first = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let id = first.imported[0].asset_id;

        assert!(assets::set_trashed(lib.store().conn(), id, true).unwrap());
        let second = lib.import_into_store(&[src], None).unwrap();
        assert!(!second.imported[0].reused);
        assert_ne!(second.imported[0].asset_id, id);

        let page = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);
        let trashed = assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trashed.items.len(), 1);
    }

    #[test]
    fn search_assets_speaks_the_search_box_grammar() {
        // The CLI and the grid must agree, so this exercises the same grammar
        // through the facade `trove search` uses.
        let (lib, root) = temp_library("grammar");
        let png = write_source(&root, "photo.png", PNG_1X1);
        let mp4 = write_source(&root, "reel.mp4", b"not really a video");
        lib.import_into_store(&[png], None).unwrap();
        lib.import_into_store(&[mp4], None).unwrap();
        for asset in assets::query(lib.store().conn(), &AssetQuery::default())
            .unwrap()
            .items
        {
            assets::update(
                lib.store().conn(),
                asset.id,
                &crate::model::AssetPatch {
                    title: Some(Some("sunset frame".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        lib.rebuild_text_index().unwrap();

        let total = |query: &str| {
            lib.search_assets(query, &AssetQuery::default())
                .unwrap()
                .total
        };
        let ids = |query: &str| {
            let page = lib.search_assets(query, &AssetQuery::default()).unwrap();
            let mut names: Vec<String> = page.items.iter().map(|a| a.file_name.clone()).collect();
            names.sort();
            names
        };

        // Both titles match; the ranking has nothing to narrow.
        assert_eq!(total("sunset"), 2);
        // A qualifier narrows the ranked set.
        assert_eq!(ids("sunset ext:png"), vec!["photo.png"]);
        assert_eq!(ids("sunset -kind:video"), vec!["photo.png"]);
        // A qualifier alone is still an answer: the filtered listing.
        assert_eq!(ids("ext:mp4"), vec!["reel.mp4"]);
        // A box of pure syntax matches nothing rather than everything.
        assert_eq!(total("\""), 0);
        assert_eq!(total("--"), 0);
    }

    #[test]
    fn search_and_smart_collection_facade() {
        // A title exposes a searchable token ("sunset") that no other item has.
        let (lib, root) = temp_library("facade");
        let one = write_source(&root, "photo.png", PNG_1X1);
        lib.import_into_store(&[one], None).unwrap();
        let two = write_source(&root, "notes.txt", b"plain");
        lib.import_into_store(&[two], None).unwrap();

        // The photo is retitled so it participates in full-text search.
        let all = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let photo_id = all
            .items
            .iter()
            .find(|a| a.file_name == "photo.png")
            .unwrap()
            .id;
        assets::update(
            lib.store().conn(),
            photo_id,
            &crate::model::AssetPatch {
                title: Some(Some("sunset on the dock".into())),
                is_favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        // search_assets hits only the retitled photo.
        let hits = lib.search_assets("sunset", &AssetQuery::default()).unwrap();
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo_id);

        // A smart collection over the same terms evaluates through the facade.
        let sc = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: None,
                name: "Dockpics".into(),
                query: serde_json::json!({
                    "op": "and",
                    "children": [
                        { "op": "match", "field": "text", "value": "sunset" },
                        { "op": "match", "field": "is_favorite", "value": true },
                    ]
                }),
                position: 0,
            })
            .unwrap();
        assert_eq!(lib.list_smart_collections().unwrap().len(), 1);
        let assets = lib
            .evaluate_smart_collection(sc.id, crate::store::smart::SmartPage::default())
            .unwrap();
        assert_eq!(assets.total, 1);
        assert_eq!(assets.items[0].id, photo_id);

        // Renaming + delete roundtrip.
        lib.rename_smart_collection(sc.id, "Sunset shots").unwrap();
        assert_eq!(
            lib.get_smart_collection(sc.id).unwrap().unwrap().name,
            "Sunset shots"
        );
        lib.delete_smart_collection(sc.id).unwrap();
        assert!(lib.get_smart_collection(sc.id).unwrap().is_none());
    }

    #[test]
    fn empty_index_is_rebuilt_on_open() {
        // A wiped (or lost) index over live rows must be noticed on open and
        // rebuilt from the rows — tag names included — so search self-repairs
        // without a manual rebuild.
        let (lib, root) = temp_library("index-backfill");
        let src = write_source(&root, "photo.png", PNG_1X1);
        lib.import_into_store(&[src], None).unwrap();

        let conn = lib.store().conn();
        let all = assets::query(conn, &AssetQuery::default()).unwrap();
        let photo_id = all.items[0].id;
        assets::update(
            conn,
            photo_id,
            &crate::model::AssetPatch {
                title: Some(Some("sunset over the sea".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let tag = tags::create(
            conn,
            &crate::model::NewTag {
                name: "landscape".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::add_to_asset(conn, photo_id, tag.id).unwrap();

        // Wipe the index, then reopen the library from disk: the reconcile
        // step must notice the empty index and rebuild it from the rows.
        lib.text_index().wipe().unwrap();
        drop(lib);
        let reopened = Library::open(&root, root.join("cache")).unwrap();
        let _conn = reopened.store().conn();

        let hits = reopened
            .search_assets("sunset", &AssetQuery::default())
            .unwrap();
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo_id);

        // The rebuilt index carries tag names too.
        let page = reopened
            .search_assets("landscape", &AssetQuery::default())
            .unwrap();
        assert_eq!(page.total, 1);
    }

    #[test]
    fn tag_and_smart_collection_facade_methods() {
        let (lib, root) = temp_library("facade");

        // ensure_tag is idempotent by (trimmed) name.
        let t1 = lib.ensure_tag("tree").unwrap();
        let t2 = lib.ensure_tag("  tree  ").unwrap();
        assert_eq!(t1.id, t2.id);
        let _ = lib.ensure_tag("park").unwrap();
        let conn = lib.store().conn();
        assert_eq!(tags::list(conn).unwrap().len(), 2);

        // Attach the tag to an asset, then delete it via the facade.
        let src = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_into_store(&[src], None).unwrap();
        let asset_id = report.imported[0].asset_id;
        lib.tag_assets(&[asset_id], t1.id, true).unwrap();
        lib.delete_tag(t1.id).unwrap();
        assert!(tags::for_asset(conn, asset_id).unwrap().is_empty());
        assert!(tags::list(conn).unwrap().iter().all(|t| t.id != t1.id));

        // Smart collection rename + delete through the facade.
        let sc = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: None,
                name: "old".into(),
                query: serde_json::json!({
                    "op": "match",
                    "field": "text",
                    "value": "x",
                }),
                position: 0,
            })
            .unwrap();
        lib.rename_smart_collection(sc.id, "new").unwrap();
        assert_eq!(
            lib.get_smart_collection(sc.id).unwrap().unwrap().name,
            "new"
        );
        lib.delete_smart_collection(sc.id).unwrap();
        assert!(lib.get_smart_collection(sc.id).unwrap().is_none());
    }
    #[test]
    fn usage_status_and_commercial_use_patch_query_and_undo() {
        use crate::model::{AssetPatch, UsageStatus};

        let (lib, dir) = temp_library("usage-status");
        let src = write_source(&dir, "a.png", PNG_1X1);
        let report = lib.import_into_store(&[src], None).unwrap();
        let id = report.imported[0].asset_id;
        let conn = lib.store().conn();

        // Fresh imports are unused and license-unverified.
        let asset = assets::get(conn, id).unwrap().unwrap();
        assert_eq!(asset.usage_status, UsageStatus::Unused);
        assert_eq!(asset.commercial_use, None);

        // Set both; the query filters see them.
        lib.patch_asset(
            id,
            &AssetPatch {
                usage_status: Some(UsageStatus::Used),
                commercial_use: Some(Some(false)),
                ..Default::default()
            },
        )
        .unwrap();
        let page = assets::query(
            conn,
            &AssetQuery {
                usage_status: Some(UsageStatus::Used),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((page.total, page.items.len()), (1, 1));
        let page = assets::query(
            conn,
            &AssetQuery {
                commercial_use: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((page.total, page.items.len()), (1, 1));
        // Unverified rows match neither clearance filter.
        let page = assets::query(
            conn,
            &AssetQuery {
                commercial_use: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 0);

        // Undo restores the fresh-import state (unused, unverified).
        lib.undo().unwrap();
        let asset = assets::get(conn, id).unwrap().unwrap();
        assert_eq!(asset.usage_status, UsageStatus::Unused);
        assert_eq!(asset.commercial_use, None);
    }
    #[test]
    fn duplicate_content_import_needs_no_sha_scan() {
        // The importer deduplicates identical content at the record level,
        // so two live assets never share a content hash — the duplicate finder
        // works on perceptual hashes instead.
        let (lib, dir) = temp_library("duplicates-hash");
        let src = write_source(&dir, "same.png", PNG_1X1);
        lib.import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        lib.import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let page = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);
        assert!(lib.find_duplicates().unwrap().is_empty());
    }

    #[test]
    fn duplicate_groups_cluster_by_phash() {
        use crate::model::{AssetKind, test_asset};
        use crate::store::Store;

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let set_phash = |name: &str, hash: u64| {
            let id = Uuid::new_v4();
            let mut asset = test_asset(name, AssetKind::Image, id);
            asset.facts.visual.visual_phash = Some(format!("{hash:016x}"));
            assets::insert(conn, &asset).unwrap();
            id
        };
        let a = set_phash("a.png", 0x0000_0000_0000_0001);
        let b = set_phash("b.png", 0x0000_0000_0000_0003); // 1 bit from a
        let c = set_phash("c.png", 0x0000_0000_0000_0007); // 1 bit from b
        set_phash("far.png", 0xAAAA_0000_5555_0000); // unrelated
        set_phash("nosig.png", 0x0); // no usable signature, ignored

        let groups = crate::store::assets::duplicate_groups(conn).unwrap();
        assert_eq!(groups.len(), 1, "a/b/c form one cluster, the rest none");
        let ids: Vec<Uuid> = groups[0].assets.iter().map(|x| x.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b) && ids.contains(&c));

        // Trashing members shrinks then dissolves the cluster.
        lib_trash(&store, c);
        let groups = crate::store::assets::duplicate_groups(conn).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].assets.len(), 2);
        lib_trash(&store, b);
        assert!(
            crate::store::assets::duplicate_groups(conn)
                .unwrap()
                .is_empty()
        );
    }

    fn lib_trash(store: &crate::store::Store, id: Uuid) {
        crate::store::assets::set_trashed(store.conn(), id, true).unwrap();
    }
    #[test]
    fn batch_rename_rewrites_titles_and_undoes_once() {
        let (lib, dir) = temp_library("batch-rename");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        // Different content: identical imports dedup to one record.
        let b = write_source(&dir, "beta.txt", b"beta");
        let ra = lib
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let rb = lib
            .import_into_store(std::slice::from_ref(&b), None)
            .unwrap();
        let ids = [ra.imported[0].asset_id, rb.imported[0].asset_id];

        let count = lib.batch_rename(&ids, "trip-{n} {name}", 2).unwrap();
        assert_eq!(count, 2);
        let conn = lib.store().conn();
        assert_eq!(
            assets::get(conn, ids[0]).unwrap().unwrap().title.as_deref(),
            Some("trip-2 alpha")
        );
        assert_eq!(
            assets::get(conn, ids[1]).unwrap().unwrap().title.as_deref(),
            Some("trip-3 beta")
        );

        // One undo restores both original titles.
        lib.undo().unwrap();
        assert_eq!(
            assets::get(conn, ids[0]).unwrap().unwrap().title.as_deref(),
            None
        );
        assert_eq!(
            assets::get(conn, ids[1]).unwrap().unwrap().title.as_deref(),
            None
        );
        // Empty pattern is rejected.
        assert!(lib.batch_rename(&ids, "  ", 1).is_err());
    }
    #[test]
    fn metadata_export_import_roundtrip() {
        let (lib, dir) = temp_library("export");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        let b = write_source(&dir, "beta.txt", b"beta");
        let ra = lib
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let rb = lib
            .import_into_store(std::slice::from_ref(&b), None)
            .unwrap();
        let (ia, ib) = (ra.imported[0].asset_id, rb.imported[0].asset_id);
        let coll = collections::create(
            lib.store().conn(),
            &crate::model::NewCollection {
                parent_id: None,
                name: "Trip".into(),
                position: 0,
            },
        )
        .unwrap();
        lib.add_assets_to_collection(coll.id, &[ia, ib]).unwrap();
        let tag = lib.ensure_tag("sunset").unwrap();
        lib.tag_assets(&[ia], tag.id, true).unwrap();

        let json = lib.export_metadata().unwrap();

        // Fresh library: everything comes back as placeholders.
        let (other, _) = temp_library("import");
        let report = other.import_metadata(&json).unwrap();
        assert_eq!(report.assets_placeholder, 2);
        assert_eq!(report.assets_linked, 0);
        assert_eq!(report.collections, 1);
        assert_eq!(report.tags, 1);
        let conn = other.store().conn();
        let restored = assets::query(conn, &AssetQuery::default()).unwrap();
        let restored_ids: Vec<Uuid> = restored.items.iter().map(|x| x.id).collect();
        assert_eq!(restored.items.len(), 2);
        // Membership survived the id remap.
        let in_coll = collections::asset_ids(conn, coll.id);
        let _ = in_coll; // collection id changed; assert via name below
        let names: Vec<String> = collections::list(conn)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["Trip".to_string()]);
        let _tagged = tags::for_asset(conn, restored_ids[0]).unwrap();
        // The image asset (first import) carries the tag.
        let image = restored
            .items
            .iter()
            .find(|x| x.kind == AssetKind::Image)
            .unwrap();
        let tagged = tags::for_asset(conn, image.id).unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].name, "sunset");

        // Restore again into the ORIGINAL library: content matches, so
        // everything links and nothing duplicates.
        let report = lib.import_metadata(&json).unwrap();
        assert_eq!(report.assets_linked, 2);
        assert_eq!(report.assets_placeholder, 0);
        let page = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 2);
    }

    #[test]
    fn smart_collection_hierarchy_survives_export_import() {
        use crate::store::smart_collections;

        let (lib, _dir) = temp_library("smart-hier");
        let conn = lib.store().conn();
        let coll = collections::create(
            conn,
            &NewCollection {
                parent_id: None,
                name: "Trip".into(),
                position: 0,
            },
        )
        .unwrap();
        let fav = serde_json::json!({"op": "match", "field": "is_favorite", "value": true});
        let under_coll = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: Some(coll.id),
                name: "in-trip".into(),
                query: fav.clone(),
                position: 0,
            })
            .unwrap();
        let outer = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: None,
                name: "outer".into(),
                query: fav.clone(),
                position: 1,
            })
            .unwrap();
        let inner = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: Some(outer.id),
                name: "inner".into(),
                query: fav,
                position: 0,
            })
            .unwrap();

        let json = lib.export_metadata().unwrap();
        let (other, _) = temp_library("smart-hier-import");
        let report = other.import_metadata(&json).unwrap();
        assert_eq!(report.collections, 1);
        assert_eq!(report.smart_collections, 3);

        // Ids are fresh; both kinds of parent links are re-resolved in the
        // importing library (collection parent and smart parent alike).
        let oconn = other.store().conn();
        let restored = smart_collections::list(oconn).unwrap();
        let by_name = |n: &str| {
            restored
                .iter()
                .find(|sc| sc.name == n)
                .unwrap_or_else(|| panic!("missing {n}"))
                .clone()
        };
        let (r_in_trip, r_outer, r_inner) =
            (by_name("in-trip"), by_name("outer"), by_name("inner"));
        assert_ne!(r_in_trip.id, under_coll.id);
        assert_ne!(r_outer.id, outer.id);
        assert_ne!(r_inner.id, inner.id);
        let coll_id = collections::list(oconn).unwrap()[0].id;
        assert_eq!(r_in_trip.parent_id, Some(coll_id));
        assert_eq!(r_outer.parent_id, None);
        assert_eq!(r_inner.parent_id, Some(r_outer.id));

        // The moved hierarchy is fully usable: evaluation through the facade.
        assert!(other.move_smart_collection(r_inner.id, None, 5).is_ok());
        assert_eq!(
            other
                .get_smart_collection(r_inner.id)
                .unwrap()
                .unwrap()
                .parent_id,
            None
        );
    }

    #[test]
    fn placeholder_self_heals_on_reimport() {
        let (lib, dir) = temp_library("heal");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        lib.import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let json = lib.export_metadata().unwrap();

        let (other, _) = temp_library("heal-target");
        other.import_metadata(&json).unwrap();
        let conn = other.store().conn();
        let restored = assets::query(conn, &AssetQuery::default()).unwrap();
        assert_eq!(restored.items.len(), 1);
        assert_eq!(restored.items[0].location(), AssetLocation::Placeholder);

        // Re-importing the same content links the blob into the placeholder.
        other
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let healed = assets::get(conn, restored.items[0].id).unwrap().unwrap();
        assert!(matches!(healed.location(), AssetLocation::Stored { .. }));
        let page = assets::query(conn, &AssetQuery::default()).unwrap();
        assert_eq!(page.total, 1);
    }
    #[test]
    fn hierarchical_tags_filter_include_subtree() {
        use crate::model::AssetPatch;
        use crate::model::NewTag;
        use crate::store::collections;

        let (lib, dir) = temp_library("hier-tags");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        let b = write_source(&dir, "beta.txt", b"beta");
        let ra = lib
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let rb = lib
            .import_into_store(std::slice::from_ref(&b), None)
            .unwrap();
        let (ia, ib) = (ra.imported[0].asset_id, rb.imported[0].asset_id);
        let conn = lib.store().conn();

        // animal > cat; animal > dog
        let animal = tags::create(
            conn,
            &NewTag {
                name: "animal".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        let cat = tags::create(
            conn,
            &NewTag {
                name: "cat".into(),
                color: None,
                parent_id: Some(animal.id),
            },
        )
        .unwrap();
        tags::create(
            conn,
            &NewTag {
                name: "dog".into(),
                color: None,
                parent_id: Some(animal.id),
            },
        )
        .unwrap();

        tags::add_to_asset(conn, ia, cat.id).unwrap();
        lib.patch_asset(
            ib,
            &AssetPatch {
                ..Default::default()
            },
        )
        .unwrap();

        // Filtering by the parent finds assets tagged with the child.
        let q = AssetQuery {
            tag_ids: vec![animal.id],
            ..Default::default()
        };
        let page = assets::query(conn, &q).unwrap();
        assert_eq!((page.total, page.items.len()), (1, 1));
        assert_eq!(page.items[0].id, ia);

        // The subtree count matches the filter.
        assert_eq!(tags::count_assets(conn, animal.id).unwrap(), 1);

        // Smart collection by tag name includes the subtree.
        let node = crate::store::smart::node_from_json(&serde_json::json!({
            "op": "match", "field": "tag", "value": "animal"
        }))
        .unwrap();
        let ids =
            crate::store::smart::evaluate(conn, Some(lib.text_index()), &node, None, 0).unwrap();
        assert_eq!(ids.items.as_slice(), &[ia][..]);

        // Moving `animal` under `cat` would create a cycle: rejected.
        assert!(lib.set_tag_parent(animal.id, Some(cat.id)).is_err());
        // A legal move is undoable.
        lib.set_tag_parent(cat.id, None).unwrap();
        lib.undo().unwrap();
        assert_eq!(
            tags::get(conn, cat.id).unwrap().unwrap().parent_id,
            Some(animal.id)
        );

        // Deleting the parent promotes the children.
        lib.delete_tag(animal.id).unwrap();
        let cat_after = tags::get(conn, cat.id).unwrap().unwrap();
        assert_eq!(cat_after.parent_id, None);
        let _ = collections::roots(conn);
    }
    #[test]
    fn svg_import_mines_dims_and_thumbnail() {
        let (lib, dir) = temp_library("svg");
        let svg = write_source(
            &dir,
            "vector.svg",
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="480" viewBox="0 0 640 480">
                   <rect width="640" height="480" fill="#ff8000"/>
                 </svg>"##,
        );
        let report = lib
            .import_into_store(std::slice::from_ref(&svg), None)
            .unwrap();
        let asset = {
            let conn = lib.store().conn();
            assets::get(conn, report.imported[0].asset_id)
                .unwrap()
                .unwrap()
        };
        assert_eq!(asset.kind, AssetKind::Image);
        assert_eq!(asset.width, Some(640));
        assert_eq!(asset.height, Some(480));
        // The rendered thumbnail is on disk.
        let thumb = thumb::abs_path(lib.cache(), asset.content_hash.as_deref().unwrap());
        assert!(thumb.is_file(), "svg thumbnail missing");
    }
    #[test]
    fn source_path_recorded_and_filterable() {
        use crate::store::assets;

        let (lib, dir) = temp_library("folders");
        let sub = dir.join("vacation");
        std::fs::create_dir_all(&sub).unwrap();
        let a = write_source(&dir, "a.png", PNG_1X1);
        let b = sub.join("b.txt");
        std::fs::write(&b, b"beta").unwrap();

        lib.import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        lib.import_into_store(std::slice::from_ref(&b), None)
            .unwrap();

        // Only the direct parent folders, each with its live-asset count.
        let folders = assets::source_folders(lib.store().conn()).unwrap();
        assert!(
            folders
                .iter()
                .any(|(f, n)| f.ends_with("vacation") && *n == 1)
        );
        assert!(
            folders
                .iter()
                .any(|(f, n)| *f == dir.to_string_lossy() && *n == 1)
        );

        // Prefix filter narrows to the subtree of that folder.
        let q = AssetQuery {
            source_path_prefix: Some(sub.to_string_lossy().to_string()),
            ..Default::default()
        };
        let page = assets::query(lib.store().conn(), &q).unwrap();
        assert_eq!((page.total, page.items.len()), (1, 1));
        assert_eq!(page.items[0].file_name, "b.txt");
    }
    #[test]
    fn media_package_roundtrip() {
        let (lib, dir) = temp_library("pkg-export");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        let b = write_source(&dir, "beta.txt", b"beta");
        let ra = lib
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let rb = lib
            .import_into_store(std::slice::from_ref(&b), None)
            .unwrap();
        let (ia, ib) = (ra.imported[0].asset_id, rb.imported[0].asset_id);
        let coll = collections::create(
            lib.store().conn(),
            &crate::model::NewCollection {
                parent_id: None,
                name: "Trip".into(),
                position: 0,
            },
        )
        .unwrap();
        lib.add_assets_to_collection(coll.id, &[ia, ib]).unwrap();
        let tag = lib.ensure_tag("sunset").unwrap();
        lib.tag_assets(&[ia], tag.id, true).unwrap();

        // Export the package.
        let dest = dir.join("packages");
        let report = lib.export_media_package(&dest).unwrap();
        assert_eq!(report.files, 2);
        assert!(report.path.join("trove-export.json").is_file());
        let media_root = report.path.join("media");
        assert!(media_root.is_dir());
        let mut blob_count = 0;
        for entry in walk_media(&media_root) {
            if entry.is_file() {
                blob_count += 1;
            }
        }
        assert_eq!(blob_count, 2);

        // Restore into a fresh library: real blobs, not placeholders.
        let (other, _) = temp_library("pkg-import");
        let imported = other.import_media_package(&report.path).unwrap();
        assert_eq!(imported.metadata.assets_placeholder, 2);
        assert_eq!(imported.imported, 2);
        let conn = other.store().conn();
        let restored = assets::query(conn, &AssetQuery::default()).unwrap();
        assert_eq!(restored.items.len(), 2);
        assert!(
            restored
                .items
                .iter()
                .all(|x| matches!(x.location(), AssetLocation::Stored { .. }))
        );
        // Membership + tags survived.
        let image = restored
            .items
            .iter()
            .find(|x| x.kind == AssetKind::Image)
            .unwrap();
        assert_eq!(tags::for_asset(conn, image.id).unwrap().len(), 1);
        let _ = (ia, ib);
    }

    fn walk_media(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        super::collect_files(dir, &mut out);
        out
    }
    #[test]
    fn heic_import_generates_thumbnail() {
        // Sample generation needs the system heif-enc (libheif tools); the
        // thumbnail path needs heif-dec. Both are the same opt-in dependency.
        let dir = std::env::temp_dir().join(format!("trove-heic-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("sample.png");
        {
            // A tiny valid PNG: reuse the constant.
            std::fs::write(&png, PNG_1X1).unwrap();
        }
        let heic = dir.join("sample.heic");
        let enc = std::process::Command::new("heif-enc")
            .arg(&png)
            .arg("-o")
            .arg(&heic)
            .output();
        let Ok(enc) = enc else {
            eprintln!("heif-enc not available, skipping HEIC test");
            return;
        };
        if !enc.status.success() {
            eprintln!("heif-enc failed, skipping HEIC test");
            return;
        }

        let (lib, _) = temp_library("heic");
        let report = lib
            .import_into_store(std::slice::from_ref(&heic), None)
            .unwrap();
        let asset = {
            let conn = lib.store().conn();
            assets::get(conn, report.imported[0].asset_id)
                .unwrap()
                .unwrap()
        };
        assert_eq!(asset.kind, AssetKind::Image);
        let thumb = thumb::abs_path(lib.cache(), asset.content_hash.as_deref().unwrap());
        assert!(thumb.is_file(), "heic thumbnail missing");
    }

    #[test]
    fn raw_sample_import_optin() {
        let Ok(sample) = std::env::var("TROVE_RAW_SAMPLE") else {
            eprintln!("set TROVE_RAW_SAMPLE to run the RAW import test");
            return;
        };
        let (lib, _) = temp_library("raw-sample");
        let path = std::path::PathBuf::from(sample);
        let report = lib
            .import_into_store(std::slice::from_ref(&path), None)
            .unwrap();
        assert_eq!(report.imported_count(), 1, "skipped: {:?}", report.skipped);
        let conn = lib.store().conn();
        let asset = assets::get(conn, report.imported[0].asset_id)
            .unwrap()
            .unwrap();
        assert_eq!(asset.kind, AssetKind::Image);
        assert!(asset.width.unwrap_or(0) > 0);
        let thumb = thumb::abs_path(lib.cache(), asset.content_hash.as_deref().unwrap());
        assert!(thumb.is_file(), "raw thumbnail missing");
    }

    /// A 4×3 red PNG on disk — wide enough that a rotation visibly swaps
    /// the reported dimensions (a square would not).
    fn write_wide_png(dir: &Path, name: &str) -> PathBuf {
        let mut img = image::RgbaImage::new(4, 3);
        for y in 0..3 {
            for x in 0..4 {
                img.put_pixel(x, y, image::Rgba([200, 30, 30, 255]));
            }
        }
        let path = dir.join(name);
        img.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        path
    }

    /// A linked asset's edit rewrites the file where it lives and the
    /// record follows the new content — same path, same origin, no blob.
    #[test]
    fn batch_edit_writes_a_linked_file_back_in_place() {
        let (lib, _root) = temp_library("edit-linked");
        // The user's own directory, outside the library root: linking is
        // what makes this the file the user keeps.
        let home = std::env::temp_dir().join(format!("trove-edit-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let src = write_wide_png(&home, "kept.png");
        let original_bytes = std::fs::read(&src).unwrap();

        let report = lib.link_files(std::slice::from_ref(&src), None).unwrap();
        let id = report.imported[0].asset_id;
        let conn = lib.store().conn();
        let before = assets::get(conn, id).unwrap().unwrap();
        assert!(matches!(
            before.location(),
            crate::model::AssetLocation::Linked { .. }
        ));
        let old_hash = before.content_hash.clone().unwrap();
        assert!(thumb::abs_path(lib.cache(), &old_hash).is_file());

        let out = lib
            .batch_edit_images(&[id], &[crate::media::edit::ImageEdit::Rotate90], 90)
            .unwrap();
        assert_eq!(out.edited, 1, "failures: {:?}", out.failures);

        // The original file now holds the rotated picture — same path, PNG
        // still, and no longer the bytes it started with.
        let rewritten = image::image_dimensions(&src).unwrap();
        assert_eq!(rewritten, (3, 4), "the file itself rotated");
        assert_ne!(std::fs::read(&src).unwrap(), original_bytes);

        // The record moved with the content; the link columns did not.
        let after = assets::get(conn, id).unwrap().unwrap();
        assert!(matches!(
            after.location(),
            crate::model::AssetLocation::Linked { .. }
        ));
        assert_eq!(
            after.facts.source_path.as_deref(),
            Some(src.to_str().unwrap())
        );
        assert_ne!(after.content_hash.as_deref(), Some(old_hash.as_str()));
        assert_eq!((after.width, after.height), (Some(3), Some(4)));
        assert!(thumb::abs_path(lib.cache(), after.content_hash.as_deref().unwrap()).is_file());
        assert!(
            !thumb::abs_path(lib.cache(), &old_hash).is_file(),
            "the old thumbnail describes content nothing references"
        );

        // No temp siblings survive the write.
        let litter: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("trove-edit"))
            .collect();
        assert!(
            litter.is_empty(),
            "temp write-back files left behind: {litter:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// A linked asset whose recorded source path is *gone* is a per-asset
    /// failure, not a silent skip: the user asked for this edit, so the
    /// report must say it did not happen. (Relinking is the fix.)
    #[test]
    fn batch_edit_reports_a_linked_asset_whose_file_vanished() {
        let (lib, root) = temp_library("edit-linked-missing");
        let src = write_wide_png(&root, "gone.png");
        let report = lib.link_files(std::slice::from_ref(&src), None).unwrap();
        let id = report.imported[0].asset_id;
        std::fs::remove_file(&src).unwrap();

        let out = lib
            .batch_edit_images(&[id], &[crate::media::edit::ImageEdit::Rotate90], 90)
            .unwrap();
        assert_eq!(out.edited, 0);
        assert_eq!(out.skipped, 0, "failures: {:?}", out.failures);
        assert_eq!(out.failures.len(), 1);
        assert_eq!(out.failures[0].0, id);
    }

    #[test]
    fn batch_edit_rotates_and_swaps_content_in_place() {
        let (lib, root) = temp_library("batch-edit");
        let src = write_wide_png(&root, "wide.png");
        let report = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let id = report.imported[0].asset_id;
        let conn = lib.store().conn();
        let before = assets::get(conn, id).unwrap().unwrap();
        let old_hash = before.content_hash.clone().unwrap();
        let old_rel = stored_rel(&before);
        let old_thumb = thumb::abs_path(lib.cache(), &old_hash);
        assert!(old_thumb.is_file(), "precondition: thumbnail exists");

        let out = lib
            .batch_edit_images(&[id], &[crate::media::edit::ImageEdit::Rotate90], 90)
            .unwrap();
        assert_eq!(out.edited, 1, "failures: {:?}", out.failures);
        assert_eq!(out.skipped, 0);

        let after = assets::get(conn, id).unwrap().unwrap();
        // Identity survives; content does not.
        assert_eq!(after.file_name, before.file_name);
        assert_eq!(after.title, before.title);
        assert_ne!(after.content_hash, before.content_hash);
        assert_eq!((after.width, after.height), (Some(3), Some(4)));
        assert_eq!(after.ext, before.ext);
        assert_eq!(after.mime, "image/png");

        // The old blob and its thumbnail are gone, the new ones exist.
        assert!(!lib.resolve(&old_rel).is_file(), "old blob removed");
        assert!(!old_thumb.is_file(), "old thumbnail removed");
        let new_rel = stored_rel(&after);
        assert!(lib.resolve(&new_rel).is_file(), "new blob exists");
        let new_thumb = thumb::abs_path(lib.cache(), after.content_hash.as_deref().unwrap());
        assert!(new_thumb.is_file(), "new thumbnail generated");

        // The pixel content is really rotated: decoding the new blob gives
        // the swapped geometry.
        use image::GenericImageView as _;
        let decoded = image::open(lib.resolve(&new_rel)).unwrap();
        assert_eq!(decoded.dimensions(), (3, 4));
    }

    #[test]
    fn batch_edit_skips_and_rejects_appropriately() {
        let (lib, root) = temp_library("batch-edit-mixed");
        let src = write_wide_png(&root, "wide.png");
        let report = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let stored_id = report.imported[0].asset_id;

        // A linked asset: its edit writes back to the file it links to (the
        // rewrite is what makes the file's owner the decision-maker, and the
        // UI confirms before handing a batch to this path).
        let linked_src = write_source(&root, "linked.png", PNG_1X1);
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

        let staged = stage_all(
            &root,
            &root.join("cache"),
            std::slice::from_ref(&linked_src),
            ImportStorage::Link,
            &std::sync::atomic::AtomicBool::new(false),
        );
        commit_staged_all(lib.store().conn(), None, staged);
        let conn = lib.store().conn();
        let linked_id = {
            let page = assets::query(conn, &AssetQuery::default()).unwrap();
            page.items
                .into_iter()
                .find(|a| a.location().is_linked())
                .expect("linked asset imported")
                .id
        };

        let linked_before = std::fs::read(&linked_src).unwrap();
        let out = lib
            .batch_edit_images(
                &[stored_id, linked_id, Uuid::new_v4()],
                &[crate::media::edit::ImageEdit::FlipHorizontal],
                90,
            )
            .unwrap();
        // The stored asset swaps blobs, the linked one rewrites its file in
        // place; only the missing id is skipped (no record at all).
        assert_eq!(out.edited, 2, "failures: {:?}", out.failures);
        assert_eq!(out.skipped, 1);
        assert!(out.failures.is_empty());
        assert_ne!(
            std::fs::read(&linked_src).unwrap(),
            linked_before,
            "the linked file was rewritten"
        );
    }

    #[test]
    fn xmp_sidecar_export_writes_next_to_the_blob() {
        let (lib, root) = temp_library("xmp-export");
        let src = write_wide_png(&root, "wide.png");
        let report = lib
            .import_into_store(std::slice::from_ref(&src), None)
            .unwrap();
        let id = report.imported[0].asset_id;
        let conn = lib.store().conn();

        lib.patch_asset(
            id,
            &crate::model::AssetPatch {
                title: Some(Some("Sunset & <beach>".into())),
                description: Some(Some("Golden hour".into())),
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap();
        let tag = lib.ensure_tag("sea").unwrap();
        lib.tag_assets(&[id], tag.id, true).unwrap();

        let out = lib.export_xmp_sidecars(&[id]).unwrap();
        assert_eq!(out.written, 1);
        assert_eq!(out.skipped, 0);

        let asset = assets::get(conn, id).unwrap().unwrap();
        let sidecar = lib.resolve(&stored_rel(&asset)).with_extension("xmp");
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.contains("Sunset &amp; &lt;beach&gt;"));
        assert!(body.contains("Golden hour"));
        assert!(body.contains("<rdf:li>sea</rdf:li>"));
        assert!(body.contains("<xmp:Rating>5</xmp:Rating>"));

        // Re-export overwrites in place; a trashed asset is skipped.
        assert_eq!(lib.export_xmp_sidecars(&[id]).unwrap().written, 1);
        lib.trash_assets(&[id]).unwrap();
        let out = lib.export_xmp_sidecars(&[id]).unwrap();
        assert_eq!(out.written, 0);
        assert_eq!(out.skipped, 1);
    }

    // -- AI embeddings ---------------------------------------------------------

    /// The full AI-vector round trip through the facade: backfill on the
    /// task manager, coverage on the settings page, semantic search through
    /// the same rank-and-page pipeline as text search, and the reset button.
    #[test]
    fn embedding_backfill_semantic_search_and_reset() {
        let (lib, root) = temp_library("embeddings");
        let conn = lib.store().conn();

        // Three assets with distinct titles; the mock maps each title to a
        // stable pseudo-random vector.
        let titles = ["red car in snow", "blue boat at sea", "green tree on hill"];
        for title in titles {
            let asset =
                crate::model::test_asset(&format!("{title}.png"), AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &asset).unwrap();
            lib.patch_asset(
                asset.id,
                &crate::model::AssetPatch {
                    title: Some(Some(title.into())),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let provider: std::sync::Arc<dyn crate::ai::EmbeddingProvider> =
            std::sync::Arc::new(crate::ai::MockProvider::new("mock-embed", 16));

        // Coverage is zero before any backfill.
        assert_eq!(lib.embedding_coverage("mock-embed").unwrap(), (0, 3));

        // Backfill on the task manager; wait for the outcome channel.
        let (task_id, rx) = lib.start_embedding_backfill(provider.clone()).unwrap();
        let outcome = rx.recv().expect("the job returns an outcome");
        assert_eq!(outcome.embedded, 3, "{outcome:?}");
        assert_eq!(outcome.error, None);
        assert!(lib.tasks().snapshot().iter().any(|t| t.id == task_id));

        assert_eq!(lib.embedding_coverage("mock-embed").unwrap(), (3, 3));

        // Semantic search ranks the exact-title asset first: the query text
        // embeds to the same vector the asset's title did.
        for title in titles {
            let page = lib
                .semantic_search(
                    provider.as_ref(),
                    title,
                    &AssetQuery {
                        kind: Some(AssetKind::Image),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(page.total, 3, "the cap feeds every vector to the filters");
            assert_eq!(
                page.items[0].title.as_deref(),
                Some(title),
                "query {title:?} must rank its own asset first"
            );
        }

        // An empty query is an empty page, not a scan.
        let page = lib
            .semantic_search(provider.as_ref(), "   ", &AssetQuery::default())
            .unwrap();
        assert!(page.items.is_empty() && page.total == 0);

        // A structural filter still applies to the semantic candidates.
        let page = lib
            .semantic_search(
                provider.as_ref(),
                "red car in snow",
                &AssetQuery {
                    kind: Some(AssetKind::Document),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(page.total, 0, "no documents in an image library");

        // The reset button clears one model and leaves no vectors behind;
        // coverage reports zero again.
        assert_eq!(lib.delete_embeddings("mock-embed").unwrap(), 3);
        assert_eq!(lib.embedding_coverage("mock-embed").unwrap(), (0, 3));
        let page = lib
            .semantic_search(provider.as_ref(), "red car in snow", &AssetQuery::default())
            .unwrap();
        assert_eq!(page.total, 0);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A frame of a run: an image asset whose `source_path` says which folder it
    /// came from, since that is what the grouping rule reads.
    fn frame(dir: &Path, name: &str, id: Uuid) -> crate::model::Asset {
        let path = dir.join(name);
        let mut asset = crate::model::test_asset(name, AssetKind::Image, id);
        asset.facts.source_path = Some(path.to_string_lossy().to_string());
        (asset.width, asset.height) = (Some(1920), Some(1080));
        asset
    }

    /// The facade is the only thing between the grid's selection and the two side
    /// tables, so it is tested at that level: every refusal has to name the
    /// reason, because the menu shows that sentence to the user, and the read
    /// back has to agree with what went in.
    #[test]
    fn sequences_can_be_grouped_and_ungrouped_through_the_facade() {
        let (lib, root) = temp_library("sequences");
        let dir = root.join("renders");
        std::fs::create_dir_all(&dir).unwrap();
        let ids: Vec<Uuid> = (0..4).map(|_| crate::model::new_id()).collect();
        for (ix, id) in ids.iter().enumerate() {
            let name = format!("shot_{:03}.png", ix + 1);
            assets::insert(lib.store.conn(), &frame(&dir, &name, *id)).unwrap();
        }

        // Refusals first, while nothing is a member yet, so each one fails for
        // the reason it is meant to.
        assert!(
            lib.create_sequence(&ids, 0.0).is_err(),
            "a rate of zero is not a rate"
        );
        assert!(
            lib.create_sequence(&ids[..2], 24.0).is_err(),
            "two frames is not a run"
        );
        let stray = crate::model::new_id();
        assets::insert(
            lib.store.conn(),
            &frame(&root.join("other"), "elsewhere.png", stray),
        )
        .unwrap();
        let mut cross = ids[..2].to_vec();
        cross.push(stray);
        assert!(
            lib.create_sequence(&cross, 24.0).is_err(),
            "a run that spans folders is two shots"
        );

        let seq = lib.create_sequence(&ids, 24.0).unwrap();
        let first = lib
            .sequence_of(ids[0])
            .unwrap()
            .expect("the first frame is a member");
        assert_eq!(first.sequence_id, seq);
        assert_eq!(first.frames.len(), 4, "the run reads back whole");
        assert_eq!(first.position, 0, "shot_001 is the card the grid shows");
        assert_eq!(first.fps(), 24.0);
        assert_eq!(
            lib.sequence_of(ids[3]).unwrap().unwrap().position,
            3,
            "the numbered order survived, not the selection order"
        );

        lib.set_sequence_fps(seq, 12.0).unwrap();
        assert_eq!(lib.sequence_of(ids[0]).unwrap().unwrap().fps(), 12.0);

        // A frame already in a run cannot start a second one.
        assert!(
            lib.create_sequence(&[ids[1], stray, ids[2]], 24.0).is_err(),
            "a member was re-grouped"
        );

        // Dissolving by selecting *any* frame reaches the whole run — that is
        // what makes the menu item work on a hidden member too.
        assert_eq!(lib.dissolve_for_assets(&[ids[2]]).unwrap(), 1);
        assert!(
            lib.sequence_of(ids[0]).unwrap().is_none(),
            "the frames are ordinary assets again"
        );
        // Dissolving what is already gone is not an error and dissolves nothing.
        assert_eq!(lib.dissolve_for_assets(&[ids[2]]).unwrap(), 0);

        std::fs::remove_dir_all(&root).ok();
    }
}
