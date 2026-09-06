//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::media;
use crate::store::{assets, batch, rows, smart, smart_collections, Store};

/// Outcome of permanently deleting a batch of assets.
#[derive(Debug, Clone, Default)]
pub struct PurgeReport {
    pub purged: u64,
    pub blobs_removed: u64,
    pub thumbs_removed: u64,
}

/// A Trove library on disk:
///
/// ```text
/// <root>/
/// ├── library.db
/// └── media/…            # content-addressed blobs
/// ```
#[derive(Clone)]
pub struct Library {
    store: Store,
    root: PathBuf,
}

impl Library {
    /// Open (or create) the library under `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let store = Store::open(&root.join("library.db"))?;
        let lib = Self { store, root };
        // Backfill the FTS index for a library migrated from a schema that had
        // no search table: without this, `search` silently returns nothing for
        // assets that predate the index.
        lib.backfill_search_on_migration()?;
        Ok(lib)
    }

    /// The FTS index mirrors the asset rows and is kept in sync on every write,
    /// so this only matters on the one migration from v1 -> v2. Cheap no-op
    /// on a healthy library: `asset_fts` is non-empty as soon as any asset was
    /// ever indexed.
    fn backfill_search_on_migration(&self) -> Result<()> {
        let conn = self.store.conn();
        let indexed = rows::query_count(conn, "SELECT COUNT(*) FROM asset_fts", vec![])?;
        if indexed > 0 {
            return Ok(());
        }
        let (asset_count, _) = assets::query(conn, &crate::model::AssetQuery::default())?;
        if asset_count > 0 {
            // Best-effort: a rebuild failure must not prevent the library from
            // opening. Fresh imports re-sync the index via the write path.
            let _ = crate::maintenance::rebuild_search_index(self);
        }
        Ok(())
    }

    /// An in-memory library whose media blobs live under `root` (tests).
    pub fn open_in_memory(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let store = Store::in_memory()?;
        Ok(Self { store, root })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of a library-relative path (e.g. a stored `rel_path`).
    pub fn resolve(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Import files, optionally into a collection. See
    /// [`media::import::import_files`] for semantics.
    pub fn import_files(
        &self,
        sources: &[PathBuf],
        into_collection: Option<Uuid>,
    ) -> Result<media::import::ImportReport> {
        media::import::import_files(&self.store, &self.root, sources, into_collection)
    }

    /// Import files into a fixed collection (optional) while auto-grouping
    /// each file into an auto-created root collection by `auto`.
    pub fn import_files_auto(
        &self,
        sources: &[PathBuf],
        into_collection: Option<Uuid>,
        auto: media::import::AutoCollection,
    ) -> Result<media::import::ImportReport> {
        media::import::import_files_assigned(
            &self.store,
            &self.root,
            sources,
            into_collection,
            Some(auto),
        )
    }

    /// Full-text search across live assets, ordered by relevance. `q` narrows
    /// the ranked set by kind / collection / tags / favorite; see
    /// [`super::store::assets::search`].
    pub fn search_assets(
        &self,
        text: &str,
        q: &crate::model::AssetQuery,
    ) -> super::error::Result<(u64, Vec<crate::model::Asset>)> {
        assets::search(self.store.conn(), text, q)
    }

    // -- smart collections ----------------------------------------------------

    /// Create a smart collection from a validated `NewSmartCollection`.
    pub fn create_smart_collection(
        &self,
        input: &crate::model::NewSmartCollection,
    ) -> Result<crate::model::SmartCollection> {
        input.validate()?;
        smart_collections::create(self.store.conn(), input)
    }

    pub fn list_smart_collections(&self) -> Result<Vec<crate::model::SmartCollection>> {
        smart_collections::list(self.store.conn())
    }

    pub fn get_smart_collection(
        &self,
        id: Uuid,
    ) -> Result<Option<crate::model::SmartCollection>> {
        smart_collections::get(self.store.conn(), id)
    }

    pub fn rename_smart_collection(&self, id: Uuid, name: &str) -> Result<()> {
        smart_collections::rename(self.store.conn(), id, name)
    }

    pub fn delete_smart_collection(&self, id: Uuid) -> Result<()> {
        smart_collections::delete(self.store.conn(), id)
    }

    /// Evaluate a stored smart collection live, materialising the matching
    /// assets as a relevance/paged list. Returns `(total_matching, page)`.
    /// `kind` / `favorite` are extra grid filters AND-ed onto the tree (the
    /// toolbar filters compose with smart collections too).
    pub fn evaluate_smart_collection(
        &self,
        id: Uuid,
        kind: Option<crate::model::AssetKind>,
        favorite: Option<bool>,
        limit: Option<u32>,
        offset: u64,
    ) -> Result<(u64, Vec<crate::model::Asset>)> {
        let conn = self.store.conn();
        let Some(smart_collection) = smart_collections::get(conn, id)? else {
            return Err(crate::Error::NotFound("smart_collection"));
        };
        let node = smart::node_from_json(&smart_collection.query)?;
        let (total, ids) = smart::evaluate_filtered(conn, &node, kind, favorite, limit, offset)?;
        let page = assets::by_ids(conn, &ids)?;
        Ok((total, page))
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
            && assets::count_by_sha256(conn, &sha)? == 0 {
                self.remove_blob_files(&rel, &sha);
            }
        Ok(())
    }

    /// Permanently delete every trashed asset. Returns the number removed.
    pub fn empty_trash(&self) -> Result<u64> {
        let conn = self.store.conn();
        let (_, trashed) = assets::query(
            conn,
            &crate::model::AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )?;
        let mut removed = 0u64;
        for asset in trashed {
            let id = asset.id;
            let sha = asset.sha256.clone();
            let rel = asset.rel_path.clone();
            assets::delete(conn, id)?;
            if let (Some(sha), Some(rel)) = (sha, rel)
                && assets::count_by_sha256(conn, &sha)? == 0 {
                    self.remove_blob_files(&rel, &sha);
                }
            removed += 1;
        }
        Ok(removed)
    }

    // -- batch asset mutations ------------------------------------------------

    /// Trash many assets (single atomic statement).
    pub fn trash_assets(&self, ids: &[Uuid]) -> Result<u64> {
        batch::set_trashed_many(self.store.conn(), ids, true)
    }

    /// Restore many trashed assets (single atomic statement).
    pub fn restore_assets(&self, ids: &[Uuid]) -> Result<u64> {
        batch::set_trashed_many(self.store.conn(), ids, false)
    }

    /// Favorite / unfavorite many assets (single atomic statement).
    pub fn set_assets_favorite(&self, ids: &[Uuid], favorite: bool) -> Result<u64> {
        batch::set_favorite_many(self.store.conn(), ids, favorite)
    }

    /// Attach many assets to a collection (idempotent).
    pub fn add_assets_to_collection(&self, collection_id: Uuid, ids: &[Uuid]) -> Result<u64> {
        batch::add_to_collection_many(self.store.conn(), collection_id, ids)
    }

    /// Permanently delete many assets atomically, freeing any content-addressed
    /// blob (and its thumbnail) once no asset references it left.
    pub fn purge_assets(&self, ids: &[Uuid]) -> Result<PurgeReport> {
        let conn = self.store.conn();
        // Track (rel, sha) for every content hash left unreferenced by this
        // purge, so the file is deleted exactly once even when several deleted
        // assets shared it.
        let mut freed: Vec<(String, String)> = Vec::new();
        let purged = rows::transaction(conn, |tx| {
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
        let thumb = media::thumb::abs_path(&self.root, sha);
        let _ = std::fs::remove_file(thumb);
    }
}

#[cfg(test)]
mod tests {
    use super::Library;
    use chrono::Utc;
    use crate::media::thumb;
    use crate::model::{AssetKind, AssetQuery, NewCollection, NewSmartCollection};
    use crate::store::{assets, collections, tags};
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    /// A minimal valid 1x1 PNG.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
        0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
        0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn temp_library(name: &str) -> (Library, PathBuf) {
        let root = std::env::temp_dir().join(format!("trove-lib-{name}-{}", Uuid::new_v4()));
        let lib = Library::open(&root).unwrap();
        (lib, root)
    }

    fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn auto_import_groups_into_collections() {
        // Source-folder bucketing creates a root collection named after the dir.
        let (lib, root) = temp_library("auto-source");
        let folder = root.join("Vacation");
        std::fs::create_dir_all(&folder).unwrap();
        let src = write_source(&folder, "photo.png", PNG_1X1);

        let report = lib
            .import_files_auto(std::slice::from_ref(&src), None, crate::media::import::AutoCollection::SourceFolder)
            .unwrap();
        assert_eq!(report.imported_count(), 1);
        let _item = &report.imported[0];

        let roots = collections::roots(lib.store().conn()).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "Vacation");
        assert_eq!(collections::count_assets(lib.store().conn(), roots[0].id).unwrap(), 1);

        // Re-importing a *different* file into the same folder reuses the bucket.
        let mut other = PNG_1X1.to_vec();
        other.push(0); // distinct content, so it is not a dedup of photo.png
        let src2 = write_source(&folder, "photo2.png", &other);
        lib.import_files_auto(&[src2], None, crate::media::import::AutoCollection::SourceFolder).unwrap();
        assert_eq!(collections::count_assets(lib.store().conn(), roots[0].id).unwrap(), 2);

        // Month bucketing creates a collection named after the import month.
        let (lib2, root2) = temp_library("auto-month");
        let plain = root2.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let s = write_source(&plain, "a.png", PNG_1X1);
        lib2
            .import_files_auto(&[s], None, crate::media::import::AutoCollection::ImportYearMonth)
            .unwrap();
        let month = Utc::now().format("%Y-%m").to_string();
        let roots = collections::roots(lib2.store().conn()).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, month);
    }

    #[test]
    fn imports_png_and_generates_thumbnail() {
        let (lib, root) = temp_library("png");
        let src = write_source(&root, "photo.png", PNG_1X1);

        let report = lib.import_files(&[src], None).unwrap();
        assert_eq!(report.imported_count(), 1);
        assert_eq!(report.skipped_count(), 0);
        let item = &report.imported[0];
        assert!(!item.reused);
        assert_eq!(item.kind, AssetKind::Image);

        let (total, all) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        let asset = &all[0];
        assert_eq!(asset.mime, "image/png");
        assert_eq!(asset.width, Some(1));
        assert_eq!(asset.height, Some(1));
        assert!(asset.sha256.is_some());
        assert_eq!(asset.file_name, "photo.png");

        // The blob exists on disk under a content-addressed name.
        let rel = asset.rel_path.as_ref().expect("stored asset has rel_path");
        assert!(lib.resolve(rel).is_file());

        // A JPEG thumbnail was generated next to it.
        let thumb_path = thumb::abs_path(lib.root(), asset.sha256.as_deref().unwrap());
        assert!(thumb_path.is_file(), "thumbnail missing at {}", thumb_path.display());
    }

    #[test]
    fn identical_content_is_deduplicated_and_reused() {
        let (lib, root) = temp_library("dedup");
        let src = write_source(&root, "same.png", PNG_1X1);

        let first = lib.import_files(std::slice::from_ref(&src), None).unwrap();
        let second = lib.import_files(&[src], None).unwrap();
        assert!(!first.imported[0].reused);
        assert!(second.imported[0].reused);
        assert_eq!(first.imported[0].asset_id, second.imported[0].asset_id);

        let (total, _) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
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
        let report = lib.import_files(&[src], Some(c.id)).unwrap();
        assert_eq!(report.imported_count(), 1);
        assert_eq!(
            collections::count_assets(lib.store().conn(), c.id).unwrap(),
            1
        );

        // Importing into a missing collection fails up front.
        let err = lib.import_files(&[root.join("nope.png")], Some(Uuid::new_v4())).unwrap_err();
        assert!(err.to_string().contains("collection"));
    }

    #[test]
    fn plain_files_and_skipped_paths() {
        let (lib, root) = temp_library("plain");
        let txt = write_source(&root, "notes.txt", b"hello world");
        let report = lib.import_files(&[txt], None).unwrap();
        let item = &report.imported[0];
        assert_eq!(item.kind, AssetKind::Document);

        // A non-existent path is reported, not fatal.
        let missing = root.join("missing.bin");
        let report = lib.import_files(std::slice::from_ref(&missing), None).unwrap();
        assert_eq!(report.imported_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped[0].path, missing);
    }

    #[test]
    fn purge_removes_blob_only_when_unreferenced() {
        let (lib, root) = temp_library("purge");
        // One imported record…
        let a = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_files(&[a], None).unwrap();
        assert_eq!(report.imported_count(), 1);
        let first_id = report.imported[0].asset_id;
        let stored = assets::get(lib.store().conn(), first_id).unwrap().unwrap();
        let blob = lib.resolve(stored.rel_path.as_ref().unwrap());

        // …moved to trash, then re-imported with a different file name but the
        // same bytes: a second record sharing the same blob.
        assert!(assets::set_trashed(lib.store().conn(), first_id, true).unwrap());
        let b = write_source(&root, "b.png", PNG_1X1);
        let report2 = lib.import_files(&[b], None).unwrap();
        assert_eq!(report2.imported_count(), 1);
        assert!(!report2.imported[0].reused, "trashed content is re-imported fresh");
        let second_id = report2.imported[0].asset_id;

        // Purging the live record keeps the blob (trashed record references it).
        lib.purge_asset(second_id).unwrap();
        assert!(blob.is_file(), "blob must survive while a trashed record exists");

        // Purging the trashed record removes blob and thumbnail.
        lib.purge_asset(first_id).unwrap();
        assert!(!blob.exists(), "blob removed after last reference is purged");
        let sha = stored.sha256.unwrap();
        assert!(!thumb::abs_path(lib.root(), &sha).exists());
    }

    #[test]
    fn empty_trash_removes_all_and_frees_blobs() {
        let (lib, root) = temp_library("empty-trash");
        let one = write_source(&root, "one.png", PNG_1X1);
        let txt = write_source(&root, "notes.txt", b"bye");
        let r = lib.import_files(&[one, txt], None).unwrap();
        assert_eq!(r.imported_count(), 2);
        for item in &r.imported {
            assert!(assets::set_trashed(lib.store().conn(), item.asset_id, true).unwrap());
        }
        let removed = lib.empty_trash().unwrap();
        assert_eq!(removed, 2);
        let (_, trash) = assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(trash.is_empty());
    }

    #[test]
    fn tags_are_case_insensitive_and_attach_to_assets() {
        let (lib, root) = temp_library("tags");
        let src = write_source(&root, "a.png", PNG_1X1);
        let report = lib.import_files(&[src], None).unwrap();
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
        let (total, _) = assets::query(
            lib.store().conn(),
            &AssetQuery {
                tag_ids: vec![blue.id],
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(total, 1);

        // Deleting a tag removes its membership rows.
        tags::delete(lib.store().conn(), red.id).unwrap();
        assert!(tags::for_asset(lib.store().conn(), asset_id).unwrap()[0].name == "blue");
    }

    #[test]
    fn trash_then_reimport_creates_fresh_record() {
        let (lib, root) = temp_library("trash");
        let src = write_source(&root, "x.png", PNG_1X1);
        let first = lib.import_files(std::slice::from_ref(&src), None).unwrap();
        let id = first.imported[0].asset_id;

        assert!(assets::set_trashed(lib.store().conn(), id, true).unwrap());
        let second = lib.import_files(&[src], None).unwrap();
        assert!(!second.imported[0].reused);
        assert_ne!(second.imported[0].asset_id, id);

        let (live, _) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(live, 1);
        let (_, trashed) = assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trashed.len(), 1);
    }

    #[test]
    fn search_and_smart_collection_facade() {
        // A title exposes a searchable token ("sunset") that no other item has.
        let (lib, root) = temp_library("facade");
        let one = write_source(&root, "photo.png", PNG_1X1);
        lib.import_files(&[one], None).unwrap();
        let two = write_source(&root, "notes.txt", b"plain");
        lib.import_files(&[two], None).unwrap();

        // The photo is retitled so it participates in full-text search.
        let (_, all) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let photo_id = all.iter().find(|a| a.file_name == "photo.png").unwrap().id;
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
        let (total, hits) = lib.search_assets("sunset", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo_id);

        // A smart collection over the same terms evaluates through the facade.
        let sc = lib
            .create_smart_collection(&NewSmartCollection {
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
        let (total, assets) =
            lib.evaluate_smart_collection(sc.id, None, None, None, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(assets[0].id, photo_id);

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
    fn empty_fts_index_is_rebuilt_on_open() {
        // Simulates the v2 -> v3 migration outcome: assets exist but the FTS
        // table was dropped and recreated empty. Library::open must backfill
        // the index (with tag names, per the new schema) so search works.
        let (lib, root) = temp_library("fts-backfill");
        let src = write_source(&root, "photo.png", PNG_1X1);
        lib.import_files(&[src], None).unwrap();

        let conn = lib.store().conn();
        let (_, all) = assets::query(conn, &AssetQuery::default()).unwrap();
        let photo_id = all[0].id;
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
            &crate::model::NewTag { name: "landscape".into(), color: None },
        )
        .unwrap();
        tags::add_to_asset(conn, photo_id, tag.id).unwrap();

        // Wipe the index, then reopen the library from disk.
        crate::store::rows::execute(conn, "DELETE FROM asset_fts", vec![]).unwrap();
        drop(lib);
        let reopened = Library::open(&root).unwrap();
        let conn = reopened.store().conn();

        let (total, hits) = reopened.search_assets("sunset", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo_id);

        // The rebuilt index carries tag names too.
        let (total, _) = reopened.search_assets("landscape", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
    }
}

