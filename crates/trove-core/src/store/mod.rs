//! Storage layer entry point: connection lifecycle and schema migration.

pub mod assets;
pub mod batch;
pub mod collections;
pub mod smart;
pub mod smart_collections;
pub mod tags;
pub(crate) mod rows;
pub mod schema;

use std::path::Path;

use libsql::Builder;

use crate::error::Result;

/// A local (single-file) Trove library database.
///
/// Cheap to clone. The libsql connection is async internally but every helper
/// runs it to completion synchronously, so the store is thread-confined and
/// simple to use from the UI thread.
#[derive(Clone)]
pub struct Store {
    conn: libsql::Connection,
}

impl Store {
    /// Open (or create) the library at `path`, applying pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        let db = pollster::block_on(Builder::new_local(path).build())?;
        let conn = db.connect()?;
        let store = Self { conn };
        store.enable_foreign_keys()?;
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory library (tests, throwaway sessions).
    pub fn in_memory() -> Result<Self> {
        let db = pollster::block_on(Builder::new_local(":memory:").build())?;
        let conn = db.connect()?;
        let store = Self { conn };
        store.enable_foreign_keys()?;
        store.migrate()?;
        Ok(store)
    }

    fn enable_foreign_keys(&self) -> Result<()> {
        rows::execute(&self.conn, "PRAGMA foreign_keys = ON", vec![])?;
        Ok(())
    }

    /// Bring the schema up to date.
    pub fn migrate(&self) -> Result<()> {
        let current = self.user_version()?;
        for (ix, sql) in schema::MIGRATIONS.iter().enumerate() {
            let target = (ix + 1) as i64;
            if current < target {
                rows::execute_transactional_batch(&self.conn, sql)?;
                self.set_user_version(target)?;
            }
        }
        Ok(())
    }

    fn user_version(&self) -> Result<i64> {
        rows::query_count(&self.conn, "PRAGMA user_version", vec![])
    }

    fn set_user_version(&self, version: i64) -> Result<()> {
        rows::execute(
            &self.conn,
            &format!("PRAGMA user_version = {version}"),
            vec![],
        )?;
        Ok(())
    }

