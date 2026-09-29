//! Lifecycle and reads: opening, backups, statistics, the text index, and the per-asset read paths.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

impl Library {
    /// Write a backup snapshot of the database now (also prunes old ones).
    pub fn create_backup(&self) -> Result<std::path::PathBuf> {
        crate::services::backup::create_backup(&self.root, self.store.conn())
    }

    /// Backup snapshots of this library, oldest first.
    pub fn list_backups(&self) -> Vec<std::path::PathBuf> {
        crate::services::backup::list_backups(&self.root)
    }

    /// Write `snapshot` back over this library's database, and return the
    /// snapshot taken of the state being replaced (so the restore itself is
    /// undoable). A snapshot holds records only — files under the library root
    /// that a later delete removed do not come back, and the text index has to
    /// be rebuilt against the restored rows. See [`crate::services::backup`].
    pub fn restore_backup(&self, snapshot: &std::path::Path) -> Result<std::path::PathBuf> {
        crate::services::backup::restore_backup(&self.root, snapshot, self.store.conn())
    }

    /// Library statistics for the settings dashboard.
    pub fn stats(&self) -> Result<crate::store::stats::LibraryStats> {
        crate::store::stats::library_stats(self.store.conn())
    }

    /// How many live assets the library currently holds — the number the
    /// license gate measures against its free-tier cap. Trashed rows do not
    /// count (emptying the trash reopens the door), and one indexed COUNT(*)
    /// is cheap enough to read at import time rather than cache and drift.
    pub fn asset_count(&self) -> u64 {
        crate::store::rows::query_count(
            self.store.conn(),
            &format!("SELECT COUNT(*) FROM assets WHERE {}", crate::store::LIVE_ROWS),
            vec![],
        )
        .unwrap_or(0)
        .max(0) as u64
    }

    pub(super) fn reconcile_search_index(&self) -> Result<()> {
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
            undo: UndoHistory::with_cap(crate::history::undo::DEFAULT_UNDO_CAP),
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
}
