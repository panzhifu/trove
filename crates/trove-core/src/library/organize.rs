//! Organisation: tags, collections, view history, XMP sidecars, and undo/redo.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

impl Library {
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
            if asset.placement().is_trashed() {
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
                rating: asset.rating.map(|rating| rating.get()),
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
        let op = Op::SetTags {
            asset: asset_id,
            before,
            after: tag_ids.to_vec(),
        };
        let desc = OpDesc::new(OpAction::TagSet, target, 1);
        undo::apply_atomic(conn, |tx| {
            tags::set_for_asset(tx, asset_id, tag_ids)?;
            self.undo.record(tx, op, &desc)
        })?;
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
        let op = Op::TagParent {
            id: tag_id,
            before,
            after: parent,
        };
        let desc = OpDesc::new(OpAction::TagMoved, Some(tag.name), 1);
        undo::apply_atomic(conn, |tx| {
            tags::move_to(tx, tag_id, parent)?;
            self.undo.record(tx, op, &desc)
        })?;
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
            // The write needs the same list the row will hold, and the row owns
            // it — so the group is cloned rather than borrowed twice.
            let write = after.clone();
            let op = Op::SetTags {
                asset: *asset_id,
                before,
                after,
            };
            let desc = OpDesc::new(OpAction::TagSet, target, 1);
            // One transaction per asset: a batch that dies halfway leaves the
            // earlier assets consistent and undoable, instead of one rolled-back
            // statement in the middle of a multi-asset write.
            undo::apply_atomic(conn, |tx| {
                tags::set_for_asset(tx, *asset_id, &write)?;
                self.undo.record(tx, op, &desc)
            })?;
        }
        Ok(())
    }

    /// Rename a tag (search index re-synced), recording the previous name.
    pub fn rename_tag(&self, tag_id: Uuid, name: &str) -> Result<()> {
        let conn = self.store.conn();
        let tag = tags::get(conn, tag_id)?.ok_or(crate::Error::NotFound("tag"))?;
        let desc = OpDesc::new(OpAction::TagRenamed, Some(tag.name.clone()), 1);
        let op = Op::TagRename {
            id: tag_id,
            before: tag.name,
            after: name.to_string(),
        };
        undo::apply_atomic(conn, |tx| {
            tags::rename(tx, tag_id, name)?;
            self.undo.record(tx, op, &desc)
        })?;
        Ok(())
    }

    /// Set (or clear) a tag's display color, recording the previous value.
    pub fn set_tag_color(&self, tag_id: Uuid, color: Option<&str>) -> Result<()> {
        let conn = self.store.conn();
        let tag = tags::get(conn, tag_id)?.ok_or(crate::Error::NotFound("tag"))?;
        let op = Op::TagColor {
            id: tag_id,
            before: tag.color,
            after: color.map(|c| c.to_string()),
        };
        let desc = OpDesc::new(OpAction::TagColored, Some(tag.name), 1);
        undo::apply_atomic(conn, |tx| {
            tags::set_color(tx, tag_id, color)?;
            self.undo.record(tx, op, &desc)
        })?;
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
    pub fn set_smart_collection_query(
        &self,
        id: Uuid,
        query: &crate::model::SmartNode,
    ) -> Result<()> {
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
        let op = Op::CollectionRename {
            id: collection_id,
            before: collection.name,
            after: name.to_string(),
        };
        undo::apply_atomic(conn, |tx| {
            collections::rename(tx, collection_id, name)?;
            self.undo.record(tx, op, &desc)
        })?;
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
        let op = Op::CollectionMove {
            id: collection_id,
            before: (c.parent_id, c.position),
            after: (new_parent, position),
        };
        let desc = OpDesc::new(OpAction::CollectionMoved, Some(c.name), 1);
        undo::apply_atomic(conn, |tx| {
            collections::move_to(tx, collection_id, new_parent, position)?;
            self.undo.record(tx, op, &desc)
        })?;
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
        self.undo.undo_len(self.store.conn())
    }

    pub fn redo_len(&self) -> usize {
        self.undo.redo_len(self.store.conn())
    }

    /// Descriptions of the last `n` undoable operations, most recent first
    /// (status bar).
    pub fn undo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.undo.undo_entries(self.store.conn(), n)
    }

    /// Descriptions of the last `n` redoable operations, next-first.
    pub fn redo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.undo.redo_entries(self.store.conn(), n)
    }

    /// Undo up to `steps` operations in sequence; returns how many were
    /// applied (stops early when the history runs out or one fails).
    pub fn undo_steps(&self, steps: usize) -> Result<usize> {
        self.undo.undo_steps(steps, self.store.conn())
    }

    pub(super) fn set_assets_trashed(&self, ids: &[Uuid], trashed: bool) -> Result<u64> {
        let conn = self.store.conn();
        let mut flips = Vec::with_capacity(ids.len());
        for id in ids {
            let was = assets::get(conn, *id)?
                .map(|a| a.placement().is_trashed())
                .unwrap_or(false);
            flips.push(Flip {
                id: *id,
                before: was,
                after: trashed,
            });
        }
        let op = Op::SetTrashed { flips };
        let desc = OpDesc::counted(
            if trashed {
                OpAction::Trash
            } else {
                OpAction::Restore
            },
            ids.len(),
        );
        // Nothing is recorded when the write changed no rows: an undo step that
        // would put the database back exactly as it was is noise in the panel,
        // and the empty case is why `if changed > 0` used to be here.
        let changed = undo::apply_atomic(conn, |tx| {
            let changed = batch::set_trashed_many(tx, ids, trashed)?;
            if changed > 0 {
                self.undo.record(tx, op, &desc)?;
            }
            Ok(changed)
        })?;
        Ok(changed)
    }
}
