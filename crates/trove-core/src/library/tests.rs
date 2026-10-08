//! Unit tests for the `Library` facade.
//!
//! Moved out of `library/mod.rs`; the methods under test are spread
//! across the sibling domain files, which changes nothing for a test
//! that goes through the public facade.

use super::Library;
use crate::media::thumb;
use crate::model::{AssetKind, AssetQuery, NewCollection, NewSmartCollection, Rating};
use crate::store::{assets, collections, tags};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A minimal valid 1x1 PNG.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
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
    let all = assets::query(conn, &AssetQuery::live()).unwrap();
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
    let all2 = assets::query(conn, &AssetQuery::live()).unwrap();
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

/// A selection dragged out of the window answers in the caller's order, and
/// whatever has no file to hand over — a linked original that left the disk,
/// an id the library does not know — is skipped rather than offered as a dead
/// entry.
#[test]
fn asset_files_answers_a_selection_in_order_skipping_the_fileless() {
    use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

    let (lib, root) = temp_library("asset-files");
    let outside = std::env::temp_dir().join(format!("trove-asset-files-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&outside).unwrap();

    // One stored record (content copied into the library's own storage) and
    // one linked record (the file stays where it is). Different content, so
    // the second import cannot fold onto the first.
    let linked_src = write_source(&outside, "linked.png", PNG_1X1);
    let stored_src = write_source(&outside, "stored.txt", b"stored content");
    lib.import_into_store(std::slice::from_ref(&stored_src), None)
        .unwrap();
    let staged = stage_all(
        &root,
        &root.join("cache"),
        std::slice::from_ref(&linked_src),
        ImportStorage::Link,
        &std::sync::atomic::AtomicBool::new(false),
    );
    let linked_report = commit_staged_all(lib.store().conn(), None, staged);
    assert_eq!(linked_report.imported_count(), 1, "linked import");

    let conn = lib.store().conn();
    let all = assets::query(conn, &AssetQuery::live()).unwrap();
    assert_eq!(all.items.len(), 2);
    let linked_id = all
        .items
        .iter()
        .find(|a| a.location().is_linked())
        .expect("linked import")
        .id;
    let stored_id = all
        .items
        .iter()
        .find(|a| !a.location().is_linked())
        .expect("stored import")
        .id;
    let stored_blob = lib
        .root()
        .join(stored_rel(&assets::get(conn, stored_id).unwrap().unwrap()));

    // The order is the caller's, not the library's.
    assert_eq!(
        lib.asset_files(&[linked_id, stored_id]),
        vec![linked_src.clone(), stored_blob.clone()]
    );

    std::fs::remove_file(&linked_src).unwrap();
    assert_eq!(
        lib.asset_files(&[linked_id, stored_id, Uuid::new_v4()]),
        vec![stored_blob]
    );
    assert!(lib.asset_files(&[]).is_empty());

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
    let page = assets::query(lib.store().conn(), &crate::model::AssetQuery::live()).unwrap();
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

    let page = assets::query(lib.store().conn(), &AssetQuery::live()).unwrap();
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

    let page = assets::query(lib.store().conn(), &AssetQuery::live()).unwrap();
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
            ..AssetQuery::trashed()
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

    tags::add_to_asset(
        lib.store().conn(),
        crate::model::AssetId(asset_id),
        crate::model::TagId(red.id),
    )
    .unwrap();
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
            ..AssetQuery::live()
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

    let page = assets::query(lib.store().conn(), &AssetQuery::live()).unwrap();
    assert_eq!(page.total, 1);
    let trashed = assets::query(
        lib.store().conn(),
        &AssetQuery {
            ..AssetQuery::trashed()
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
    for asset in assets::query(lib.store().conn(), &AssetQuery::live())
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

    let total = |query: &str| lib.search_assets(query, &AssetQuery::live()).unwrap().total;
    let ids = |query: &str| {
        let page = lib.search_assets(query, &AssetQuery::live()).unwrap();
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
    let all = assets::query(lib.store().conn(), &AssetQuery::live()).unwrap();
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
    let hits = lib.search_assets("sunset", &AssetQuery::live()).unwrap();
    assert_eq!(hits.total, 1);
    assert_eq!(hits.items[0].id, photo_id);

    // A smart collection over the same terms evaluates through the facade.
    let sc = lib
        .create_smart_collection(&NewSmartCollection {
            parent_id: None,
            name: "Dockpics".into(),
            query: crate::store::smart::node_from_json(&serde_json::json!({
                "op": "and",
                "children": [
                    { "op": "match", "field": "text", "value": "sunset" },
                    { "op": "match", "field": "is_favorite", "value": true },
                ]
            }))
            .unwrap(),
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
    let all = assets::query(conn, &AssetQuery::live()).unwrap();
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
    tags::add_to_asset(
        conn,
        crate::model::AssetId(photo_id),
        crate::model::TagId(tag.id),
    )
    .unwrap();

    // Wipe the index, then reopen the library from disk: the reconcile
    // step must notice the empty index and rebuild it from the rows.
    lib.text_index().wipe().unwrap();
    drop(lib);
    let reopened = Library::open(&root, root.join("cache")).unwrap();
    let _conn = reopened.store().conn();

    let hits = reopened
        .search_assets("sunset", &AssetQuery::live())
        .unwrap();
    assert_eq!(hits.total, 1);
    assert_eq!(hits.items[0].id, photo_id);

    // The rebuilt index carries tag names too.
    let page = reopened
        .search_assets("landscape", &AssetQuery::live())
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
            query: crate::store::smart::node_from_json(&serde_json::json!({
                "op": "match",
                "field": "text",
                "value": "x",
            }))
            .unwrap(),
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
            ..AssetQuery::live()
        },
    )
    .unwrap();
    assert_eq!((page.total, page.items.len()), (1, 1));
    let page = assets::query(
        conn,
        &AssetQuery {
            commercial_use: Some(false),
            ..AssetQuery::live()
        },
    )
    .unwrap();
    assert_eq!((page.total, page.items.len()), (1, 1));
    // Unverified rows match neither clearance filter.
    let page = assets::query(
        conn,
        &AssetQuery {
            commercial_use: Some(true),
            ..AssetQuery::live()
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
    let page = assets::query(lib.store().conn(), &AssetQuery::live()).unwrap();
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

    tags::add_to_asset(conn, crate::model::AssetId(ia), crate::model::TagId(cat.id)).unwrap();
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
        ..AssetQuery::live()
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
    let ids = crate::store::smart::evaluate(conn, Some(lib.text_index()), &node, None, 0).unwrap();
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
        ..AssetQuery::live()
    };
    let page = assets::query(lib.store().conn(), &q).unwrap();
    assert_eq!((page.total, page.items.len()), (1, 1));
    assert_eq!(page.items[0].file_name, "b.txt");
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

/// A linked text asset saves back over its source: the file gets the new
/// characters, the record follows the content, and a second save of the
/// same text is a no-op. A file that moved underneath the viewer between
/// read and save refuses — the compare-and-swap working — and a stored
/// asset refuses outright, because "saving" over a content-addressed blob
/// would leave the record's hash pointing at content the file no longer has.
#[test]
fn a_text_save_writes_the_linked_source_conflicts_on_change_and_refuses_a_stored_asset() {
    let (lib, _root) = temp_library("text-save");
    let home = std::env::temp_dir().join(format!("trove-text-src-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let src = home.join("kept.md");
    std::fs::write(&src, b"first draft").unwrap();

    let report = lib.link_files(std::slice::from_ref(&src), None).unwrap();
    let id = report.imported[0].asset_id;
    let conn = lib.store().conn();
    let before = assets::get(conn, id).unwrap().unwrap();
    let expected_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
    let expected_size = std::fs::metadata(&src).unwrap().len();

    let outcome = lib
        .save_linked_text(
            id,
            "second draft, edited",
            "utf-8",
            false,
            expected_mtime,
            expected_size,
        )
        .unwrap();
    assert_eq!(outcome, crate::library::lifecycle::TextSaveOutcome::Written);
    assert_eq!(std::fs::read(&src).unwrap(), b"second draft, edited");
    let after = assets::get(conn, id).unwrap().unwrap();
    let new_hash = after.content_hash.clone().unwrap();
    assert_ne!(new_hash, before.content_hash.clone().unwrap());
    assert_eq!(
        new_hash.as_str(),
        crate::media::hash::hash_bytes(b"second draft, edited"),
        "the record describes the file that now exists"
    );
    assert_eq!(after.size_bytes, "second draft, edited".len() as u64);
    // The same text again: the encoded buffer is byte-identical, so the
    // save is a no-op rather than a fresh mtime on the file.
    let outcome = lib
        .save_linked_text(
            id,
            "second draft, edited",
            "utf-8",
            false,
            std::fs::metadata(&src).unwrap().modified().unwrap(),
            std::fs::metadata(&src).unwrap().len(),
        )
        .unwrap();
    assert_eq!(
        outcome,
        crate::library::lifecycle::TextSaveOutcome::Unchanged
    );

    // The file changed under the viewer (an outside editor wrote it): the
    // stale read's save is refused, and the file keeps the other editor's
    // bytes.
    std::fs::write(&src, b"an outside editor was here").unwrap();
    let outcome = lib
        .save_linked_text(
            id,
            "second draft, edited",
            "utf-8",
            false,
            expected_mtime,
            expected_size,
        )
        .unwrap();
    assert_eq!(
        outcome,
        crate::library::lifecycle::TextSaveOutcome::Conflict
    );
    assert_eq!(std::fs::read(&src).unwrap(), b"an outside editor was here");

    // A stored asset has no writable source: its bytes are a blob the
    // record's hash names, not the user's file.
    let stored_src = home.join("copied.txt");
    std::fs::write(&stored_src, b"library copy").unwrap();
    let report = lib
        .import_into_store(std::slice::from_ref(&stored_src), None)
        .unwrap();
    let stored_id = report.imported[0].asset_id;
    let outcome = lib
        .save_linked_text(
            stored_id,
            "nope",
            "utf-8",
            false,
            expected_mtime,
            expected_size,
        )
        .unwrap();
    assert_eq!(
        outcome,
        crate::library::lifecycle::TextSaveOutcome::NotWritable
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
        let page = assets::query(conn, &AssetQuery::live()).unwrap();
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
            rating: Some(Some(Rating::new(5).unwrap())),
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

    // Backfill on the task manager; wait for the outcome channel. The
    // provider arrives as a factory, exactly as the app hands it over: the
    // task thread builds it.
    let make = provider.clone();
    let (task_id, rx) = lib
        .start_embedding_backfill("mock-embed", move || Ok(make.clone()))
        .unwrap();
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
                    ..AssetQuery::live()
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
        .semantic_search(provider.as_ref(), "   ", &AssetQuery::live())
        .unwrap();
    assert!(page.items.is_empty() && page.total == 0);

    // A structural filter still applies to the semantic candidates.
    let page = lib
        .semantic_search(
            provider.as_ref(),
            "red car in snow",
            &AssetQuery {
                kind: Some(AssetKind::Document),
                ..AssetQuery::live()
            },
        )
        .unwrap();
    assert_eq!(page.total, 0, "no documents in an image library");

    // The reset button clears one model and leaves no vectors behind;
    // coverage reports zero again.
    assert_eq!(lib.delete_embeddings("mock-embed").unwrap(), 3);
    assert_eq!(lib.embedding_coverage("mock-embed").unwrap(), (0, 3));
    let page = lib
        .semantic_search(provider.as_ref(), "red car in snow", &AssetQuery::live())
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

/// A library an earlier build left dirty must not open with the task panel
/// claiming interrupted work that never existed.
///
/// `record_start` refuses a resident service's kind now, but the rows already on
/// disk are the point: each one was left `running` by a quit that could not write
/// its terminal row, so they surface on *every* open until something folds them
/// away. That something runs inside `open`, before the read — which is what this
/// test can fail on that the unit tests cannot: retire after the read and the
/// stale row still reaches the panel.
#[test]
fn a_stale_service_row_is_retired_before_the_interrupted_read() {
    let root = std::env::temp_dir().join(format!("trove-lib-retire-{}", Uuid::new_v4()));
    let cache = root.join("cache");
    let interrupted_kinds = |lib: &Library| -> Vec<String> {
        lib.interrupted_tasks()
            .iter()
            .map(|entry| entry.kind.name().to_string())
            .collect()
    };

    let lib = Library::open(&root, &cache).unwrap();
    lib.store()
        .conn()
        .execute(
            "INSERT INTO task_journal (task_id, kind, label, status, started_at) \
             VALUES (?1, 'watch-scan', 'watch', 'running', 'then')",
            [Uuid::new_v4().to_string()],
        )
        .unwrap();
    drop(lib);

    let lib = Library::open(&root, &cache).unwrap();
    let stale = interrupted_kinds(&lib);
    assert!(
        stale.is_empty(),
        "the resident service's leftover row read as interrupted work: {stale:?}"
    );

    // The half that stops this passing by switching the report off: a job of an
    // ordinary kind, unfinished because the process really did die mid-work,
    // must still be surfaced.
    lib.store()
        .conn()
        .execute(
            "INSERT INTO task_journal (task_id, kind, label, status, started_at) \
             VALUES (?1, 'import', 'import 320 of 1200', 'running', 'then')",
            [Uuid::new_v4().to_string()],
        )
        .unwrap();
    drop(lib);

    let lib = Library::open(&root, &cache).unwrap();
    assert_eq!(
        interrupted_kinds(&lib),
        vec!["import".to_string()],
        "a genuinely interrupted job is still reported"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A restore is a rescue, so the assertions are all about what comes back and
/// what is kept: the snapshot's records, the overwritten state written as a
/// snapshot of its own, and — because a snapshot carries only the database —
/// files never touched. The reopened library has to re-index too: its text index
/// was built over the rows the restore replaced.
#[test]
fn restoring_a_snapshot_puts_the_library_back_and_leaves_a_way_back() {
    let (lib, root) = temp_library("restore-snapshot");
    let cache = root.join("cache");
    let inbox = root.join("incoming");
    std::fs::create_dir_all(&inbox).unwrap();
    let counted = |lib: &Library, where_: &str| -> i64 {
        lib.store()
            .conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM assets {where_}"),
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    let live = |lib: &Library| counted(lib, "WHERE trashed_at IS NULL");

    let photo = write_source(&inbox, "kept.png", PNG_1X1);
    let id = import_linked(&lib, &root, &photo);
    assert_eq!(live(&lib), 1, "one live asset when the snapshot is taken");
    let snapshot = lib.create_backup().unwrap();

    // The change the snapshot cannot know about: the asset went to the trash.
    lib.trash_assets(&[id]).unwrap();
    assert_eq!(live(&lib), 0, "and it is not live any more");

    let before = lib.restore_backup(&snapshot).unwrap();
    assert!(
        before.is_file() && before != snapshot,
        "the state the restore replaced is kept as a snapshot of its own"
    );
    drop(lib);

    let reopened = Library::open(&root, &cache).unwrap();
    assert_eq!(
        live(&reopened),
        1,
        "the trashed state went away with the overwritten database"
    );
    assert!(
        photo.is_file(),
        "a restore never touches files, so the record and its file are back together"
    );
    assert_eq!(
        reopened.rebuild_text_index().unwrap(),
        1,
        "a rebuilt index describes the restored rows, not the overwritten ones"
    );

    // And the way back exists: that first-after-the-fact snapshot still holds the
    // trashed row, so a restore that turned out to be the wrong choice is itself
    // restorable.
    let prior =
        rusqlite::Connection::open_with_flags(&before, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        prior
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE trashed_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "the pre-restore snapshot kept the state it replaced"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The reason the history is a table: a mutation made in one session is taken
/// back in the next one.
#[test]
fn an_undo_step_survives_reopening_the_library() {
    let (lib, root) = temp_library("undo-reopen");
    let cache = root.join("cache");
    let inbox = root.join("incoming");
    std::fs::create_dir_all(&inbox).unwrap();
    let photo = write_source(&inbox, "photo.png", PNG_1X1);
    let id = import_linked(&lib, &root, &photo);

    lib.set_assets_favorite(&[id], true).unwrap();
    assert_eq!(lib.undo_len(), 1, "one step, in this session");
    drop(lib);

    let lib = Library::open(&root, &cache).unwrap();
    assert_eq!(
        lib.undo_len(),
        1,
        "a fresh session reads the same history out of the database"
    );
    assert!(lib.undo().unwrap());
    assert_eq!(lib.undo_len(), 0);
    assert!(
        !assets::get(lib.store().conn(), id)
            .unwrap()
            .unwrap()
            .is_favorite,
        "and it took the favorite flag back with it"
    );
    // Redo *is* available here — this session is the one that did the undo.
    assert_eq!(lib.redo_len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

/// A mutation whose undo row cannot be written does not happen at all.
///
/// This is the reason the record call runs inside the mutation's transaction
/// rather than after it. The alternative is not "undo is missing later" but a
/// change that landed, cannot be taken back, and never said so — the same shape
/// of silence this repo spent a whole round removing from journal and settings
/// writes. The trigger stands in for any reason the insert can fail: a full
/// disk, a bad page, a row the schema refuses.
#[test]
fn a_mutation_whose_undo_row_cannot_be_written_leaves_no_change() {
    let (lib, root) = temp_library("undo-atomic");
    let inbox = root.join("incoming");
    std::fs::create_dir_all(&inbox).unwrap();
    let photo = write_source(&inbox, "photo.png", PNG_1X1);
    let id = import_linked(&lib, &root, &photo);

    lib.store()
        .conn()
        .execute_batch(
            "CREATE TRIGGER undo_log_refuses BEFORE INSERT ON undo_log
             BEGIN SELECT RAISE(ABORT, 'the undo row is refused'); END;",
        )
        .unwrap();

    let error = lib.set_assets_favorite(&[id], true).unwrap_err();
    assert!(
        error.to_string().contains("undo row is refused"),
        "the refusal should reach the caller: {error}"
    );
    assert_eq!(lib.undo_len(), 0, "and nothing was recorded");
    assert!(
        !assets::get(lib.store().conn(), id)
            .unwrap()
            .unwrap()
            .is_favorite,
        "the favorite flag is exactly where it was"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn asset_count_measures_live_rows() {
    // The number the license gate measures: live assets only. A trashed row
    // stops counting — emptying the trash reopens the free tier's door — and
    // the count is the store's answer, never a cached shadow.
    let (lib, root) = temp_library("asset-count");
    assert_eq!(lib.asset_count(), 0);

    let one = write_source(&root, "one.png", PNG_1X1);
    lib.import_into_store(std::slice::from_ref(&one), None)
        .unwrap();
    assert_eq!(lib.asset_count(), 1);

    // Different bytes, or the store's content dedup collapses the pair.
    let two = write_source(&root, "two.png", b"a different picture");
    lib.import_into_store(std::slice::from_ref(&two), None)
        .unwrap();
    assert_eq!(lib.asset_count(), 2);

    let first = assets::query(lib.store().conn(), &AssetQuery::live())
        .unwrap()
        .items[0]
        .id;
    assets::set_trashed(lib.store().conn(), first, true).unwrap();
    assert_eq!(lib.asset_count(), 1, "trashed rows stop counting");

    std::fs::remove_dir_all(&root).ok();
}

/// The retention sweep purges exactly the trashed rows older than the
/// window — backdated ones go, fresh ones and live ones stay, and zero days
/// means the caller disabled the sweep entirely.
#[test]
fn the_retention_sweep_purges_only_expired_trash() {
    let (lib, root) = temp_library("retention");
    let expired_src = root.join("expired.png");
    let fresh_src = root.join("fresh.png");
    // Distinct pixels, not distinct names: content-addressed dedup would
    // collapse two identical files into one asset.
    image::RgbImage::new(2, 1)
        .save_with_format(&expired_src, image::ImageFormat::Png)
        .unwrap();
    image::RgbImage::new(1, 2)
        .save_with_format(&fresh_src, image::ImageFormat::Png)
        .unwrap();
    lib.import_into_store(&[expired_src, fresh_src], None)
        .unwrap();
    let conn = lib.store().conn();
    let all = assets::query(conn, &AssetQuery::live()).unwrap().items;
    assert_eq!(all.len(), 2);
    let (expired, fresh) = (&all[0], &all[1]);
    assets::set_trashed(conn, expired.id, true).unwrap();
    assets::set_trashed(conn, fresh.id, true).unwrap();
    // Backdate the first one past every window.
    let old = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
    crate::store::rows::execute(
        conn,
        "UPDATE assets SET trashed_at = ?1 WHERE id = ?2",
        vec![
            crate::store::rows::bind_opt_ts(Some(
                chrono::DateTime::parse_from_rfc3339(&old)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            )),
            crate::store::rows::uuid(expired.id).into(),
        ],
    )
    .unwrap();

    assert_eq!(lib.purge_expired_trash(30).unwrap(), 1);
    assert!(
        assets::get(conn, expired.id).unwrap().is_none(),
        "expired row purged"
    );
    assert!(
        assets::get(conn, fresh.id).unwrap().is_some(),
        "fresh row stays"
    );
    // Live rows are never touched, whatever their age would say.
    assert_eq!(lib.purge_expired_trash(0).unwrap(), 0);

    std::fs::remove_dir_all(&root).ok();
}

/// A video import mines the playback facts the preview starts its decoder
/// from — fps and audio presence ride the container probe into the asset's
/// facts, so entering the preview never has to probe. Skipped without
/// ffmpeg/ffprobe.
#[test]
fn a_video_import_mines_the_playback_facts() {
    let ffprobe = |tool: &str| {
        std::process::Command::new(tool)
            .arg("-version")
            .output()
            .is_ok()
    };
    if !ffprobe("ffmpeg") || !ffprobe("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not on PATH");
        return;
    }
    let (lib, root) = temp_library("videofacts");
    let src = root.join("clip.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&src)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success());
    lib.import_into_store(std::slice::from_ref(&src), None)
        .unwrap();

    let conn = lib.store().conn();
    let asset = crate::store::assets::query(
        conn,
        &crate::model::AssetQuery {
            kind: Some(crate::model::AssetKind::Video),
            ..crate::model::AssetQuery::live()
        },
    )
    .unwrap()
    .items
    .pop()
    .expect("the video asset imported");

    assert!(asset.facts.video.fps.is_some(), "fps mined for the preview");
    assert_eq!(asset.facts.video.has_audio, Some(true));
    assert!(asset.width.is_some() && asset.height.is_some());
    assert!(asset.duration_ms.is_some());

    std::fs::remove_dir_all(&root).ok();
}

// ============================================================================
// Tag merge + bulk delete
// ============================================================================

fn tag_id_named(lib: &Library, name: &str) -> uuid::Uuid {
    lib.list_tags()
        .unwrap()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tag {name} missing"))
        .id
}

/// Renaming a tag onto an existing name is a merge: the named tag stays, the
/// renamed one is gone, and an asset carrying both keeps a single relation.
/// The merge is undoable — the undo puts the source row and its relations
/// back under the old id, and the redo merges again.
#[test]
fn a_rename_onto_an_existing_name_merges_and_undoes() {
    let (lib, root) = temp_library("tag-merge");
    for name in ["one.txt", "two.txt", "three.txt"] {
        let dir = std::env::temp_dir().join(format!("trove-tag-merge-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, format!("content of {name}")).unwrap();
        lib.import_into_store(std::slice::from_ref(&file), None)
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
    let conn = lib.store().conn();
    let all = assets::query(conn, &AssetQuery::live()).unwrap().items;
    let (a, b, c) = (all[0].id, all[1].id, all[2].id);

    lib.create_tag("风光", None).unwrap();
    lib.create_tag("风景", None).unwrap();
    let scenery = tag_id_named(&lib, "风景");
    lib.tag_assets(&[a, b], tag_id_named(&lib, "风光"), true)
        .unwrap();
    lib.tag_assets(&[b, c], scenery, true).unwrap();

    // The rename lands on an existing name, so it merges.
    lib.rename_tag(tag_id_named(&lib, "风光"), "风景").unwrap();
    let names: Vec<String> = lib
        .list_tags()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["风景".to_string()], "the source tag is gone");
    let carried = |id| {
        tags::for_asset(conn, id)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(carried(a), vec!["风景".to_string()]);
    assert_eq!(
        carried(b).len(),
        1,
        "an asset that had both tags keeps one relation"
    );
    assert_eq!(carried(c), vec!["风景".to_string()]);

    // Undo: the source row comes back under its old id with its relations.
    let merged_id = tag_id_named(&lib, "风景"); // target unchanged
    lib.undo().unwrap();
    assert_eq!(lib.list_tags().unwrap().len(), 2);
    assert_eq!(
        tags::for_asset(conn, a).unwrap()[0].id,
        tag_id_named(&lib, "风光"),
        "the restored row keeps the id the relations remember"
    );
    assert_eq!(carried(b).len(), 2, "the both-tags asset has both back");
    // Redo: merged again.
    lib.redo().unwrap();
    assert_eq!(lib.list_tags().unwrap().len(), 1);
    assert_eq!(carried(a), vec!["风景".to_string()]);
    assert_eq!(tags::get(conn, merged_id).unwrap().unwrap().name, "风景");

    std::fs::remove_dir_all(&root).ok();
}

/// The merge refuses what it cannot answer for: itself, a missing side, and
/// a tag whose children would be orphaned by the re-point.
#[test]
fn a_merge_refuses_what_it_cannot_answer_for() {
    let (lib, root) = temp_library("tag-merge-refuse");
    lib.create_tag("parent", None).unwrap();
    lib.create_tag("child", lib.tag_by_name("parent").unwrap().map(|t| t.id))
        .unwrap();
    lib.create_tag("other", None).unwrap();
    let parent = tag_id_named(&lib, "parent");
    let child = tag_id_named(&lib, "child");
    let other = tag_id_named(&lib, "other");

    assert!(lib.merge_tags(parent, parent).is_err(), "into itself");
    assert!(
        lib.merge_tags(parent, Uuid::new_v4()).is_err(),
        "target does not exist"
    );
    assert!(
        lib.merge_tags(parent, other).is_err(),
        "a tag with children refuses"
    );
    // The leaf still merges.
    lib.merge_tags(child, other).unwrap();
    assert_eq!(lib.list_tags().unwrap().len(), 2);

    std::fs::remove_dir_all(&root).ok();
}

/// A batch delete takes each tag and its relations, leaves the rest alone,
/// and reports how many rows went.
#[test]
fn delete_many_takes_each_tag_and_its_relations() {
    let (lib, root) = temp_library("tag-delete-many");
    let dir = std::env::temp_dir().join(format!("trove-tag-delete-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.txt");
    std::fs::write(&file, "content").unwrap();
    lib.import_into_store(std::slice::from_ref(&file), None)
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let conn = lib.store().conn();
    let asset = assets::query(conn, &AssetQuery::live()).unwrap().items[0].id;

    for name in ["gone-a", "gone-b", "stays"] {
        lib.create_tag(name, None).unwrap();
    }
    let gone_a = tag_id_named(&lib, "gone-a");
    let gone_b = tag_id_named(&lib, "gone-b");
    let stays = tag_id_named(&lib, "stays");
    lib.tag_assets(std::slice::from_ref(&asset), gone_a, true)
        .unwrap();

    let deleted = lib
        .delete_tags_many(&[gone_a, gone_b, Uuid::new_v4()])
        .unwrap();
    assert_eq!(deleted, 2, "the unknown id deletes nothing");
    let names: Vec<String> = lib
        .list_tags()
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["stays".to_string()]);
    assert!(tags::for_asset(conn, asset).unwrap().is_empty());

    // A subtree delete goes through the same door: parent and child both go.
    lib.create_tag("parent", None).unwrap();
    lib.create_tag("sub", lib.tag_by_name("parent").unwrap().map(|t| t.id))
        .unwrap();
    let parent = tag_id_named(&lib, "parent");
    let ids = lib.tag_subtree_ids(parent).unwrap();
    assert_eq!(ids.len(), 2);
    lib.delete_tags_many(&ids).unwrap();
    assert_eq!(lib.list_tags().unwrap().len(), 1);
    assert_eq!(tags::get(conn, stays).unwrap().unwrap().name, "stays");

    std::fs::remove_dir_all(&root).ok();
}

/// A file Trove writes itself beside the media — a subtitle sidecar — becomes
/// a linked asset once; re-writing it refreshes that same record (hash and
/// size move) instead of adding a second row for the same path.
#[test]
fn ensure_linked_file_is_idempotent_and_refreshes_content() {
    let (lib, root) = temp_library("ensure-linked");
    let source = root.join("clip.srt");
    std::fs::write(&source, b"1\n00:00:00,000 --> 00:00:01,000\nhi\n").unwrap();

    let first = lib.ensure_linked_file(&source).unwrap();
    // Same path, same content: the same record.
    assert_eq!(lib.ensure_linked_file(&source).unwrap(), first);

    // The pipeline draws a text card for it, so the grid has a picture to
    // show rather than only a kind icon.
    let hash = lib.asset(first).unwrap().unwrap().content_hash.unwrap();
    let thumb = thumb::abs_path(&root.join("cache"), &hash);
    assert!(thumb.is_file(), "subtitle text card was not generated");

    // Rewritten content (an edited subtitle): still the same record, not a
    // second one for a path the library already knows.
    std::fs::write(&source, b"1\n00:00:00,000 --> 00:00:02,000\nbye\n").unwrap();
    assert_eq!(lib.ensure_linked_file(&source).unwrap(), first);

    let asset = lib.asset(first).unwrap().unwrap();
    assert!(asset.location().is_linked());
    assert_eq!(asset.file_name, "clip.srt");

    std::fs::remove_dir_all(&root).ok();
}

/// The whole cutout path against a real library and the real checkpoint:
/// import one image, run the job exactly as the context menu does, and check
/// a transparent PNG of the source's own dimensions came out the other side.
/// Needs the 168 MB model on disk, so it is a gate to run by hand rather than
/// a cost every `cargo test` pays.
///
/// ```text
/// TROVE_U2NET=/path/u2net.onnx cargo test -p trove-core --lib cutout -- --ignored
/// ```
#[test]
#[ignore = "needs the u2net checkpoint on disk"]
fn a_cutout_run_writes_a_transparent_png_per_asset() {
    use crate::tasks::matting::CutoutOptions;

    let model = std::env::var("TROVE_U2NET").expect("set TROVE_U2NET to a u2net.onnx");
    let (lib, root) = temp_library("cutout");
    let outside = std::env::temp_dir().join(format!("trove-cutout-src-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&outside).unwrap();

    // A subject on a ground, big enough that "some pixels kept, some dropped"
    // is a claim about the mask rather than about interpolation noise.
    let subject = image::RgbaImage::from_fn(640, 480, |x, y| {
        if (x as i32 - 320).abs() < 120 && (y as i32 - 240).abs() < 90 {
            image::Rgba([230, 200, 40, 255])
        } else {
            let v = 40 + (x as u8) / 4;
            image::Rgba([v, v, v, 255])
        }
    });
    let source = outside.join("subject.png");
    subject.save(&source).unwrap();

    let report = lib
        .import_into_store(std::slice::from_ref(&source), None)
        .unwrap();
    assert_eq!(report.imported_count(), 1);
    let ids: Vec<Uuid> = assets::query(lib.store().conn(), &AssetQuery::live())
        .unwrap()
        .items
        .iter()
        .map(|asset| asset.id)
        .collect();

    let options = CutoutOptions {
        db_path: root.join("library.db"),
        data_root: root.clone(),
        out_dir: outside.join("out"),
        model_path: PathBuf::from(&model),
        only: ids,
    };
    let (_task, rx) = lib.start_cutout(options).unwrap();
    let outcome = rx.recv().unwrap();
    assert_eq!(outcome.cut, 1, "outcome says {outcome:?}");
    assert_eq!(outcome.written.len(), 1);

    let written = &outcome.written[0];
    assert!(written.is_file(), "the run reported {}", written.display());
    let cutout = image::open(written).unwrap().to_rgba8();
    assert_eq!((cutout.width(), cutout.height()), (640, 480));
    let kept = cutout.pixels().filter(|p| p[3] > 200).count();
    let dropped = cutout.pixels().filter(|p| p[3] < 60).count();
    assert!(
        kept > 1000 && dropped > 1000,
        "kept {kept}, dropped {dropped} of {}",
        640 * 480
    );

    std::fs::remove_dir_all(&outside).ok();
    std::fs::remove_dir_all(&root).ok();
}
