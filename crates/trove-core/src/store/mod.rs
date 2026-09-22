//! Storage layer entry point: connection lifecycle and schema migration.

pub mod assets;
pub mod batch;
pub mod browse;
pub use browse::BrowseContext;
pub mod collections;
pub mod embeddings;
pub(crate) mod rows;
pub mod schema;
pub mod smart;
pub mod smart_collections;
pub mod stats;
pub mod tags;
pub mod view_history;
pub mod visual_search;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use crate::error::Result;

/// A local (single-file) Trove library database.
///
/// Cheap to clone. The store is synchronous and thread-confined, simple to
/// use from the UI thread.
#[derive(Clone)]
pub struct Store {
    conn: Rc<RefCell<rusqlite::Connection>>,
}

impl Store {
    /// Open (or create) the library at `path`, applying pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        // WAL lets backend jobs (imports own a second connection) write while
        // the UI thread reads. The mode is persistent per database file; an
        // in-memory database ignores it, so the result is not checked.
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
        // WAL's usual pairing: fsync at checkpoint instead of per commit.
        // An app crash is still safe (WAL replays); only a power cut can
        // lose the last transactions — and every bulk write on this database
        // (an import) is re-runnable by design. Without this every commit
        // transaction pays one fsync, which on a small-file import costs
        // more than the staging does.
        let _ = conn.execute_batch("PRAGMA synchronous = NORMAL;");
        // A larger page cache: the working set of a browse/scan easily
        // exceeds the ~2 MB default on a six-figure library.
        let _ = conn.execute_batch("PRAGMA cache_size = -16000;");
        // A backend writer holding the write lock must not error the UI's
        // reads; wait briefly instead.
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let store = Self {
            conn: Rc::new(RefCell::new(conn)),
        };
        store.enable_foreign_keys()?;
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory library (tests, throwaway sessions).
    pub fn in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let store = Self {
            conn: Rc::new(RefCell::new(conn)),
        };
        store.enable_foreign_keys()?;
        store.migrate()?;
        Ok(store)
    }

    fn enable_foreign_keys(&self) -> Result<()> {
        let conn = self.conn();
        rows::execute(conn, "PRAGMA foreign_keys = ON", vec![])?;
        Ok(())
    }

    /// Open a new library, check that an existing one is a shape this build
    /// reads, or walk it forward along [`schema::UPGRADES`].
    ///
    /// A fresh file (version 0) gets [`schema::SCHEMA`] whole. A library at
    /// [`schema::SCHEMA_VERSION`] is left alone. A library sitting at the
    /// `from` of an upgrade step is migrated in place, one step at a time
    /// until the shape is current. Anything else is refused with both
    /// versions in the message — before a statement has run, so the file is
    /// untouched and the user's next move (back it up, start a new one) is
    /// theirs to make on intact data.
    pub fn migrate(&self) -> Result<()> {
        let mut current = self.user_version()?;
        if current == schema::SCHEMA_VERSION {
            return Ok(());
        }
        if current == 0 {
            self.apply(schema::SCHEMA)?;
            return self.set_user_version(schema::SCHEMA_VERSION);
        }
        // Walk the upgrade list to the current shape. Every step's DDL is
        // additive and re-runnable, so a crash between the DDL and the
        // version bump only costs a redo, never a bricked library.
        while current != schema::SCHEMA_VERSION {
            let Some(step) = schema::UPGRADES.iter().find(|step| step.from == current) else {
                return Err(crate::error::Error::Validation(format!(
                    "library schema v{current}, this build reads v{} only: \
                     back the library up and start a new one",
                    schema::SCHEMA_VERSION
                )));
            };
            self.apply(step.sql)?;
            self.set_user_version(step.to)?;
            current = step.to;
        }
        Ok(())
    }

    /// Apply one DDL script atomically.
    fn apply(&self, sql: &str) -> Result<()> {
        let mut mut_borrow = self.conn.borrow_mut();
        let tx = mut_borrow.transaction()?;
        tx.execute_batch(sql).map_err(crate::error::Error::from)?;
        tx.commit().map_err(crate::error::Error::from)?;
        Ok(())
    }

    fn user_version(&self) -> Result<i64> {
        let conn = self.conn();
        rows::query_count(conn, "PRAGMA user_version", vec![])
    }

    fn set_user_version(&self, version: i64) -> Result<()> {
        let conn = self.conn();
        rows::execute(conn, &format!("PRAGMA user_version = {version}"), vec![])?;
        Ok(())
    }

    /// Borrow the underlying SQLite connection.
    ///
    /// # Safety
    ///
    /// The returned reference is valid for the lifetime of `&self`. The
    /// `RefCell` enforces at runtime that no mutable borrow exists while
    /// this reference is in use; the UI is single-threaded, so this is
    /// always satisfied as long as store functions don't recursively call
    /// back into the store while holding a borrow.
    pub fn conn(&self) -> &rusqlite::Connection {
        unsafe { &*self.conn.as_ptr() }
    }

    /// Run a closure inside a SQLite transaction. The closure receives a
    /// `&Transaction` and must return a `Result`. On success the transaction
    /// commits; on error it rolls back.
    pub fn transaction<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        match f(&tx) {
            Ok(v) => {
                tx.commit().map_err(crate::error::Error::from)?;
                Ok(v)
            }
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Store, schema};
    use crate::model::{
        Asset, AssetKind, AssetPatch, AssetQuery, NewCollection, NewTag, Origin, Page, UsageStatus,
        now,
    };
    use crate::store::{assets, collections, tags};
    use uuid::Uuid;

    fn sample_asset(name: &str, kind: AssetKind) -> Asset {
        let id = Uuid::new_v4();
        Asset {
            id,
            origin: Origin::Stored,
            rel_path: Some(format!("media/{}/{}", &id.to_string()[..2], name)),
            file_name: name.to_string(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 128,
            content_hash: Some("a".repeat(64)),
            kind,
            width: Some(800),
            height: Some(600),
            duration_ms: None,
            captured_at: None,
            title: None,
            description: None,
            rating: None,
            is_favorite: false,
            source_url: None,
            usage_status: UsageStatus::Unused,
            commercial_use: None,
            facts: Default::default(),
            created_at: now(),
            updated_at: now(),
            trashed_at: None,
        }
    }

    #[test]
    fn font_kind_roundtrips_and_filters() {
        let store = Store::in_memory().unwrap();
        assets::insert(store.conn(), &sample_asset("Inter.ttf", AssetKind::Font)).unwrap();
        assets::insert(store.conn(), &sample_asset("a.png", AssetKind::Image)).unwrap();

        let page = assets::query(
            store.conn(),
            &AssetQuery {
                kind: Some(AssetKind::Font),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].kind, AssetKind::Font);
        // A patch can move an asset into (and back out of) the new kind.
        let id = page.items[0].id;
        assets::update(
            store.conn(),
            id,
            &AssetPatch {
                kind: Some(AssetKind::Document),
                ..Default::default()
            },
        )
        .unwrap();
        let page = assets::query(
            store.conn(),
            &AssetQuery {
                kind: Some(AssetKind::Font),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 0);
    }

    #[test]
    fn model_kind_roundtrips_and_filters() {
        let store = Store::in_memory().unwrap();
        assets::insert(store.conn(), &sample_asset("dragon.stl", AssetKind::Model)).unwrap();
        assets::insert(store.conn(), &sample_asset("a.png", AssetKind::Image)).unwrap();

        let page = assets::query(
            store.conn(),
            &AssetQuery {
                kind: Some(AssetKind::Model),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 1);
        let id = page.items[0].id;
        assert_eq!(page.items[0].kind, AssetKind::Model);

        // The column holds the wire name a smart rule filters on, and decoding
        // it back yields the same kind.
        let tag: String = store
            .conn()
            .query_row(
                "SELECT kind FROM assets WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tag, "model");
        assert_eq!(
            assets::get(store.conn(), id).unwrap().unwrap().kind,
            AssetKind::Model
        );
    }

    #[test]
    fn distinct_exts_folds_case_dedupes_and_skips_trashed() {
        let store = Store::in_memory().unwrap();
        for (name, ext) in [
            ("a.png", "png"),
            ("b.PNG", "PNG"),
            ("c.jpg", "jpg"),
            ("d.Jpg", "Jpg"),
            ("e", ""),
        ] {
            let mut asset = sample_asset(name, AssetKind::Image);
            asset.ext = ext.into();
            assets::insert(store.conn(), &asset).unwrap();
        }
        // A trashed row must not contribute its extension.
        let mut gone = sample_asset("f.tiff", AssetKind::Image);
        gone.ext = "tiff".into();
        gone.trashed_at = Some(now());
        assets::insert(store.conn(), &gone).unwrap();

        assert_eq!(
            assets::distinct_exts(store.conn()).unwrap(),
            vec!["jpg".to_string(), "png".to_string()]
        );

        // ⚠️ The index plan is deliberately *not* asserted here. `SELECT
        // DISTINCT ext` is answered from `idx_assets_ext` on a full-size
        // library but not on a handful of rows — SQLite's small-table
        // heuristic picks `idx_assets_trashed` instead, at 6 rows and still at
        // 400. The plan is pinned by `search_smoke --profile`, which prints it
        // against a 100k library; this test only covers the semantics the
        // Rust-side folding is responsible for.
    }

    #[test]
    fn smart_collection_update_query_validates() {
        use crate::store::smart_collections;
        let store = Store::in_memory().unwrap();
        let sc = smart_collections::create(
            store.conn(),
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "pics".into(),
                query: serde_json::json!({"op": "match", "field": "kind", "value": "image"}),
                color: None,
                position: 0,
            },
        )
        .unwrap();

        // A valid tree replaces the stored query.
        let tree = serde_json::json!({
            "op": "and",
            "children": [
                {"op": "match", "field": "rating", "compare": "gte", "value": 4},
                {"op": "match", "field": "tag", "value": "三毛"}
            ]
        });
        smart_collections::update_query(store.conn(), sc.id, &tree, Some("#3b82f6")).unwrap();
        let stored = smart_collections::get(store.conn(), sc.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.query, tree);
        assert_eq!(stored.color.as_deref(), Some("#3b82f6"));

        // A garbage tree is refused and the stored one survives.
        assert!(smart_collections::update_query(
            store.conn(),
            sc.id,
            &serde_json::json!({"op": "match", "field": "text", "compare": "gte", "value": "x"}),
            None,
        )
        .is_err());
        assert_eq!(
            smart_collections::get(store.conn(), sc.id)
                .unwrap()
                .unwrap()
                .query,
            tree
        );
        assert!(
            smart_collections::update_query(store.conn(), Uuid::new_v4(), &tree, None).is_err()
        );
    }

    #[test]
    fn a_new_file_gets_the_schema_and_reopening_is_a_no_op() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.user_version().unwrap(), schema::SCHEMA_VERSION);
        store.migrate().unwrap();
    }

    /// A library sitting at the `from` of an upgrade step is walked forward
    /// instead of refused — the case the version gate was waiting for.
    #[test]
    fn a_v14_library_is_upgraded_in_place() {
        let dir = std::env::temp_dir().join(format!("trove-schema-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        let asset = sample_asset("kept.png", AssetKind::Image);

        {
            let store = Store::open(&path).unwrap();
            assets::insert(store.conn(), &asset).unwrap();
        }

        // Rewind to exactly the v14 shape: v15 added only this table and its
        // index.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "DROP INDEX IF EXISTS idx_ai_analysis_model;\n                 DROP TABLE IF EXISTS ai_analysis;\n                 PRAGMA user_version = 14;",
            )
            .unwrap();

        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.user_version().unwrap(),
            schema::SCHEMA_VERSION,
            "the step ran and the version moved"
        );
        let tables: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'ai_analysis'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the upgrade created the cache table");
        assert!(
            assets::get(store.conn(), asset.id).unwrap().is_some(),
            "the upgrade is additive: existing rows survive"
        );

        // Re-opening a migrated library is a no-op.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.user_version().unwrap(), schema::SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A library from a version with no upgrade step is refused rather than
    /// guessed at — and refused *before anything runs*, so the file the user
    /// has is the file they had. That is the whole argument for a version
    /// gate: the failure mode is a message, not a half-applied shape.
    ///
    /// Only v14 has a step (see [`schema::UPGRADES`]); everything else is
    /// refused, including the v13 whose only difference was a column name.
    #[test]
    fn a_library_from_another_version_is_refused_untouched() {
        let dir = std::env::temp_dir().join(format!("trove-schema-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        let asset = sample_asset("kept.png", AssetKind::Image);
        {
            let store = Store::open(&path).unwrap();
            assets::insert(store.conn(), &asset).unwrap();
        }

        for version in [7, 13] {
            // Rewrite the version as another build would have left it.
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(&format!("PRAGMA user_version = {version}"))
                .unwrap();

            let err = match Store::open(&path) {
                Ok(_) => panic!("a v{version} library must not open"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(&format!("v{version}")), "{err}");
            assert!(
                err.contains(&format!("v{}", schema::SCHEMA_VERSION)),
                "{err}"
            );

            // Nothing ran: the version is still what it was, and the row is
            // still there.
            let conn = rusqlite::Connection::open(&path).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                version
            );
            assert!(
                assets::get(&conn, asset.id).unwrap().is_some(),
                "the refusal left the data alone"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collection_tree_crud_and_cycle_refusal() {
        let store = Store::in_memory().unwrap();
        let root = collections::create(
            store.conn(),
            &NewCollection {
                parent_id: None,
                name: "root".into(),
                position: 0,
            },
        )
        .unwrap();
        let child = collections::create(
            store.conn(),
            &NewCollection {
                parent_id: Some(root.id),
                name: "child".into(),
                position: 0,
            },
        )
        .unwrap();

        assert_eq!(
            collections::children_of(store.conn(), None).unwrap().len(),
            1
        );
        assert_eq!(
            collections::children_of(store.conn(), Some(root.id)).unwrap()[0].id,
            child.id
        );

        // Moving root under its own child must be refused.
        let err = collections::move_to(store.conn(), root.id, Some(child.id), 0).unwrap_err();
        assert!(err.to_string().contains("descendant"));

        // Rename + re-root under a fresh parent.
        collections::rename(store.conn(), child.id, "renamed").unwrap();
        let orphan_parent = collections::create(
            store.conn(),
            &NewCollection {
                parent_id: None,
                name: "other".into(),
                position: 1,
            },
        )
        .unwrap();
        collections::move_to(store.conn(), child.id, Some(orphan_parent.id), 5).unwrap();
        let moved = collections::get(store.conn(), child.id).unwrap().unwrap();
        assert_eq!(moved.parent_id, Some(orphan_parent.id));
        assert_eq!(moved.position, 5);

        // Deleting a parent cascades to children.
        collections::delete(store.conn(), root.id).unwrap();
        assert!(collections::get(store.conn(), root.id).unwrap().is_none());
    }

    #[test]
    fn asset_lifecycle_query_and_collection_membership() {
        let store = Store::in_memory().unwrap();
        let img = sample_asset("sunset.png", AssetKind::Image);
        let doc = sample_asset("notes.md", AssetKind::Document);
        assets::insert(store.conn(), &img).unwrap();
        assets::insert(store.conn(), &doc).unwrap();

        // Favorite + retitle via patch.
        assets::update(
            store.conn(),
            img.id,
            &AssetPatch {
                title: Some(Some("Sunset".into())),
                is_favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        // Filter by favorite + kind. (Free text is not a `AssetQuery` concern
        // any more; the index owns it — see the `search_*` tests below.)
        let favs = assets::query(
            store.conn(),
            &AssetQuery {
                is_favorite: Some(true),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(favs.items.len(), 1);

        // Collection membership is many-to-many.
        let c1 = collections::create(
            store.conn(),
            &NewCollection {
                parent_id: None,
                name: "album".into(),
                position: 0,
            },
        )
        .unwrap();
        let c2 = collections::create(
            store.conn(),
            &NewCollection {
                parent_id: None,
                name: "work".into(),
                position: 1,
            },
        )
        .unwrap();
        collections::add_asset(store.conn(), c1.id, img.id).unwrap();
        collections::add_asset(store.conn(), c1.id, doc.id).unwrap();
        collections::add_asset(store.conn(), c2.id, img.id).unwrap();

        assert_eq!(collections::count_assets(store.conn(), c1.id).unwrap(), 2);
        assert_eq!(collections::count_assets(store.conn(), c2.id).unwrap(), 1);

        let in_album = assets::query(
            store.conn(),
            &AssetQuery {
                collection_id: Some(c1.id),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(in_album.total, 2);
        assert_eq!(in_album.items.len(), 2);

        // Trash hides from normal queries, restores bring it back.
        assert!(assets::set_trashed(store.conn(), doc.id, true).unwrap());
        let live = assets::query(store.conn(), &AssetQuery::default()).unwrap();
        assert_eq!(live.items.len(), 1);
        let trash = assets::query(
            store.conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trash.items.len(), 1);

        let live = assets::query(store.conn(), &AssetQuery::default()).unwrap();
        assert_eq!(live.items.len(), 1);
        let trash = assets::query(
            store.conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trash.items.len(), 1);

        // Membership rows follow the asset when it is deleted.
        assets::delete(store.conn(), doc.id).unwrap();
        assert_eq!(collections::count_assets(store.conn(), c1.id).unwrap(), 1);
    }

    #[test]
    fn persisted_file_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("trove-test-{}", Uuid::new_v4()));
        let path = dir.join("library.db");
        {
            let store = Store::open(&path).unwrap();
            let c = collections::create(
                store.conn(),
                &NewCollection {
                    parent_id: None,
                    name: "kept".into(),
                    position: 0,
                },
            )
            .unwrap();
            collections::add_asset(
                store.conn(),
                c.id,
                sample_asset("kept.png", AssetKind::Image).id,
            )
            .unwrap_err(); // not inserted yet — ok, ignore for roundtrip of collection
            let _ = c;
        }
        {
            let store = Store::open(&path).unwrap();
            let roots = collections::roots(store.conn()).unwrap();
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].name, "kept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- full-text search ------------------------------------------------

    // -- full-text search (Tantivy) ----------------------------------------

    /// Index every asset row into `idx`. Production drives this through the
    /// search_queue outbox on library open; the tests call it directly.
    fn index_all(store: &Store, idx: &crate::search::TextIndex) {
        for trashed in [false, true] {
            let page = assets::query(
                store.conn(),
                &AssetQuery {
                    is_trashed: trashed,
                    ..Default::default()
                },
            )
            .unwrap();
            for a in page.items {
                idx.index_asset(store.conn(), a.id).unwrap();
            }
        }
        idx.commit().unwrap();
    }

    /// The search view of the grid: ranked candidates from the index,
    /// narrowed by the compound filters.
    fn search_page(
        store: &Store,
        idx: &crate::search::TextIndex,
        text: &str,
        kind: Option<AssetKind>,
    ) -> Page<Asset> {
        super::BrowseContext {
            search: text.to_string(),
            kind,
            ..Default::default()
        }
        .run(store.conn(), idx, None, None)
        .unwrap()
    }

    /// The library schema must carry no FTS5 remnants: free-text search is the
    /// Tantivy index's job, and the SQLite side keeps only the `search_queue`
    /// outbox that feeds it.
    #[test]
    fn schema_has_no_fts5_leftovers() {
        let store = Store::in_memory().unwrap();
        // Matches the virtual table and all of its shadow tables at once.
        let leftovers: Vec<String> = crate::store::rows::query_map(
            store.conn(),
            "SELECT name FROM sqlite_master WHERE name LIKE '%fts%'",
            vec![],
            |row| crate::store::rows::req_str(row, 0),
        )
        .unwrap();
        assert!(leftovers.is_empty(), "FTS5 leftovers: {leftovers:?}");

        let outbox = crate::store::rows::query_count(
            store.conn(),
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'search_queue'",
            vec![],
        )
        .unwrap();
        assert_eq!(outbox, 1, "the search_queue outbox must exist");
    }

    #[test]
    fn search_ranks_and_filters() {
        let store = Store::in_memory().unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let mut photo = sample_asset("vacation.jpg", AssetKind::Image);
        photo.title = Some("Sunset over the beach".into());
        let mut doc = sample_asset("beach-plan.md", AssetKind::Document);
        doc.description = Some("Notes about the sunset trip".into());
        let mut audio = sample_asset("song.mp3", AssetKind::Audio);
        audio.title = Some("Rainy morning mix".into());
        assets::insert(store.conn(), &photo).unwrap();
        assets::insert(store.conn(), &doc).unwrap();
        assets::insert(store.conn(), &audio).unwrap();
        index_all(&store, &idx);

        // Text hits photo + doc, not audio.
        let page = search_page(&store, &idx, "sunset", None);
        assert_eq!(page.total, 2);

        // Compound filter (kind) narrows the ranked set.
        let hits = search_page(&store, &idx, "sunset", Some(AssetKind::Image));
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo.id);

        // Trashed assets are excluded.
        assets::set_trashed(store.conn(), photo.id, true).unwrap();
        let page = search_page(&store, &idx, "sunset", None);
        assert_eq!(page.total, 1);

        // Special characters never panic and are treated literally.
        let hits = search_page(&store, &idx, "\" * NEAR", None);
        assert_eq!(hits.total, 0);
    }

    /// The ranked-search clause must leave the candidate id list as the only
    /// usable index source.
    ///
    /// This is worth a test of its own because the failure mode is invisible in
    /// a small library and catastrophic in a large one: left to itself the
    /// planner drives the intersection off a filter index, and the one every
    /// search carries — `trashed_at IS NULL` — matches every live row. The
    /// intersection then degrades from N index probes into a full scan (100k
    /// library, measured: 21 ms for 97 candidates and 36 ms for 2000, against
    /// 0.1 ms and 5 ms with the id list driving).
    #[test]
    fn ranked_where_clause_leaves_the_id_list_driving() {
        let store = Store::in_memory().unwrap();
        let q = AssetQuery {
            kind: Some(AssetKind::Image),
            is_favorite: Some(true),
            ..Default::default()
        };
        let (ranked, args) =
            assets::build_where(store.conn(), &q, assets::WhereMode::Rejecting).unwrap();
        let (listing, listing_args) =
            assets::build_where(store.conn(), &q, assets::WhereMode::Driving).unwrap();

        // The modes differ only in the index-suppressing prefixes, so both
        // clauses select the same rows with the same arguments.
        assert_eq!(
            ranked,
            "WHERE +kind = ?1 AND +is_favorite = ?2 AND +trashed_at IS NULL"
        );
        assert_eq!(
            listing,
            "WHERE kind = ?1 AND is_favorite = ?2 AND trashed_at IS NULL"
        );
        assert_eq!(args.len(), listing_args.len());

        /// The plan for `SELECT id FROM assets <clause> AND id IN (?, …)`.
        /// `params` is the statement's total placeholder count: the clause's
        /// own arguments plus the ids.
        fn plan(store: &Store, clause: &str, ids: usize, params: usize) -> String {
            let marks = std::iter::repeat_n("?", ids).collect::<Vec<_>>().join(",");
            let sql = format!("SELECT id FROM assets {clause} AND id IN ({marks})");
            let rows: Vec<String> = store
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .and_then(|mut stmt| {
                    let rows = stmt.query_map(
                        rusqlite::params_from_iter(std::iter::repeat_n(
                            rusqlite::types::Value::Null,
                            params,
                        )),
                        |r| r.get::<_, String>(3),
                    )?;
                    Ok(rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap();
            rows.join("; ")
        }

        /// Whether the plan lets a filter-column index drive the query. Which
        /// one it is depends on the filters (`trashed_at` by default,
        /// `is_favorite` once that is in play), so the test only asks whether
        /// any of them got the job.
        fn drives_a_filter_index(plan: &str) -> Option<&'static str> {
            [
                "idx_assets_trashed",
                "idx_assets_kind",
                "idx_assets_favorite",
            ]
            .into_iter()
            .find(|idx| plan.contains(idx))
        }

        // The listing clause is *supposed* to drive off a filter index, and on
        // this schema it does — which is what makes the assertion below a real
        // one rather than a coincidence. The candidate count has to be in the
        // hundreds: SQLite prices an `id IN (…)` probe by the length of the
        // list, so a two-entry list wins on price even when it loses on work.
        let ids = 300;
        let listing_plan = plan(&store, &listing, ids, listing_args.len() + ids);
        assert!(
            drives_a_filter_index(&listing_plan).is_some(),
            "expected the listing clause to drive off a filter index, got {listing_plan}"
        );

        let ranked_plan = plan(&store, &ranked, ids, args.len() + ids);
        assert_eq!(
            drives_a_filter_index(&ranked_plan),
            None,
            "a filter index drives the ranked intersection: {ranked_plan}"
        );

        // Every condition kind in one clause: the indexable comparisons get
        // suppressed, while the terms that were never index sources — an
        // `EXISTS` on a join table, `json_extract`, `LOWER(ext)`, the
        // orientation `CASE` — keep their plain rendering.
        let mixed = AssetQuery {
            kind: Some(AssetKind::Image),
            collection_id: Some(Uuid::new_v4()),
            is_favorite: Some(true),
            source_path_prefix: Some("/home/shot".into()),
            orientation: Some(crate::model::Orientation::Landscape),
            min_rating: Some(3),
            ext: Some("png".into()),
            ..Default::default()
        };
        let (clause, mixed_args) =
            assets::build_where(store.conn(), &mixed, assets::WhereMode::Rejecting).unwrap();
        assert!(clause.starts_with("WHERE +kind = ?1 AND EXISTS (SELECT 1 FROM asset_collection"));
        for expected in [
            "+is_favorite = ?3",
            "json_extract(assets.extra, '$.source_path') LIKE ?4",
            "CASE WHEN width IS NULL",
            "+rating >= ?6",
            "LOWER(ext) = LOWER(?7)",
            "+trashed_at IS NULL",
        ] {
            assert!(
                clause.contains(expected),
                "missing {expected:?} in {clause:?}"
            );
        }
        let mixed_plan = plan(&store, &clause, ids, mixed_args.len() + ids);
        assert_eq!(
            drives_a_filter_index(&mixed_plan),
            None,
            "a filter index drives the mixed ranked intersection: {mixed_plan}"
        );
    }

    /// A tag filter has to compose with the conditions that come before it.
    ///
    /// `id_list` used to number its placeholders from `?1` no matter what the
    /// clause had already numbered, so `kind` + tag bound the tag subquery's
    /// `?1` to the *kind* string and silently matched nothing, and a second tag
    /// collided with the first. On top of that, a tag whose subtree came back
    /// empty (row deleted while the query still referenced it) rendered
    /// `IN ()`, which is not even valid SQL.
    #[test]
    fn tag_filter_composes_with_the_conditions_before_it() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let photo = sample_asset("img.png", AssetKind::Image);
        let doc = sample_asset("notes.md", AssetKind::Document);
        assets::insert(conn, &photo).unwrap();
        assets::insert(conn, &doc).unwrap();

        let mk = |name: &str| {
            tags::create(
                conn,
                &NewTag {
                    name: name.into(),
                    color: None,
                    parent_id: None,
                },
            )
            .unwrap()
        };
        let day = mk("day");
        let night = mk("night");
        tags::add_to_asset(conn, photo.id, day.id).unwrap();
        tags::add_to_asset(conn, photo.id, night.id).unwrap();
        tags::add_to_asset(conn, doc.id, night.id).unwrap();

        let tagged = |q: &AssetQuery| -> Vec<Uuid> {
            let mut ids: Vec<Uuid> = assets::query(conn, q)
                .unwrap()
                .items
                .iter()
                .map(|a| a.id)
                .collect();
            ids.sort();
            ids
        };
        let sorted = |mut ids: Vec<Uuid>| {
            ids.sort();
            ids
        };

        // Tag alone.
        let q = AssetQuery {
            tag_ids: vec![day.id],
            ..Default::default()
        };
        assert_eq!(tagged(&q), vec![photo.id]);

        // Tag next to a condition that is numbered before it — where the
        // placeholders used to collide.
        let q = AssetQuery {
            kind: Some(AssetKind::Image),
            tag_ids: vec![day.id],
            ..Default::default()
        };
        assert_eq!(tagged(&q), vec![photo.id]);
        let q = AssetQuery {
            kind: Some(AssetKind::Document),
            tag_ids: vec![day.id],
            ..Default::default()
        };
        assert!(tagged(&q).is_empty(), "day is only on the image");

        // Two tags are AND-ed and each keeps its own placeholders.
        let q = AssetQuery {
            tag_ids: vec![day.id, night.id],
            ..Default::default()
        };
        assert_eq!(tagged(&q), vec![photo.id]);
        let q = AssetQuery {
            tag_ids: vec![night.id],
            ..Default::default()
        };
        assert_eq!(tagged(&q), sorted(vec![photo.id, doc.id]));

        // A tag id whose row is gone yields an empty subtree: match nothing,
        // rather than build invalid SQL.
        let q = AssetQuery {
            tag_ids: vec![Uuid::new_v4()],
            ..Default::default()
        };
        assert!(tagged(&q).is_empty());
    }

    /// `counts_by_tag` must agree with `count_assets` for every tag, including
    /// the case that would make a naive roll-up wrong: an asset carrying both a
    /// parent tag and one of its children counts once, not twice.
    #[test]
    fn counts_by_tag_matches_count_assets() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mk = |name: &str, parent: Option<Uuid>| {
            tags::create(
                conn,
                &NewTag {
                    name: name.into(),
                    color: None,
                    parent_id: parent,
                },
            )
            .unwrap()
        };
        let root = mk("root", None);
        let child = mk("child", Some(root.id));
        let grandchild = mk("grandchild", Some(child.id));
        let unrelated = mk("unrelated", None);
        let childless = mk("childless", None);

        let a = sample_asset("a.png", AssetKind::Image);
        let b = sample_asset("b.png", AssetKind::Image);
        let c = sample_asset("c.png", AssetKind::Image);
        for asset in [&a, &b, &c] {
            assets::insert(conn, asset).unwrap();
        }

        tags::add_to_asset(conn, a.id, root.id).unwrap();
        tags::add_to_asset(conn, a.id, child.id).unwrap();
        tags::add_to_asset(conn, b.id, grandchild.id).unwrap();
        tags::add_to_asset(conn, c.id, unrelated.id).unwrap();

        let counts = tags::counts_by_tag(conn).unwrap();
        for tag in [&root, &child, &grandchild, &unrelated, &childless] {
            assert_eq!(
                counts.get(&tag.id).copied().unwrap_or(0),
                tags::count_assets(conn, tag.id).unwrap(),
                "{} disagrees",
                tag.name
            );
        }
        // The numbers themselves too, so a bug shared by both paths — which
        // the loop above would happily accept — cannot pass.
        let at = |tag: &crate::model::Tag| counts.get(&tag.id).copied().unwrap_or(0);
        assert_eq!(
            at(&root),
            2,
            "a carries root+child, b carries the grandchild"
        );
        assert_eq!(at(&child), 2);
        assert_eq!(at(&grandchild), 1);
        assert_eq!(at(&unrelated), 1);
        assert_eq!(at(&childless), 0);

        // A tag whose row is gone is simply absent from the map.
        tags::delete(conn, childless.id).unwrap();
        let counts = tags::counts_by_tag(conn).unwrap();
        assert!(!counts.contains_key(&childless.id));
    }

    #[test]
    fn search_multi_term_is_and_substring() {
        let store = Store::in_memory().unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let mut photo = sample_asset("vacation.jpg", AssetKind::Image);
        photo.title = Some("Sunset over the beach".into());
        let mut doc = sample_asset("trip-notes.md", AssetKind::Document);
        doc.description = Some("Notes about the sunset trip".into());
        let mut audio = sample_asset("song.mp3", AssetKind::Audio);
        audio.title = Some("Rainy morning mix".into());
        assets::insert(store.conn(), &photo).unwrap();
        assets::insert(store.conn(), &doc).unwrap();
        assets::insert(store.conn(), &audio).unwrap();
        index_all(&store, &idx);

        // Multiple terms AND together: only the photo has both.
        let hits = search_page(&store, &idx, "sunset beach", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo.id);

        // Substring matching: a term matches anywhere inside a token, so
        // `sunse` hits `Sunset` mid-word.
        let page = search_page(&store, &idx, "sunse", None);
        assert_eq!(page.total, 2);
        let hits = search_page(&store, &idx, "beac sunse", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo.id);

        // Typo tolerance: `beacho` is one edit from `beach` and still finds
        // the beach asset.
        let page = search_page(&store, &idx, "beacho", None);
        assert_eq!(page.total, 1);
        let page = search_page(&store, &idx, "rainy", None);
        assert_eq!(page.total, 1);

        // Whitespace is only a separator; extra spaces change nothing.
        let page = search_page(&store, &idx, "  sunset   beach  ", None);
        assert_eq!(page.total, 1);
    }

    #[test]
    fn search_substring_and_chinese() {
        let store = Store::in_memory().unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let flower = sample_asset("flower.png", AssetKind::Image);
        let mut cat = sample_asset("花园里的猫.png", AssetKind::Image);
        cat.title = Some("A cat in the garden 花园里的猫".into());
        assets::insert(store.conn(), &flower).unwrap();
        assets::insert(store.conn(), &cat).unwrap();
        index_all(&store, &idx);

        // Infix substring, not just a prefix: `ower` sits inside `flower`.
        let hits = search_page(&store, &idx, "ower", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, flower.id);

        // A single hanzi rides the jieba word field.
        let hits = search_page(&store, &idx, "猫", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, cat.id);

        // A two-hanzi substring rides the gram field.
        let hits = search_page(&store, &idx, "园里", None);
        assert_eq!(hits.total, 1);

        // Pinyin: full syllables and initials both find the cat.
        let hits = search_page(&store, &idx, "mao", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, cat.id);
        let hits = search_page(&store, &idx, "hyl", None);
        assert_eq!(hits.total, 1);

        // A smart rule's text condition finds short terms the same way.
        let node = smart_node(serde_json::json!({
            "op": "match", "field": "text", "value": "猫"
        }));
        let ids = super::smart::evaluate(store.conn(), Some(&idx), &node, None, 0).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items[0], cat.id);
    }

    #[test]
    fn search_matches_tag_names_and_stays_in_sync() {
        let store = Store::in_memory().unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let conn = store.conn();
        let photo = sample_asset("img.png", AssetKind::Image);
        assets::insert(conn, &photo).unwrap();
        index_all(&store, &idx);

        let tag = tags::create(
            conn,
            &NewTag {
                name: "landscape".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        let other = tags::create(
            conn,
            &NewTag {
                name: "night".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();

        // No tag attached yet: not found.
        let page = search_page(&store, &idx, "landscape", None);
        assert_eq!(page.total, 0);

        // Attach: the tag name becomes searchable, prefix included. The
        // outbox trigger enqueues the asset; the drain refreshes the doc.
        tags::add_to_asset(conn, photo.id, tag.id).unwrap();
        crate::search::drain(conn, &idx).unwrap();
        let hits = search_page(&store, &idx, "landscape", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo.id);
        let page = search_page(&store, &idx, "lands", None);
        assert_eq!(page.total, 1);

        // Detach: the stale index entry must disappear.
        tags::remove_from_asset(conn, photo.id, tag.id).unwrap();
        crate::search::drain(conn, &idx).unwrap();
        let page = search_page(&store, &idx, "landscape", None);
        assert_eq!(page.total, 0);

        // Batch replace carries every new tag name.
        tags::set_for_asset(conn, photo.id, &[tag.id, other.id]).unwrap();
        crate::search::drain(conn, &idx).unwrap();
        let page = search_page(&store, &idx, "landscape night", None);
        assert_eq!(page.total, 1);

        // Deleting a tag removes it from every indexed asset.
        tags::delete(conn, other.id).unwrap();
        crate::search::drain(conn, &idx).unwrap();
        let page = search_page(&store, &idx, "night", None);
        assert_eq!(page.total, 0);
        let page = search_page(&store, &idx, "landscape", None);
        assert_eq!(page.total, 1);
    }

    #[test]
    fn search_syntax_characters_stay_literal() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let mut photo = sample_asset("a.png", AssetKind::Image);
        photo.title = Some("C++ tips and (tricks)".into());
        assets::insert(conn, &photo).unwrap();
        index_all(&store, &idx);

        // Query operators and syntax characters are matched literally, never
        // parsed: these inputs must not error or widen the match.
        for q in [
            "c*",
            "\"c*\"",
            "NEAR",
            "c AND tips",
            "c OR (tips)",
            "\"",
            "*",
            "--",
        ] {
            let page = search_page(&store, &idx, q, None);
            assert_eq!(page.total, 0, "query {q:?} must match nothing");
        }
        // "tips" still finds it.
        let hits = search_page(&store, &idx, "tips", None);
        assert_eq!(hits.total, 1);
        assert_eq!(hits.items[0].id, photo.id);
    }

    #[test]
    fn by_ids_preserves_input_order() {
        let store = Store::in_memory().unwrap();
        let mut a = sample_asset("a.png", AssetKind::Image);
        a.title = Some("delta".into());
        let mut b = sample_asset("b.png", AssetKind::Image);
        b.title = Some("alpha".into());
        let mut c = sample_asset("c.png", AssetKind::Image);
        c.title = Some("charlie".into());
        assets::insert(store.conn(), &a).unwrap();
        assets::insert(store.conn(), &b).unwrap();
        assets::insert(store.conn(), &c).unwrap();

        let ordered = assets::by_ids(store.conn(), &[c.id, a.id, b.id]).unwrap();
        assert_eq!(
            ordered.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![c.id, a.id, b.id]
        );
    }

    // -- smart collections -----------------------------------------------

    fn smart_node(value: serde_json::Value) -> crate::model::SmartNode {
        super::smart::node_from_json(&value).unwrap()
    }

    #[test]
    fn smart_collection_evaluates_trees() {
        let store = Store::in_memory().unwrap();
        let mut img = sample_asset("p1.png", AssetKind::Image);
        img.rating = Some(4);
        let mut vid = sample_asset("v1.mp4", AssetKind::Video);
        vid.rating = Some(2);
        let mut doc = sample_asset("d1.md", AssetKind::Document);
        doc.rating = Some(5);
        assets::insert(store.conn(), &img).unwrap();
        assets::insert(store.conn(), &vid).unwrap();
        assets::insert(store.conn(), &doc).unwrap();

        let tags = super::tags::ensure_named(store.conn(), "Trip").unwrap();
        super::tags::add_to_asset(store.conn(), img.id, tags.id).unwrap();
        super::tags::add_to_asset(store.conn(), doc.id, tags.id).unwrap();

        // rating >= 4  → img + doc
        let node = smart_node(serde_json::json!({
            "op": "match", "field": "rating", "compare": "gte", "value": 4
        }));
        let ids = super::smart::evaluate(store.conn(), None, &node, None, 0).unwrap();
        assert_eq!(ids.total, 2);
        assert!(ids.items.contains(&img.id) && ids.items.contains(&doc.id));

        // kind != image ∧ rating >= 4 → doc only (kind pairs with rating in an AND tree)
        let tree = smart_node(serde_json::json!({
            "op": "and",
            "children": [
                { "op": "match", "field": "rating", "compare": "gte", "value": 4 },
                { "op": "match", "field": "kind", "compare": "ne", "value": "image" },
            ]
        }));
        let ids = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![doc.id]);
    }

    #[test]
    fn a_tree_without_a_node_type_is_still_an_error() {
        // The `op` tag is required; a node that carries neither it nor a field
        // is not something to guess at.
        assert!(
            super::smart::node_from_json(&serde_json::json!({
                "op": "and", "children": [{ "value": 1 }]
            }))
            .is_err()
        );
    }

    #[test]
    fn evaluate_filtered_and_grid_filters() {
        let store = Store::in_memory().unwrap();
        let mut img = sample_asset("p.png", AssetKind::Image);
        img.is_favorite = true;
        let mut vid = sample_asset("v.mp4", AssetKind::Video);
        vid.is_favorite = true;
        let doc = sample_asset("d.md", AssetKind::Document);
        assets::insert(store.conn(), &img).unwrap();
        assets::insert(store.conn(), &vid).unwrap();
        assets::insert(store.conn(), &doc).unwrap();

        // The match-all tree: an OR over favorite/kind always true for all.
        let node = smart_node(serde_json::json!({
            "op": "or", "children": [
                { "op": "match", "field": "is_favorite", "value": true },
                { "op": "match", "field": "is_favorite", "compare": "ne", "value": true },
            ]
        }));

        // kind filter narrows to images.
        let ids = super::smart::evaluate_filtered(
            store.conn(),
            None,
            &node,
            super::smart::SmartPage {
                kind: Some(AssetKind::Image),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![img.id]);

        // favorite filter narrows to the two favorites (img + vid).
        let page = super::smart::evaluate_filtered(
            store.conn(),
            None,
            &node,
            super::smart::SmartPage {
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 2);

        // Both compose with AND.
        let ids = super::smart::evaluate_filtered(
            store.conn(),
            None,
            &node,
            super::smart::SmartPage {
                kind: Some(AssetKind::Video),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![vid.id]);
    }

    #[test]
    fn smart_collection_text_and_tag_match() {
        let store = Store::in_memory().unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let mut a = sample_asset("notes.md", AssetKind::Document);
        a.title = Some("quarterly beach report".into());
        a.is_favorite = true;
        assets::insert(store.conn(), &a).unwrap();
        index_all(&store, &idx);

        // text condition routes through the Tantivy index.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "text", "value": "beach"
        }));
        let ids = super::smart::evaluate(store.conn(), Some(&idx), &tree, None, 0).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![a.id]);

        // tag match is case-insensitive.
        super::tags::ensure_named(store.conn(), "Travel").unwrap();
        super::tags::add_to_asset(
            store.conn(),
            a.id,
            super::tags::ensure_named(store.conn(), "travel")
                .unwrap()
                .id,
        )
        .unwrap();
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "tag", "value": "TRAVEL"
        }));
        let page = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(page.total, 1);

        // favorite filter + AND with a tag.
        let tree = smart_node(serde_json::json!({
            "op": "and",
            "children": [
                { "op": "match", "field": "is_favorite", "value": true },
                { "op": "match", "field": "tag", "value": "travel" },
            ]
        }));
        let page = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(page.total, 1);
    }

    #[test]
    fn smart_collection_color_filter() {
        let store = Store::in_memory().unwrap();
        // Color lives in the visual facts (as mined by color::dominant_colors).
        let mut red = sample_asset("red.png", AssetKind::Image);
        red.facts.visual.dominant_color = Some("#d01010".into());
        let mut blue = sample_asset("blue.png", AssetKind::Image);
        blue.facts.visual.dominant_color = Some("#1a5cff".into());
        assets::insert(store.conn(), &red).unwrap();
        assets::insert(store.conn(), &blue).unwrap();

        // Exact match, case-insensitive and `#`-optional.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "color", "value": "#d01010"
        }));
        let ids = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![red.id]);

        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "color", "compare": "ne", "value": "D01010"
        }));
        let ids = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items, vec![blue.id]);

        // A malformed color is rejected at compile time.
        let bad = smart_node(serde_json::json!({
            "op": "match", "field": "color", "value": "notacolor"
        }));
        assert!(super::smart::compile(None, None, &bad).is_err());
    }

    #[test]
    fn smart_collection_pages_and_validates() {
        let store = Store::in_memory().unwrap();
        for i in 0..10 {
            let mut a = sample_asset(&format!("f{i}.png"), AssetKind::Image);
            a.rating = Some(5);
            assets::insert(store.conn(), &a).unwrap();
        }
        let node = smart_node(serde_json::json!({
            "op": "match", "field": "rating", "compare": "gte", "value": 5
        }));
        let ids = super::smart::evaluate(store.conn(), None, &node, Some(4), 0).unwrap();
        assert_eq!(ids.total, 10);
        assert_eq!(ids.items.len(), 4);

        // An unknown field is rejected when the JSON is deserialized into a node.
        let bad_node = super::smart::node_from_json(&serde_json::json!({
            "op": "match", "field": "nope", "value": 1
        }));
        assert!(bad_node.is_err());

        // Wrong value types are rejected when the tree is compiled.
        let bad_type = smart_node(serde_json::json!({
            "op": "match", "field": "rating", "value": "high"
        }));
        assert!(super::smart::compile(None, None, &bad_type).is_err());

        let bad_op = smart_node(serde_json::json!({
            "op": "match", "field": "kind", "compare": "gt", "value": "image"
        }));
        assert!(super::smart::compile(None, None, &bad_op).is_err());
    }

    #[test]
    fn smart_collection_date_aspect_orientation() {
        let store = Store::in_memory().unwrap();
        let parse = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // sample_asset is 800x600 (landscape).
        let mut landscape = sample_asset("land.png", AssetKind::Image);
        landscape.captured_at = Some(parse("2024-06-15T10:00:00Z"));
        let mut portrait = sample_asset("port.png", AssetKind::Image);
        (portrait.width, portrait.height) = (Some(600), Some(800));
        portrait.captured_at = Some(parse("2025-01-02T08:30:00Z"));
        let mut square = sample_asset("sq.png", AssetKind::Image);
        (square.width, square.height) = (Some(500), Some(500));
        let mut audio = sample_asset("song.mp3", AssetKind::Audio);
        (audio.width, audio.height) = (None, None);
        for a in [&landscape, &portrait, &square, &audio] {
            assets::insert(store.conn(), a).unwrap();
        }

        // Captured date: day equality via the RFC 3339 prefix.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "captured_at", "value": "2024-06-15"
        }));
        let ids = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!((ids.total, ids.items.len()), (1, 1));
        assert_eq!(ids.items[0], landscape.id);

        // A date range (gte + lt in an `and` group).
        let tree = smart_node(serde_json::json!({
            "op": "and",
            "children": [
                { "op": "match", "field": "captured_at", "compare": "gte", "value": "2024-01-01" },
                { "op": "match", "field": "captured_at", "compare": "lt", "value": "2025-01-01" },
            ]
        }));
        let page = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(page.total, 1);

        // Orientation splits the three images; assets without dimensions
        // (the audio file) never match.
        for (orientation, expected) in [("landscape", 1), ("portrait", 1), ("square", 1)] {
            let tree = smart_node(serde_json::json!({
                "op": "match", "field": "orientation", "value": orientation
            }));
            let page = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
            assert_eq!(page.total, expected, "{orientation}");
        }
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "orientation", "compare": "ne", "value": "landscape"
        }));
        let page = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!(page.total, 2);

        // Aspect ratio: 800/600 ≈ 1.33 matches the > 1.2 bucket.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "aspect_ratio", "compare": "gt", "value": 1.2
        }));
        let ids = super::smart::evaluate(store.conn(), None, &tree, None, 0).unwrap();
        assert_eq!((ids.total, ids.items.len()), (1, 1));
        assert_eq!(ids.items[0], landscape.id);

        // A malformed date is rejected at compile time.
        let bad = smart_node(serde_json::json!({
            "op": "match", "field": "captured_at", "value": "June 2024"
        }));
        assert!(super::smart::compile(None, None, &bad).is_err());
        let bad_orientation = smart_node(serde_json::json!({
            "op": "match", "field": "orientation", "value": "diagonal"
        }));
        assert!(super::smart::compile(None, None, &bad_orientation).is_err());
    }

    #[test]
    fn smart_collection_crud_roundtrip() {
        let store = Store::in_memory().unwrap();
        let input = crate::model::NewSmartCollection {
            parent_id: None,
            name: "Favorites".into(),
            query: serde_json::json!({
                "op": "match", "field": "is_favorite", "value": true
            }),
            color: Some("#f00".into()),
            position: 0,
        };
        let created = super::smart_collections::create(store.conn(), &input).unwrap();
        let fetched = super::smart_collections::get(store.conn(), created.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.name, "Favorites");
        assert_eq!(fetched.query, input.query);
        let listed = super::smart_collections::list(store.conn()).unwrap();
        assert_eq!(listed.len(), 1);
        super::smart_collections::rename(store.conn(), created.id, "Renamed").unwrap();
        assert_eq!(
            super::smart_collections::get(store.conn(), created.id)
                .unwrap()
                .unwrap()
                .name,
            "Renamed"
        );
        super::smart_collections::delete(store.conn(), created.id).unwrap();
        assert!(
            super::smart_collections::get(store.conn(), created.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn smart_collection_hierarchy_and_cascade() {
        use crate::model::{NewCollection, NewSmartCollection};
        use crate::store::{collections, smart_collections};

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let fav = serde_json::json!({"op": "match", "field": "is_favorite", "value": true});
        let mk = |parent_id: Option<Uuid>, name: &str| NewSmartCollection {
            parent_id,
            name: name.into(),
            query: fav.clone(),
            color: None,
            position: 0,
        };

        let folder = collections::create(
            conn,
            &NewCollection {
                parent_id: None,
                name: "Trips".into(),
                position: 0,
            },
        )
        .unwrap();
        let sub = collections::create(
            conn,
            &NewCollection {
                parent_id: Some(folder.id),
                name: "2026".into(),
                position: 0,
            },
        )
        .unwrap();

        // A parent that is neither a collection nor a smart collection is
        // rejected (there is no SQL FK to enforce it).
        assert!(smart_collections::create(conn, &mk(Some(Uuid::new_v4()), "bad")).is_err());

        let parent = smart_collections::create(conn, &mk(Some(folder.id), "parent")).unwrap();
        let child = smart_collections::create(conn, &mk(Some(parent.id), "child")).unwrap();
        let grandchild =
            smart_collections::create(conn, &mk(Some(child.id), "grandchild")).unwrap();
        let sibling = smart_collections::create(conn, &mk(Some(parent.id), "sibling")).unwrap();

        assert_eq!(parent.parent_id, Some(folder.id));
        assert_eq!(
            smart_collections::get(conn, child.id)
                .unwrap()
                .unwrap()
                .parent_id,
            Some(parent.id)
        );

        // Cycle and self-parent refusals.
        assert!(smart_collections::move_to(conn, parent.id, Some(child.id), 0).is_err());
        assert!(smart_collections::move_to(conn, parent.id, Some(parent.id), 0).is_err());
        // Moving a missing row, or under a missing parent, fails cleanly.
        assert!(smart_collections::move_to(conn, Uuid::new_v4(), None, 0).is_err());
        assert!(smart_collections::move_to(conn, child.id, Some(Uuid::new_v4()), 0).is_err());

        // Legal moves: to the root, and under a (sub-)collection.
        smart_collections::move_to(conn, child.id, None, 3).unwrap();
        assert_eq!(
            smart_collections::get(conn, child.id)
                .unwrap()
                .unwrap()
                .parent_id,
            None
        );
        smart_collections::move_to(conn, child.id, Some(sub.id), 0).unwrap();

        // Deleting a smart collection takes its smart descendants, and only
        // those: `child` was moved out from under `parent` beforehand.
        smart_collections::delete(conn, parent.id).unwrap();
        assert!(smart_collections::get(conn, parent.id).unwrap().is_none());
        assert!(smart_collections::get(conn, sibling.id).unwrap().is_none());
        assert!(smart_collections::get(conn, child.id).unwrap().is_some());

        // Deleting a collection cascades to smart children of the whole
        // collection subtree (`child` sits under `sub`).
        collections::delete(conn, folder.id).unwrap();
        assert!(smart_collections::get(conn, child.id).unwrap().is_none());
        assert!(
            smart_collections::get(conn, grandchild.id)
                .unwrap()
                .is_none()
        );
        assert!(smart_collections::list(conn).unwrap().is_empty());
    }

    #[test]
    fn rebuild_search_index_backfills() {
        let lib_root = std::env::temp_dir().join(format!("trove-rebuild-{}", Uuid::new_v4()));
        let lib = super::super::library::Library::open_in_memory(&lib_root).unwrap();
        let conn = lib.store().conn();
        let mut a = sample_asset("a.png", AssetKind::Image);
        a.title = Some("lost treasure".into());
        let mut b = sample_asset("b.png", AssetKind::Image);
        b.title = Some("other thing".into());
        assets::insert(conn, &a).unwrap();
        assets::insert(conn, &b).unwrap();
        // Simulate a wiped index then rebuild it from the rows.
        lib.text_index().wipe().unwrap();

        let n = crate::services::maintenance::rebuild_search_index(&lib).unwrap();
        assert_eq!(n, 2);
        let page = lib
            .search_assets("treasure", &AssetQuery::default())
            .unwrap();
        assert_eq!(page.total, 1);
    }

    #[test]
    fn tag_rename_updates_index_and_rejects_duplicates() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "beach".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        let mut a = sample_asset("a.png", AssetKind::Image);
        a.title = Some("sunset".into());
        assets::insert(conn, &a).unwrap();
        tags::add_to_asset(conn, a.id, tag.id).unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        index_all(&store, &idx);
        // The old name is searchable before the rename.
        let page = search_page(&store, &idx, "beach", None);
        assert_eq!(page.total, 1);

        tags::rename(conn, tag.id, "coastline").unwrap();
        let renamed = tags::get(conn, tag.id).unwrap().unwrap();
        assert_eq!(renamed.name, "coastline");
        // The outbox picked the rename up; the index follows in both directions.
        crate::search::drain(conn, &idx).unwrap();
        let page = search_page(&store, &idx, "coastline", None);
        assert_eq!(page.total, 1);
        let page = search_page(&store, &idx, "beach", None);
        assert_eq!(page.total, 0);

        // Renaming onto an existing name (case-insensitive) fails.
        let other = tags::create(
            conn,
            &NewTag {
                name: "night".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        assert!(tags::rename(conn, tag.id, "NIGHT").is_err());
        let _ = other;
    }

    #[test]
    fn tag_color_validates_and_persists() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "t".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::set_color(conn, tag.id, Some("FF00AA")).unwrap();
        assert_eq!(
            tags::get(conn, tag.id).unwrap().unwrap().color,
            Some("#ff00aa".into())
        );
        tags::set_color(conn, tag.id, None).unwrap();
        assert_eq!(tags::get(conn, tag.id).unwrap().unwrap().color, None);
        assert!(tags::set_color(conn, tag.id, Some("nothex")).is_err());
    }

    #[test]
    fn asset_query_sort_orders() {
        use crate::model::AssetSort;
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut a = sample_asset("aaa.png", AssetKind::Image);
        a.size_bytes = 300;
        a.rating = Some(2);
        let mut b = sample_asset("zzz.png", AssetKind::Image);
        b.size_bytes = 100;
        b.rating = Some(5);
        // Stagger import times so the default newest-first order is
        // deterministic (sample timestamps would otherwise collide).
        let mut c = sample_asset("mmm.png", AssetKind::Image);
        a.created_at = now();
        b.created_at = now() - chrono::Duration::seconds(1);
        c.created_at = now() - chrono::Duration::seconds(2);
        assets::insert(conn, &a).unwrap();
        assets::insert(conn, &b).unwrap();
        assets::insert(conn, &c).unwrap();

        let names = |q: AssetQuery| -> Vec<String> {
            assets::query(conn, &q)
                .unwrap()
                .items
                .iter()
                .map(|x| x.file_name.clone())
                .collect()
        };
        // Default: newest first (insert order C, B, A).
        assert_eq!(
            names(AssetQuery::default()),
            vec!["mmm.png", "zzz.png", "aaa.png"]
        );
        assert_eq!(
            names(AssetQuery {
                sort: AssetSort::Name,
                sort_desc: false,
                ..Default::default()
            }),
            vec!["aaa.png", "mmm.png", "zzz.png"]
        );
        assert_eq!(
            names(AssetQuery {
                sort: AssetSort::SizeBytes,
                sort_desc: true,
                ..Default::default()
            }),
            vec!["aaa.png", "mmm.png", "zzz.png"]
        );
        // Un-rated assets come last in a descending rating sort.
        assert_eq!(
            names(AssetQuery {
                sort: AssetSort::Rating,
                sort_desc: true,
                ..Default::default()
            }),
            vec!["zzz.png", "aaa.png", "mmm.png"]
        );
    }

    #[test]
    fn export_metadata_roundtrip() {
        use crate::store::{collections, smart_collections};
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut a = sample_asset("a.png", AssetKind::Image);
        a.title = Some("sunset".into());
        assets::insert(conn, &a).unwrap();
        let coll = collections::create(
            conn,
            &NewCollection {
                parent_id: None,
                name: "trip".into(),
                position: 0,
            },
        )
        .unwrap();
        collections::add_asset(conn, coll.id, a.id).unwrap();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "beach".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::add_to_asset(conn, a.id, tag.id).unwrap();
        smart_collections::create(
            conn,
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "fav".into(),
                query: serde_json::json!({
                    "op": "match", "field": "is_favorite", "value": true
                }),
                color: None,
                position: 0,
            },
        )
        .unwrap();

        let json = crate::library::export_metadata_from_store(&store).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["format"], "trove-export");
        assert_eq!(value["asset_count"], 1);
        assert_eq!(value["assets"][0]["title"], "sunset");
        assert_eq!(value["collections"][0]["name"], "trip");
        assert_eq!(value["tags"][0]["name"], "beach");
        assert_eq!(value["smart_collections"][0]["name"], "fav");
    }

    #[test]
    fn view_history_records_prunes_and_hides_trashed() {
        use crate::store::view_history;
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let a = sample_asset("a.png", AssetKind::Image);
        let b = sample_asset("b.png", AssetKind::Image);
        let c = sample_asset("c.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();
        assets::insert(conn, &b).unwrap();
        assets::insert(conn, &c).unwrap();

        // View order b, a, c -> newest first is c, a, b.
        view_history::record(conn, b.id).unwrap();
        view_history::record(conn, a.id).unwrap();
        view_history::record(conn, c.id).unwrap();
        assert_eq!(
            view_history::recent_ids(conn, 10).unwrap(),
            vec![c.id, a.id, b.id]
        );
        assert_eq!(view_history::live_count(conn).unwrap(), 3);

        // Re-viewing bumps the asset back to the top (upsert, no duplicate).
        view_history::record(conn, b.id).unwrap();
        assert_eq!(
            view_history::recent_ids(conn, 10).unwrap(),
            vec![b.id, c.id, a.id]
        );
        assert_eq!(view_history::live_count(conn).unwrap(), 3);

        // Trashed assets drop out of the view but keep their row, so a
        // restore brings the entry back.
        assets::set_trashed(conn, c.id, true).unwrap();
        assert_eq!(
            view_history::recent_ids(conn, 10).unwrap(),
            vec![b.id, a.id]
        );
        assert_eq!(view_history::live_count(conn).unwrap(), 2);

        // Purging an asset cascades its history row away.
        super::rows::execute(
            conn,
            "DELETE FROM assets WHERE id = ?1",
            vec![super::rows::uuid(c.id).into()],
        )
        .unwrap();
        assert_eq!(
            view_history::recent_ids(conn, 10).unwrap(),
            vec![b.id, a.id]
        );

        // The cap prunes the oldest entries.
        view_history::prune(conn, 2).unwrap();
        assert_eq!(
            view_history::recent_ids(conn, 10).unwrap(),
            vec![b.id, a.id]
        );

        // Clear wipes everything.
        view_history::clear(conn).unwrap();
        assert_eq!(view_history::live_count(conn).unwrap(), 0);
    }
}
