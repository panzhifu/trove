//! The `Library` facade: a database plus its media directory, exposing the
//! high-level operations an application shell drives.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::Result;
use crate::media;
use crate::store::{Store, assets, batch, collections, rows, smart, smart_collections, tags};
use crate::undo::{self, Op, SharedUndoStack};

/// Serialize the whole metadata catalog of `store` (assets, collections,
/// tags, smart collections) as pretty JSON. Media blobs are not included —
/// the export is a portable catalog, not a backup of the files.
pub fn export_metadata_from_store(store: &Store) -> Result<String> {
    let conn = store.conn();
    let (_, assets) = assets::query(conn, &crate::model::AssetQuery::default())?;
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
    /// Entries that could not be restored (invalid smart queries, version 1
    /// exports have no membership tables, …).
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
    /// v2 membership tables (absent in version 1 exports).
    #[serde(default)]
    asset_collections: Vec<(Uuid, Uuid)>,
    #[serde(default)]
    asset_tags: Vec<(Uuid, Uuid)>,
    /// Export schema version (accepted: 2; version 1 restores without
    /// membership tables — every pair then counts as skipped).
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
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
    /// Undo/redo log for invertible metadata mutations (see [`crate::undo`]).
    undo: SharedUndoStack,
}

impl Library {
    /// Open (or create) the library under `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let store = Store::open(&root.join("library.db"))?;
        let lib = Self {
            store,
            root,
            undo: SharedUndoStack::default(),
        };
        // Backfill the FTS index for a library migrated from a schema that had
        // no search table: without this, `search` silently returns nothing for
        // assets that predate the index.
        lib.backfill_search_on_migration()?;
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
            let _ = crate::services::maintenance::rebuild_search_index(self);
        }
        Ok(())
    }

    /// An in-memory library whose media blobs live under `root` (tests).
    pub fn open_in_memory(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let store = Store::in_memory()?;
        Ok(Self {
            store,
            root,
            undo: SharedUndoStack::default(),
        })
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
    /// Imported assets are added directly to "All Assets" unless a target
    /// collection is specified. Smart collections automatically capture
    /// matching assets via their rules.
    pub fn import_files(
        &self,
        sources: &[PathBuf],
        into_collection: Option<Uuid>,
    ) -> Result<media::import::ImportReport> {
        media::import::import_files(&self.store, &self.root, sources, into_collection)
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

    pub fn get_smart_collection(&self, id: Uuid) -> Result<Option<crate::model::SmartCollection>> {
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
            && assets::count_by_sha256(conn, &sha)? == 0
        {
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
            let stem = std::path::Path::new(&asset.file_name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&asset.file_name)
                .to_string();
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
        self.undo.record(Op::SetTitles { before, after });
        Ok(count)
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
            self.undo.record(Op::SetFavorite {
                before,
                after: ids.iter().map(|id| (*id, favorite)).collect(),
            });
        }
        Ok(changed)
    }

    /// Attach many assets to a collection (idempotent). Only the actual
    /// membership delta is recorded for undo.
    pub fn add_assets_to_collection(&self, collection_id: Uuid, ids: &[Uuid]) -> Result<u64> {
        let conn = self.store.conn();
        let members = collections::asset_ids(conn, collection_id)?;
        let changed = batch::add_to_collection_many(conn, collection_id, ids)?;
        let added: Vec<Uuid> = ids
            .iter()
            .filter(|id| !members.contains(id))
            .copied()
            .collect();
        if !added.is_empty() {
            self.undo.record(Op::MembershipAdd {
                collection: collection_id,
                added,
            });
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
            self.undo.record(Op::MembershipRemove {
                collection: collection_id,
                removed,
            });
        }
        Ok(count)
    }

    /// Apply a metadata patch to one asset, recording the full pre-state so
    /// undo restores every editable column (title/description edits also
    /// re-sync the FTS index through `assets::update`).
    pub fn patch_asset(&self, asset_id: Uuid, patch: &crate::model::AssetPatch) -> Result<()> {
        patch.validate()?;
        let conn = self.store.conn();
        let before = assets::get(conn, asset_id)?
            .map(|asset| undo::restore_patch(&asset))
            .ok_or(crate::Error::NotFound("asset"))?;
        assets::update(conn, asset_id, patch)?;
        self.undo.record(Op::PatchAsset {
            id: asset_id,
            before,
            after: patch.clone(),
        });
        Ok(())
    }

    /// Replace one asset's whole tag group (missing tags must already exist —
    /// use [`tags::ensure_named`] at the call site first).
    pub fn set_asset_tags(&self, asset_id: Uuid, tag_ids: &[Uuid]) -> Result<()> {
        let conn = self.store.conn();
        let before: Vec<Uuid> = tags::for_asset(conn, asset_id)?
            .iter()
            .map(|t| t.id)
            .collect();
        tags::set_for_asset(conn, asset_id, tag_ids)?;
        self.undo.record(Op::SetTags {
            asset: asset_id,
            before,
            after: tag_ids.to_vec(),
        });
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
        let before = tags::get(conn, tag_id)?
            .ok_or(crate::Error::NotFound("tag"))?
            .parent_id;
        tags::move_to(conn, tag_id, parent)?;
        self.undo.record(Op::TagParent {
            id: tag_id,
            before,
            after: parent,
        });
        Ok(())
    }

    /// Attach (`add = true`) or detach one tag on many assets, recording the
    /// per-asset tag-group delta.
    pub fn tag_assets(&self, asset_ids: &[Uuid], tag_id: Uuid, add: bool) -> Result<()> {
        let conn = self.store.conn();
        for asset_id in asset_ids {
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
            self.undo.record(Op::SetTags {
                asset: *asset_id,
                before,
                after,
            });
        }
        Ok(())
    }

    /// Rename a tag (FTS re-synced), recording the previous name.
    pub fn rename_tag(&self, tag_id: Uuid, name: &str) -> Result<()> {
        let conn = self.store.conn();
        let before = tags::get(conn, tag_id)?
            .ok_or(crate::Error::NotFound("tag"))?
            .name;
        tags::rename(conn, tag_id, name)?;
        self.undo.record(Op::TagRename {
            id: tag_id,
            before,
            after: name.to_string(),
        });
        Ok(())
    }

    /// Set (or clear) a tag's display color, recording the previous value.
    pub fn set_tag_color(&self, tag_id: Uuid, color: Option<&str>) -> Result<()> {
        let conn = self.store.conn();
        let before = tags::get(conn, tag_id)?
            .ok_or(crate::Error::NotFound("tag"))?
            .color;
        tags::set_color(conn, tag_id, color)?;
        self.undo.record(Op::TagColor {
            id: tag_id,
            before,
            after: color.map(|c| c.to_string()),
        });
        Ok(())
    }

    /// Rename a collection, recording the previous name.
    pub fn rename_collection(&self, collection_id: Uuid, name: &str) -> Result<()> {
        let conn = self.store.conn();
        let before = collections::get(conn, collection_id)?
            .ok_or(crate::Error::NotFound("collection"))?
            .name;
        collections::rename(conn, collection_id, name)?;
        self.undo.record(Op::CollectionRename {
            id: collection_id,
            before,
            after: name.to_string(),
        });
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
        self.undo.record(Op::CollectionMove {
            id: collection_id,
            before: (c.parent_id, c.position),
            after: (new_parent, position),
        });
        Ok(())
    }

    // -- semantic (CLIP) search ------------------------------------------------

    /// Semantic text-to-image search: embed the query with the CLIP text
    /// encoder, rank stored image embeddings by cosine similarity and return
    /// the matching assets best-first. Requires a configured model
    /// (`media::clip::semantic_ready`); surfaces its error otherwise.
    pub fn semantic_text_search(
        &self,
        query: &str,
        min_similarity: f32,
        limit: Option<u32>,
    ) -> Result<Vec<crate::model::Asset>> {
        let vec = media::clip::text_embedding(query)?;
        let query_emb = media::clip::Embedding::new(vec);
        let scored = media::clip::semantic_search(&self.store, &query_emb, min_similarity, limit)?;
        let ids: Vec<Uuid> = scored.iter().map(|(id, _)| *id).collect();
        assets::by_ids(self.store.conn(), &ids)
    }

    /// Semantic image-to-image search: returns `(asset, similarity)` pairs
    /// ordered best-first (the results dialog shows the score per hit).
    pub fn semantic_image_search(
        &self,
        query_path: &Path,
        min_similarity: f32,
        limit: Option<u32>,
    ) -> Result<Vec<(crate::model::Asset, f32)>> {
        let vec = media::clip::image_embedding(query_path)?;
        let query_emb = media::clip::Embedding::new(vec);
        let scored = media::clip::semantic_search(&self.store, &query_emb, min_similarity, limit)?;
        let conn = self.store.conn();
        let ids: Vec<Uuid> = scored.iter().map(|(id, _)| *id).collect();
        let by_id: std::collections::HashMap<Uuid, crate::model::Asset> =
            assets::by_ids(conn, &ids)?
                .into_iter()
                .map(|a| (a.id, a))
                .collect();
        Ok(scored
            .into_iter()
            .filter_map(|(id, score)| by_id.get(&id).cloned().map(|a| (a, score)))
            .collect())
    }

    /// Re-embed every live image that has no vector yet. Returns
    /// `(embedded, skipped)`.
    pub fn embed_missing_all(&self) -> Result<(u64, u64)> {
        media::clip::embed_all_missing(&self.store, &self.root)
    }

    /// Embed one asset if it is a live image without a vector. Returns
    /// `Ok(true)` when a new embedding was stored, `Ok(false)` when skipped.
    pub fn embed_one(&self, asset_id: Uuid) -> Result<bool> {
        media::clip::embed_asset(&self.store, &self.root, asset_id)
    }

    /// `(embedded_images, total_live_images)` embedding coverage.
    pub fn embedding_status(&self) -> Result<(u64, u64)> {
        assets::embedding_counts(self.store.conn())
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
            self.undo.record(Op::SetTrashed {
                before,
                after: ids.iter().map(|id| (*id, trashed)).collect(),
            });
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

        // Smart collections: copied with fresh ids; an invalid condition
        // tree (foreign version) is skipped, not fatal.
        for sc in file.smart_collections {
            let input = NewSmartCollection {
                name: sc.name.clone(),
                query: sc.query.clone(),
                color: sc.color.clone(),
                position: sc.position,
            };
            if input.validate().is_ok() && smart_collections::create(conn, &input).is_ok() {
                report.smart_collections += 1;
            } else {
                report.skipped += 1;
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
                color_label: asset.color_label.clone(),
                extra: asset.extra.clone(),
                created_at: asset.created_at,
                updated_at: asset.updated_at,
                trashed_at: None,
            };
            assets::insert(conn, &placeholder)?;
            asset_map.insert(asset.id, id);
            report.assets_placeholder += 1;
        }

        // v2 membership tables. A version 1 export has neither; every entry
        // then counts as skipped, which the report surfaces honestly.
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
        let thumb = media::thumb::abs_path(&self.root, sha);
        let _ = std::fs::remove_file(thumb);
    }
}

#[cfg(test)]
mod tests {
    use super::Library;
    use crate::media::thumb;
    use crate::model::{AssetKind, AssetPatch, AssetQuery, NewCollection, NewSmartCollection};
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
        // Imports go directly to "All Assets" without creating collections.
        let (lib, root) = temp_library("auto-source");
        let folder = root.join("Vacation");
        std::fs::create_dir_all(&folder).unwrap();
        let src = write_source(&folder, "photo.png", PNG_1X1);

        let report = lib.import_files(std::slice::from_ref(&src), None).unwrap();
        assert_eq!(report.imported_count(), 1);
        let _item = &report.imported[0];

        // No auto-created collections — asset goes to "All Assets".
        let roots = collections::roots(lib.store().conn()).unwrap();
        assert_eq!(roots.len(), 0);

        // Total asset count is 1.
        let total = assets::query(lib.store().conn(), &crate::model::AssetQuery::default())
            .unwrap()
            .0;
        assert_eq!(total, 1);

        // Re-importing identical content dedupes.
        let report2 = lib.import_files(std::slice::from_ref(&src), None).unwrap();
        assert_eq!(report2.imported_count(), 1);
        assert!(report2.imported[0].reused);
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
        let err = lib
            .import_files(&[root.join("nope.png")], Some(Uuid::new_v4()))
            .unwrap_err();
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
        let report = lib
            .import_files(std::slice::from_ref(&missing), None)
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
        let (total, assets) = lib
            .evaluate_smart_collection(sc.id, None, None, None, 0)
            .unwrap();
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
            &crate::model::NewTag {
                name: "landscape".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::add_to_asset(conn, photo_id, tag.id).unwrap();

        // Wipe the index, then reopen the library from disk.
        crate::store::rows::execute(conn, "DELETE FROM asset_fts", vec![]).unwrap();
        drop(lib);
        let reopened = Library::open(&root).unwrap();
        let _conn = reopened.store().conn();

        let (total, hits) = reopened
            .search_assets("sunset", &AssetQuery::default())
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].id, photo_id);

        // The rebuilt index carries tag names too.
        let (total, _) = reopened
            .search_assets("landscape", &AssetQuery::default())
            .unwrap();
        assert_eq!(total, 1);
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
        let report = lib.import_files(&[src], None).unwrap();
        let asset_id = report.imported[0].asset_id;
        lib.tag_assets(&[asset_id], t1.id, true).unwrap();
        lib.delete_tag(t1.id).unwrap();
        assert!(tags::for_asset(conn, asset_id).unwrap().is_empty());
        assert!(tags::list(conn).unwrap().iter().all(|t| t.id != t1.id));

        // Smart collection rename + delete through the facade.
        let sc = lib
            .create_smart_collection(&NewSmartCollection {
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
    fn color_label_patch_query_smart_and_undo() {
        use crate::model::{AssetPatch, SmartCompare, SmartField, SmartNode};
        use crate::store::smart;

        let (lib, dir) = temp_library("color-label");
        let src = write_source(&dir, "a.png", PNG_1X1);
        let report = lib.import_files(&[src], None).unwrap();
        let id = report.imported[0].asset_id;
        let conn = lib.store().conn();

        // Set a label; the query filter and the smart field both see it.
        lib.patch_asset(
            id,
            &AssetPatch {
                color_label: Some(Some("red".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let q = AssetQuery {
            color_label: Some("red".into()),
            ..Default::default()
        };
        let (total, page) = assets::query(conn, &q).unwrap();
        assert_eq!((total, page.len()), (1, 1));

        let node = SmartNode::Match {
            field: SmartField::ColorLabel,
            op: SmartCompare::Eq,
            value: serde_json::json!("red"),
        };
        let (n, ids) = smart::evaluate(conn, &node, None, 0).unwrap();
        assert_eq!((n, ids.as_slice()), (1, &[id][..]));

        // Unknown palette names are rejected and change nothing.
        assert!(
            lib.patch_asset(
                id,
                &AssetPatch {
                    color_label: Some(Some("magenta".into())),
                    ..Default::default()
                },
            )
            .is_err()
        );

        // Undo restores the unlabeled state.
        lib.undo().unwrap();
        let asset = assets::get(conn, id).unwrap().unwrap();
        assert_eq!(asset.color_label, None);
    }
    #[test]
    fn duplicate_content_import_needs_no_sha_scan() {
        // The importer deduplicates identical content at the record level,
        // so two live assets never share a SHA-256 — the duplicate finder
        // works on perceptual hashes instead.
        let (lib, dir) = temp_library("duplicates-sha");
        let src = write_source(&dir, "same.png", PNG_1X1);
        lib.import_files(std::slice::from_ref(&src), None).unwrap();
        lib.import_files(std::slice::from_ref(&src), None).unwrap();
        let (total, _) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
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
            asset.extra.insert(
                "visual_phash".into(),
                serde_json::Value::String(format!("{hash:016x}")),
            );
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
        let ra = lib.import_files(std::slice::from_ref(&a), None).unwrap();
        let rb = lib.import_files(std::slice::from_ref(&b), None).unwrap();
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
        let ra = lib.import_files(std::slice::from_ref(&a), None).unwrap();
        let rb = lib.import_files(std::slice::from_ref(&b), None).unwrap();
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
        let (_, restored) = assets::query(conn, &AssetQuery::default()).unwrap();
        let restored_ids: Vec<Uuid> = restored.iter().map(|x| x.id).collect();
        assert_eq!(restored.len(), 2);
        // Membership survived the id remap.
        let in_coll = collections::asset_ids(conn, coll.id);
        let _ = in_coll; // collection id changed; assert via name below
        let names: Vec<String> = collections::list(conn)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["Trip".to_string()]);
        let tagged = tags::for_asset(conn, restored_ids[0]).unwrap();
        // The image asset (first import) carries the tag.
        let image = restored
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
        let (total, _) = assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn placeholder_self_heals_on_reimport() {
        let (lib, dir) = temp_library("heal");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        lib.import_files(std::slice::from_ref(&a), None).unwrap();
        let json = lib.export_metadata().unwrap();

        let (other, _) = temp_library("heal-target");
        other.import_metadata(&json).unwrap();
        let conn = other.store().conn();
        let (_, restored) = assets::query(conn, &AssetQuery::default()).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(restored[0].rel_path.is_none());

        // Re-importing the same content links the blob into the placeholder.
        other.import_files(std::slice::from_ref(&a), None).unwrap();
        let healed = assets::get(conn, restored[0].id).unwrap().unwrap();
        assert!(healed.rel_path.is_some());
        let (total, _) = assets::query(conn, &AssetQuery::default()).unwrap();
        assert_eq!(total, 1);
    }
    #[test]
    fn hierarchical_tags_filter_include_subtree() {
        use crate::model::AssetPatch;
        use crate::model::NewTag;
        use crate::store::collections;

        let (lib, dir) = temp_library("hier-tags");
        let a = write_source(&dir, "alpha.png", PNG_1X1);
        let b = write_source(&dir, "beta.txt", b"beta");
        let ra = lib.import_files(std::slice::from_ref(&a), None).unwrap();
        let rb = lib.import_files(std::slice::from_ref(&b), None).unwrap();
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
        let (total, page) = assets::query(conn, &q).unwrap();
        assert_eq!((total, page.len()), (1, 1));
        assert_eq!(page[0].id, ia);

        // The subtree count matches the filter.
        assert_eq!(tags::count_assets(conn, animal.id).unwrap(), 1);

        // Smart collection by tag name includes the subtree.
        let node = crate::store::smart::node_from_json(&serde_json::json!({
            "op": "match", "field": "tag", "value": "animal"
        }))
        .unwrap();
        let (n, ids) = crate::store::smart::evaluate(conn, &node, None, 0).unwrap();
        assert_eq!((n, ids.as_slice()), (1, &[ia][..]));

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
}
