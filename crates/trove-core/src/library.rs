//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::history::undo::{self, Op, OpAction, OpDesc, SharedUndoStack};
use crate::media;
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
    /// Records whose content (SHA-256) already lives in the library: the
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
        let lib = Self {
            store,
            root,
            cache,
            undo: SharedUndoStack::with_cap(crate::config::AppConfig::load().undo_cap()),
            tasks: std::sync::Arc::new(crate::tasks::TaskManager::new()),
            text_index,
            vector_index: std::cell::RefCell::new(None),
        };
        // Reconcile the search index with the asset rows: a fresh, wiped or
        // outdated index re-derives itself from the store here, so `search`
        // never silently returns nothing for assets that predate it.
        lib.reconcile_search_index()?;
        // Health headline: a library is open and here is its size — the two
        // gauges a /health scrape leads with. A failed count is not worth
        // failing the open over; the gauge just stays at zero.
        let assets =
            rows::query_count(lib.store.conn(), "SELECT COUNT(*) FROM assets", vec![]).unwrap_or(0);
        crate::metrics::set_library_open(assets.max(0) as u64);
        // Daily safety snapshot (24h throttle, rolling 10 files). Best-effort:
        // a failed backup never blocks opening the library.
        crate::services::backup::maybe_auto_backup(&lib.root, lib.store.conn());
        Ok(lib)
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
            vector_index: std::cell::RefCell::new(None),
        })
    }

    /// The background task manager. One running job per kind; progress and
    /// lifecycle events are polled from the UI side.
    pub fn tasks(&self) -> &crate::tasks::TaskManager {
        &self.tasks
    }

    pub fn store(&self) -> &Store {
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

    /// The real file behind `id`: the in-library blob for stored assets, the
    /// linked original (recorded at import) for linked ones. `None` when the
    /// record is missing or the file no longer exists.
    pub fn asset_file(&self, id: Uuid) -> Option<std::path::PathBuf> {
        let asset = assets::get(self.store.conn(), id).ok().flatten()?;
        let path = match asset.origin {
            crate::model::Origin::Linked => std::path::PathBuf::from(asset.facts.source_path?),
            crate::model::Origin::Stored => self.root.join(asset.rel_path?),
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
        if text.trim().is_empty() {
            return Ok(crate::model::Page::new(0, Vec::new()));
        }
        // Pending outbox rows flush before the lookup, so a just-committed
        // mutation is visible to the same search.
        self.drain_search_queue()?;
        let t_drain = t0.elapsed();
        let candidates = self.text_index.search(text, crate::search::CANDIDATE_CAP)?;
        let t_index = t0.elapsed();
        // Free text is entirely the index's business; `q` only carries the
        // structural filters, so it goes straight into the SQL intersection.
        let (total, ids) = assets::rank_intersect(conn, &candidates, q)?;
        let t_rank = t0.elapsed();
        let page = assets::page_assets(&ids, q, conn)?;
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
        Ok(crate::model::Page::new(total, page))
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
        };
        let label = format!("embedding backfill ({})", provider.id());
        self.tasks.start(
            crate::tasks::TaskKind::EmbeddingBackfill,
            label,
            move |ctx| crate::tasks::embed::run(&options, provider.as_ref(), ctx),
        )
    }

    /// Semantic search: embed `query` with `provider`, score the model's
    /// stored vectors by cosine, and narrow the top candidates with `q`'s
    /// structural filters — the same rank-intersect-then-page pipeline the
    /// full-text search uses. An empty query is an empty page, not a scan.
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
        let index = self.vector_index_for(provider);
        let candidates =
            index.search(conn, &vector, crate::search::vector::VECTOR_CANDIDATE_CAP)?;
        let ranked: Vec<Uuid> = candidates.into_iter().map(|m| m.asset_id).collect();
        let (total, ids) = assets::rank_intersect(conn, &ranked, q)?;
        let page = assets::page_assets(&ids, q, conn)?;
        crate::metrics::note_vector_search();
        crate::metrics::note_query(started.elapsed());
        Ok(crate::model::Page::new(total, page))
    }

    /// The cached index for `provider`'s model+space, rebuilt when the
    /// provider changes. Drift inside one model (a backfill finishing, an
    /// asset deleted) is the index's own fingerprint check, not this cache's.
    fn vector_index_for(
        &self,
        provider: &dyn crate::ai::EmbeddingProvider,
    ) -> crate::search::vector::VectorIndex {
        let mut cached = self.vector_index.borrow_mut();
        let stale = match cached.as_ref() {
            Some((model, space, _)) => model != provider.id() || *space != provider.asset_space(),
            None => true,
        };
        if stale {
            *cached = Some((
                provider.id().to_string(),
                provider.asset_space(),
                crate::search::vector::VectorIndex::new(provider.id(), provider.asset_space()),
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
    /// once no other asset references the same content hash.
    pub fn purge_asset(&self, asset_id: Uuid) -> Result<()> {
        let conn = self.store.conn();
        let Some(asset) = assets::get(conn, asset_id)? else {
            return Err(crate::Error::NotFound("asset"));
        };
        let sha = asset.sha256.clone();
        let rel = asset.rel_path.clone();
        assets::delete(conn, asset_id)?;
        if let (Some(sha), Some(rel)) = (sha, rel)
            && assets::count_by_sha256(conn, &sha)? == 0
        {
            self.remove_blob_files(&rel, &sha);
        }
        Ok(())
    }

    /// Permanently delete every trashed asset. Returns the number removed.
    pub fn empty_trash(&self) -> Result<u64> {
        let conn = self.store.conn();
        let page = assets::query(
            conn,
            &crate::model::AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )?;
        let mut removed = 0u64;
        for asset in page.items {
            let id = asset.id;
            let sha = asset.sha256.clone();
            let rel = asset.rel_path.clone();
            assets::delete(conn, id)?;
            if let (Some(sha), Some(rel)) = (sha, rel)
                && assets::count_by_sha256(conn, &sha)? == 0
            {
                self.remove_blob_files(&rel, &sha);
            }
            removed += 1;
        }
        Ok(removed)
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
            let Some(rel) = &asset.rel_path else { continue };
            let src = self.root.join(rel);
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

    /// Group live assets with identical content (SHA-256). The UI offers
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

    /// Re-point a linked asset at a moved file. The chosen file must hash
    /// to the same SHA-256 as the one recorded at import — relinking
    /// reconnects a *moved* file, it never swaps content (import the new
    /// file instead when the original is truly gone).
    pub fn relink_asset(&self, asset_id: Uuid, new_path: &Path) -> Result<()> {
        let conn = self.store.conn();
        let asset = assets::get(conn, asset_id)?.ok_or(crate::Error::NotFound("asset"))?;
        if asset.origin != crate::model::Origin::Linked || asset.rel_path.is_some() {
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
        let (sha, _) = crate::media::blob::hash_file(new_path)?;
        let recorded = asset.sha256.as_deref().unwrap_or_default();
        if !sha.eq_ignore_ascii_case(recorded) {
            return Err(crate::Error::Validation(format!(
                "content mismatch: recorded sha256 {recorded}, found {sha}"
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
        if asset.origin == crate::model::Origin::Linked {
            return self.edit_linked_in_place(&asset, edits, jpeg_quality);
        }
        if asset.rel_path.is_none() {
            return Ok(false);
        }

        let source = self
            .root
            .join(asset.rel_path.as_deref().unwrap_or_default());
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
        let Some(source) = asset.facts.source_path.as_ref().map(PathBuf::from) else {
            // No reachable original (moved, or never recorded): skip the
            // asset — relinking is the fix, not an error toast.
            return Ok(false);
        };
        let out = media::edit::apply(&source, edits, jpeg_quality)?;
        let sha = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&out.bytes);
            crate::media::blob::hex(hasher.finalize().as_slice())
        };
        if asset
            .sha256
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(&sha))
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

        let old_sha = asset.sha256.clone().unwrap_or_default();
        assets::set_linked_media_columns(
            self.store.conn(),
            asset.id,
            &sha,
            out.bytes.len() as u64,
            Some(out.width),
            Some(out.height),
        )?;

        // The old thumbnail described content no record references anymore
        // once the last asset on that hash is gone; the new one is rebuilt
        // from the file where it lives.
        if !old_sha.is_empty() && assets::count_by_sha256(self.store.conn(), &old_sha)? == 0 {
            let _ = std::fs::remove_file(media::thumb::abs_path(self.cache(), &old_sha));
        }
        media::thumb::regenerate(self.cache(), &sha, asset.kind, &source);
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
        let old_sha = asset.sha256.clone().unwrap_or_default();
        let old_rel = asset.rel_path.clone();
        if staged.sha256.eq_ignore_ascii_case(&old_sha) {
            // The edits produced byte-identical content: the blob in place
            // is already correct.
            return Ok(());
        }

        let new_blob = self.root.join(&staged.rel_path);
        assets::set_media_columns(
            self.store.conn(),
            id,
            &staged.sha256,
            &staged.rel_path,
            staged.size,
            Some(width),
            Some(height),
        )?;

        // Free the old content when this was the last reference to it.
        if assets::count_by_sha256(self.store.conn(), &old_sha)? == 0
            && let Some(rel) = &old_rel
        {
            self.remove_blob_files(rel, &old_sha);
        }

        // Thumbnail and visual fingerprint describe the old pixels; both
        // must follow the content to its new hash.
        media::thumb::regenerate(self.cache(), &staged.sha256, asset.kind, &new_blob);
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
            let target = match asset.origin {
                crate::model::Origin::Stored => {
                    asset.rel_path.as_ref().map(|rel| self.root.join(rel))
                }
                crate::model::Origin::Linked => asset.facts.source_path.as_ref().map(PathBuf::from),
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
    /// content (SHA-256) already exists are linked, everything else becomes
    /// a placeholder record that self-heals when the file is re-imported
    /// (content-addressed storage keys both paths by hash).
    pub fn import_metadata(&self, json: &str) -> Result<MetadataImportReport> {
        use crate::model::{NewSmartCollection, Origin};

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
                color: sc.color.clone(),
                position: sc.position,
            };
            if input.validate().is_ok() && smart::validate_json(&input.query).is_ok() {
                match smart_collections::create(conn, &input) {
                    Ok(created) => {
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
            if let Some(sha) = &asset.sha256
                && let Some(existing) = assets::find_by_sha256(conn, sha)?
            {
                asset_map.insert(asset.id, existing.id);
                report.assets_linked += 1;
                continue;
            }
            let id = Uuid::new_v4();
            let placeholder = crate::model::Asset {
                id,
                origin: Origin::Stored,
                rel_path: None,
                file_name: asset.file_name.clone(),
                ext: asset.ext.clone(),
                mime: asset.mime.clone(),
                size_bytes: asset.size_bytes,
                sha256: asset.sha256.clone(),
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
            };
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
    pub fn purge_assets(&self, ids: &[Uuid]) -> Result<PurgeReport> {
        // Track (rel, sha) for every content hash left unreferenced by this
        // purge, so the file is deleted exactly once even when several deleted
        // assets shared it.
        let mut freed: Vec<(String, String)> = Vec::new();
        let purged = self.store.transaction(|tx| {
            let mut freed_tx: Vec<(String, String)> = Vec::new();
            for id in ids {
                let Some(asset) = assets::get(tx, *id)? else {
                    continue;
                };
                let sha = asset.sha256.clone();
                let rel = asset.rel_path.clone();
                assets::delete(tx, *id)?;
                if let (Some(sha), Some(rel)) = (sha, rel)
                    && assets::count_by_sha256(tx, &sha)? == 0
                {
                    freed_tx.push((rel, sha));
                }
            }
            freed = freed_tx;
            Ok(ids.len() as u64)
        })?;

        let mut report = PurgeReport {
            purged,
            ..Default::default()
        };
        for (rel, sha) in freed {
            if rel.starts_with("media/") {
                report.blobs_removed += 1;
            }
            report.thumbs_removed += 1;
            self.remove_blob_files(&rel, &sha);
        }
        Ok(report)
    }

    /// Best-effort removal of a content-addressed blob and its thumbnail.
    /// Only called once the content is unreferenced.
    fn remove_blob_files(&self, rel: &str, sha: &str) {
        if rel.starts_with("media/") {
            let _ = std::fs::remove_file(self.root.join(rel));
        }
        let thumb = media::thumb::abs_path(&self.cache, sha);
        let _ = std::fs::remove_file(thumb);
    }
}

#[cfg(test)]
mod tests {
    use super::Library;
    use crate::media::thumb;
    use crate::model::{AssetKind, AssetQuery, NewCollection, NewSmartCollection};
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

    fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn relink_asset_repoints_a_moved_file() {
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};
        use crate::model::Origin;

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
        assert_eq!(all.items[0].origin, Origin::Linked);

        // Move the file elsewhere, then reconnect the record to it.
        let moved = outside.join("moved-elsewhere.png");
        std::fs::rename(&src, &moved).unwrap();
        lib.relink_asset(id, &moved).unwrap();
        let asset = assets::get(conn, id).unwrap().unwrap();
        assert_eq!(
            asset.facts.source_path.as_deref(),
            Some(moved.display().to_string().as_str())
        );
        assert_eq!(asset.sha256.as_deref(), all.items[0].sha256.as_deref());

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
            .find(|a| a.origin != Origin::Linked)
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
        assert!(asset.sha256.is_some());
        assert_eq!(asset.file_name, "photo.png");

        // The blob exists on disk under a content-addressed name.
        let rel = asset.rel_path.as_ref().expect("stored asset has rel_path");
        assert!(lib.resolve(rel).is_file());

        // A JPEG thumbnail was generated next to it.
        let thumb_path = thumb::abs_path(lib.cache(), asset.sha256.as_deref().unwrap());
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
        let blob = lib.resolve(stored.rel_path.as_ref().unwrap());

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
        let sha = stored.sha256.unwrap();
        assert!(!thumb::abs_path(lib.cache(), &sha).exists());
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
                color: None,
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
                color: None,
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
        // so two live assets never share a SHA-256 — the duplicate finder
        // works on perceptual hashes instead.
        let (lib, dir) = temp_library("duplicates-sha");
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
                color: None,
                position: 0,
            })
            .unwrap();
        let outer = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: None,
                name: "outer".into(),
                query: fav.clone(),
                color: None,
                position: 1,
            })
            .unwrap();
        let inner = lib
            .create_smart_collection(&NewSmartCollection {
                parent_id: Some(outer.id),
                name: "inner".into(),
                query: fav,
                color: None,
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
        assert!(restored.items[0].rel_path.is_none());

        // Re-importing the same content links the blob into the placeholder.
        other
            .import_into_store(std::slice::from_ref(&a), None)
            .unwrap();
        let healed = assets::get(conn, restored.items[0].id).unwrap().unwrap();
        assert!(healed.rel_path.is_some());
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
        let thumb = thumb::abs_path(lib.cache(), asset.sha256.as_deref().unwrap());
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
        assert!(restored.items.iter().all(|x| x.rel_path.is_some()));
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
        let thumb = thumb::abs_path(lib.cache(), asset.sha256.as_deref().unwrap());
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
        let thumb = thumb::abs_path(lib.cache(), asset.sha256.as_deref().unwrap());
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
        assert_eq!(before.origin, crate::model::Origin::Linked);
        assert!(before.rel_path.is_none());
        let old_sha = before.sha256.clone().unwrap();
        assert!(thumb::abs_path(lib.cache(), &old_sha).is_file());

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
        assert_eq!(after.origin, crate::model::Origin::Linked);
        assert!(after.rel_path.is_none());
        assert_eq!(
            after.facts.source_path.as_deref(),
            Some(src.to_str().unwrap())
        );
        assert_ne!(after.sha256.as_deref(), Some(old_sha.as_str()));
        assert_eq!((after.width, after.height), (Some(3), Some(4)));
        assert!(thumb::abs_path(lib.cache(), after.sha256.as_deref().unwrap()).is_file());
        assert!(
            !thumb::abs_path(lib.cache(), &old_sha).is_file(),
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
        let old_sha = before.sha256.clone().unwrap();
        let old_rel = before.rel_path.clone().unwrap();
        let old_thumb = thumb::abs_path(lib.cache(), &old_sha);
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
        assert_ne!(after.sha256, before.sha256);
        assert_eq!((after.width, after.height), (Some(3), Some(4)));
        assert_eq!(after.ext, before.ext);
        assert_eq!(after.mime, "image/png");

        // The old blob and its thumbnail are gone, the new ones exist.
        assert!(!lib.resolve(&old_rel).is_file(), "old blob removed");
        assert!(!old_thumb.is_file(), "old thumbnail removed");
        let new_rel = after.rel_path.as_ref().unwrap();
        assert!(lib.resolve(new_rel).is_file(), "new blob exists");
        let new_thumb = thumb::abs_path(lib.cache(), after.sha256.as_deref().unwrap());
        assert!(new_thumb.is_file(), "new thumbnail generated");

        // The pixel content is really rotated: decoding the new blob gives
        // the swapped geometry.
        use image::GenericImageView as _;
        let decoded = image::open(lib.resolve(new_rel)).unwrap();
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
        use crate::model::Origin;
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
                .find(|a| a.origin == Origin::Linked)
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
        let sidecar = lib
            .resolve(asset.rel_path.as_ref().unwrap())
            .with_extension("xmp");
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
}