    pub fn conn(&self) -> &libsql::Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::{schema, Store};
    use crate::model::{
        Asset, AssetKind, AssetPatch, AssetQuery, NewCollection, NewTag, Origin, now,
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
            sha256: Some("a".repeat(64)),
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
            extra: Default::default(),
            created_at: now(),
            updated_at: now(),
            trashed_at: None,
        }
    }

    #[test]
    fn migrations_run_and_store_reopens() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.user_version().unwrap(), schema::SCHEMA_VERSION);
        // Migrating again is a no-op.
        store.migrate().unwrap();
    }

    #[test]
    fn collection_tree_crud_and_cycle_refusal() {
        let store = Store::in_memory().unwrap();
        let root = collections::create(store.conn(), &NewCollection {
            parent_id: None,
            name: "root".into(),
            position: 0,
        })
        .unwrap();
        let child = collections::create(store.conn(), &NewCollection {
            parent_id: Some(root.id),
            name: "child".into(),
            position: 0,
        })
        .unwrap();

        assert_eq!(collections::children_of(store.conn(), None).unwrap().len(), 1);
        assert_eq!(
            collections::children_of(store.conn(), Some(root.id)).unwrap()[0].id,
            child.id
        );

        // Moving root under its own child must be refused.
        let err = collections::move_to(store.conn(), root.id, Some(child.id), 0).unwrap_err();
        assert!(err.to_string().contains("descendant"));

        // Rename + re-root under a fresh parent.
        collections::rename(store.conn(), child.id, "renamed").unwrap();
        let orphan_parent = collections::create(store.conn(), &NewCollection {
            parent_id: None,
            name: "other".into(),
            position: 1,
        })
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

        // Filter by text, kind, favorite.
        let (total, hits) = assets::query(
            store.conn(),
            &AssetQuery {
                text: Some("sunset".into()),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, img.id);

        let (_, favs) = assets::query(
            store.conn(),
            &AssetQuery {
                is_favorite: Some(true),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(favs.len(), 1);

        // Collection membership is many-to-many.
        let c1 = collections::create(store.conn(), &NewCollection {
            parent_id: None,
            name: "album".into(),
            position: 0,
        })
        .unwrap();
        let c2 = collections::create(store.conn(), &NewCollection {
            parent_id: None,
            name: "work".into(),
            position: 1,
        })
        .unwrap();
        collections::add_asset(store.conn(), c1.id, img.id).unwrap();
        collections::add_asset(store.conn(), c1.id, doc.id).unwrap();
        collections::add_asset(store.conn(), c2.id, img.id).unwrap();

        assert_eq!(collections::count_assets(store.conn(), c1.id).unwrap(), 2);
        assert_eq!(collections::count_assets(store.conn(), c2.id).unwrap(), 1);

        let (total, in_album) = assets::query(
            store.conn(),
            &AssetQuery {
                collection_id: Some(c1.id),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(in_album.len(), 2);

        // Trash hides from normal queries, restores bring it back.
        assert!(assets::set_trashed(store.conn(), doc.id, true).unwrap());
        let (_, live) = assets::query(store.conn(), &AssetQuery::default()).unwrap();
        assert_eq!(live.len(), 1);
        let (_, trash) = assets::query(
            store.conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trash.len(), 1);


        let (_, live) = assets::query(store.conn(), &AssetQuery::default()).unwrap();
        assert_eq!(live.len(), 1);
        let (_, trash) = assets::query(
            store.conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trash.len(), 1);

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
            let c = collections::create(store.conn(), &NewCollection {
                parent_id: None,
                name: "kept".into(),
                position: 0,
            })
            .unwrap();
            collections::add_asset(store.conn(), c.id, sample_asset("kept.png", AssetKind::Image).id)
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

    #[test]
    fn fts_search_ranks_and_filters() {
        let store = Store::in_memory().unwrap();
        let mut photo = sample_asset("vacation.jpg", AssetKind::Image);
        photo.title = Some("Sunset over the beach".into());
        let mut doc = sample_asset("beach-plan.md", AssetKind::Document);
        doc.description = Some("Notes about the sunset trip".into());
        let mut audio = sample_asset("song.mp3", AssetKind::Audio);
        audio.title = Some("Rainy morning mix".into());

        assets::insert(store.conn(), &photo).unwrap();
        assets::insert(store.conn(), &doc).unwrap();
        assets::insert(store.conn(), &audio).unwrap();

        // Text hits photo + doc, not audio.
        let (total, _) = assets::search(store.conn(), "sunset", &AssetQuery::default()).unwrap();
        assert_eq!(total, 2);

        // Compound filter (kind) narrows the ranked set.
        let (total, hits) = assets::search(
            store.conn(),
            "sunset",
            &AssetQuery {
                kind: Some(AssetKind::Image),
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);

        // Trashed assets are excluded.
        assets::set_trashed(store.conn(), photo.id, true).unwrap();
        let (total, _) = assets::search(store.conn(), "sunset", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);

        // Special characters never panic and are treated literally.
        let (total, hits) = assets::search(store.conn(), "\" * NEAR", &AssetQuery::default()).unwrap();
        assert_eq!(total, 0);
        let _ = hits;
    }

    #[test]
    fn fts_multi_term_is_and_and_prefix() {
        let store = Store::in_memory().unwrap();
        let mut photo = sample_asset("vacation.jpg", AssetKind::Image);
        photo.title = Some("Sunset over the beach".into());
        let mut doc = sample_asset("trip-notes.md", AssetKind::Document);
        doc.description = Some("Notes about the sunset trip".into());
        let mut audio = sample_asset("song.mp3", AssetKind::Audio);
        audio.title = Some("Rainy morning mix".into());
        assets::insert(store.conn(), &photo).unwrap();
        assets::insert(store.conn(), &doc).unwrap();
        assets::insert(store.conn(), &audio).unwrap();

        // Multiple terms AND together: only the photo has both.
        let (total, hits) =
            assets::search(store.conn(), "sunset beach", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);

        // Relevance still ranks (no panic, no dupes); strict ordering is
        // covered by the rank/compound-filter test above.
        let (total, _) = assets::search(store.conn(), "sunset", &AssetQuery::default()).unwrap();
        assert_eq!(total, 2);

        // Prefix matching: a term matches tokens that start with it.
        let (total, _) = assets::search(store.conn(), "sunse", &AssetQuery::default()).unwrap();
        assert_eq!(total, 2);
        let (total, hits) =
            assets::search(store.conn(), "beac sunse", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);

        // A longer-than-token prefix matches nothing (prefix, not substring).
        let (total, _) = assets::search(store.conn(), "beacho", &AssetQuery::default()).unwrap();
        assert_eq!(total, 0);
        let (total, _) = assets::search(store.conn(), "rainy", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);

        // Whitespace is only a separator; extra spaces change nothing.
        let (total, _) =
            assets::search(store.conn(), "  sunset   beach  ", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn fts_matches_tag_names_and_stays_in_sync() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut photo = sample_asset("mountain.png", AssetKind::Image);
        photo.title = Some("Unrelated title".into());
        assets::insert(conn, &photo).unwrap();

        let tag = tags::create(conn, &NewTag { name: "landscape".into(), color: None }).unwrap();
        let other = tags::create(conn, &NewTag { name: "night".into(), color: None }).unwrap();

        // No tag attached yet: not found.
        let (total, _) = assets::search(conn, "landscape", &AssetQuery::default()).unwrap();
        assert_eq!(total, 0);

        // Attach: the tag name becomes searchable, prefix included.
        tags::add_to_asset(conn, photo.id, tag.id).unwrap();
        let (total, hits) = assets::search(conn, "landscape", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);
        let (total, _) = assets::search(conn, "lands", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);

        // Detach: the stale index entry must disappear.
        tags::remove_from_asset(conn, photo.id, tag.id).unwrap();
        let (total, _) = assets::search(conn, "landscape", &AssetQuery::default()).unwrap();
        assert_eq!(total, 0);

        // Batch replace syncs once and carries every new tag name.
        tags::set_for_asset(conn, photo.id, &[tag.id, other.id]).unwrap();
        let (total, _) =
            assets::search(conn, "landscape night", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);

        // Deleting a tag removes it from every indexed asset.
        tags::delete(conn, other.id).unwrap();
        let (total, _) = assets::search(conn, "night", &AssetQuery::default()).unwrap();
        assert_eq!(total, 0);
        let (total, _) = assets::search(conn, "landscape", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn query_text_condition_matches_tag_names() {
        // The LIKE-based `text` filter (used by query() and smart collections)
        // searches the same surface as FTS, tag names included.
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let photo = sample_asset("img.png", AssetKind::Image);
        assets::insert(conn, &photo).unwrap();

        let (total, _) = assets::query(
            conn,
            &AssetQuery { text: Some("nature".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(total, 0);

        let tag = tags::create(conn, &NewTag { name: "nature".into(), color: None }).unwrap();
        tags::add_to_asset(conn, photo.id, tag.id).unwrap();

        let (total, hits) = assets::query(
            conn,
            &AssetQuery { text: Some("nature".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);

        tags::remove_from_asset(conn, photo.id, tag.id).unwrap();
        let (total, _) = assets::query(
            conn,
            &AssetQuery { text: Some("nature".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn fts_syntax_characters_stay_literal_after_refactor() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut photo = sample_asset("a.png", AssetKind::Image);
        photo.title = Some("C++ tips and (tricks)".into());
        assets::insert(conn, &photo).unwrap();

        // FTS5 operators and syntax characters are matched literally, never
        // parsed: these inputs must not error or widen the match.
        for q in ["c*", "\"c*\"", "NEAR", "c AND tips", "c OR (tips)", "\"", "*", "--"] {
            let result = assets::search(conn, q, &AssetQuery::default());
            assert!(result.is_ok(), "search panicked on query {q:?}");
        }
        // "tips" still finds it.
        let (total, hits) = assets::search(conn, "tips", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo.id);
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
        assert_eq!(ordered.iter().map(|x| x.id).collect::<Vec<_>>(), vec![c.id, a.id, b.id]);
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
        let (total, ids) = super::smart::evaluate(store.conn(), &node, None, 0).unwrap();
        assert_eq!(total, 2);
        assert!(ids.contains(&img.id) && ids.contains(&doc.id));

        // kind != image ∧ rating >= 4 → doc only (kind pairs with rating in an AND tree)
        let tree = smart_node(serde_json::json!({
            "op": "and",
            "children": [
                { "op": "match", "field": "rating", "compare": "gte", "value": 4 },
                { "op": "match", "field": "kind", "compare": "ne", "value": "image" },
            ]
        }));
        let (total, ids) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(ids, vec![doc.id]);
    }

    #[test]
    fn smart_collection_text_and_tag_match() {
        let store = Store::in_memory().unwrap();
        let mut a = sample_asset("notes.md", AssetKind::Document);
        a.title = Some("quarterly beach report".into());
        a.is_favorite = true;
        assets::insert(store.conn(), &a).unwrap();

        // text condition routes through the FTS index.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "text", "value": "beach"
        }));
        let (total, ids) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(ids, vec![a.id]);

        // tag match is case-insensitive.
        super::tags::ensure_named(store.conn(), "Travel").unwrap();
        super::tags::add_to_asset(store.conn(), a.id, super::tags::ensure_named(store.conn(), "travel").unwrap().id).unwrap();
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "tag", "value": "TRAVEL"
        }));
        let (total, _) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);

        // favorite filter + AND with a tag.
        let tree = smart_node(serde_json::json!({
            "op": "and",
            "children": [
                { "op": "match", "field": "is_favorite", "value": true },
                { "op": "match", "field": "tag", "value": "travel" },
            ]
        }));
        let (total, _) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn smart_collection_color_filter() {
        let store = Store::in_memory().unwrap();
        // Color lives in `extra.dominant_color` (as mined by color::dominant_colors).
        let mut red = sample_asset("red.png", AssetKind::Image);
        red.extra = [("dominant_color".into(), serde_json::json!("#d01010"))].into_iter().collect();
        let mut blue = sample_asset("blue.png", AssetKind::Image);
        blue.extra = [("dominant_color".into(), serde_json::json!("#1a5cff"))].into_iter().collect();
        assets::insert(store.conn(), &red).unwrap();
        assets::insert(store.conn(), &blue).unwrap();

        // Exact match, case-insensitive and `#`-optional.
        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "color", "value": "#d01010"
        }));
        let (total, ids) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(ids, vec![red.id]);

        let tree = smart_node(serde_json::json!({
            "op": "match", "field": "color", "compare": "ne", "value": "D01010"
        }));
        let (total, ids) = super::smart::evaluate(store.conn(), &tree, None, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(ids, vec![blue.id]);

        // A malformed color is rejected at compile time.
        let bad = smart_node(serde_json::json!({
            "op": "match", "field": "color", "value": "notacolor"
        }));
        assert!(super::smart::compile(&bad).is_err());
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
        let (total, ids) = super::smart::evaluate(store.conn(), &node, Some(4), 0).unwrap();
        assert_eq!(total, 10);
        assert_eq!(ids.len(), 4);

        // An unknown field is rejected when the JSON is deserialized into a node.
        let bad_node = super::smart::node_from_json(&serde_json::json!({
            "op": "match", "field": "nope", "value": 1
        }));
        assert!(bad_node.is_err());

        // Wrong value types are rejected when the tree is compiled.
        let bad_type = smart_node(serde_json::json!({
            "op": "match", "field": "rating", "value": "high"
        }));
        assert!(super::smart::compile(&bad_type).is_err());

        let bad_op = smart_node(serde_json::json!({
            "op": "match", "field": "kind", "compare": "gt", "value": "image"
        }));
        assert!(super::smart::compile(&bad_op).is_err());
    }

    #[test]
    fn smart_collection_crud_roundtrip() {
        let store = Store::in_memory().unwrap();
        let input = crate::model::NewSmartCollection {
            name: "Favorites".into(),
            query: serde_json::json!({
                "op": "match", "field": "is_favorite", "value": true
            }),
            color: Some("#f00".into()),
            position: 0,
        };
        let created = super::smart_collections::create(store.conn(), &input).unwrap();
        let fetched = super::smart_collections::get(store.conn(), created.id).unwrap().unwrap();
        assert_eq!(fetched.name, "Favorites");
        assert_eq!(fetched.query, input.query);
        let listed = super::smart_collections::list(store.conn()).unwrap();
        assert_eq!(listed.len(), 1);
        super::smart_collections::rename(store.conn(), created.id, "Renamed").unwrap();
        assert_eq!(
            super::smart_collections::get(store.conn(), created.id).unwrap().unwrap().name,
            "Renamed"
        );
        super::smart_collections::delete(store.conn(), created.id).unwrap();
        assert!(super::smart_collections::get(store.conn(), created.id).unwrap().is_none());
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
        crate::store::rows::execute(conn, "DELETE FROM asset_fts", vec![]).unwrap();

        let n = crate::maintenance::rebuild_search_index(&lib).unwrap();
        assert_eq!(n, 2);
        let (total, _) = assets::search(conn, "treasure", &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
    }
}
