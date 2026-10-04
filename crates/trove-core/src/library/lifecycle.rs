//! Asset lifecycle: trash/restore/purge, patching, relinking, and the in-place image edit.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

/// What a text save did — an enum rather than a `bool` pair because the app
/// shows a different message for every arm and the arms are not errors:
/// a [`Conflict`](Self::Conflict) is the CAS working, not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSaveOutcome {
    /// The bytes were written and the record now describes them.
    Written,
    /// The encoded buffer is byte-identical to what the record holds —
    /// nothing to write, nothing to re-hash.
    Unchanged,
    /// The file's mtime or size moved under the viewer between its read and
    /// the save. Nothing was written; reloading the file is the fix.
    Conflict,
    /// This asset has no writable source: it is stored (its bytes live in a
    /// content-addressed blob, where "saving" means re-importing, not
    /// editing in place), trashed, or its linked file is not on disk.
    NotWritable,
}

impl Library {
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
        let page = assets::query(self.store.conn(), &AssetQuery::trashed())?;
        let ids: Vec<Uuid> = page.items.iter().map(|asset| asset.id).collect();
        Ok(self.purge_assets_against(&ids, inbox)?.purged)
    }

    /// Purge every trashed asset older than `days`, the open-time sweep behind
    /// the retention setting. Zero days means the caller disabled the sweep
    /// and never reaches here. Returns the number removed.
    ///
    /// `trashed_at` values all go through the same `to_rfc3339` serialization
    /// (UTC, same suffix shape), so a lexicographic comparison against a
    /// cutoff rendered the same way is an honest date comparison.
    pub fn purge_expired_trash(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(i64::from(days))).to_rfc3339();
        let ids: Vec<Uuid> = crate::store::rows::query_map(
            self.store.conn(),
            "SELECT id FROM assets \
             WHERE trashed_at IS NOT NULL AND trashed_at < ?1",
            vec![rusqlite::types::Value::Text(cutoff)],
            |row| crate::store::rows::req_uuid(row, 0),
        )?;
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(self
            .purge_assets_against(&ids, &collect::inbox_dir())?
            .purged)
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
        let mut flips: Vec<Flip<Option<String>>> = Vec::with_capacity(ids.len());
        let mut n = start_number;
        for id in ids {
            let Some(asset) = assets::get(conn, *id)? else {
                continue;
            };
            let stem = asset.file_stem();
            let title = pattern
                .replace("{n}", &n.to_string())
                .replace("{name}", &stem);
            flips.push(Flip {
                id: *id,
                before: asset.title,
                after: Some(title),
            });
            n += 1;
        }
        let count = flips.len() as u64;
        if count == 0 {
            return Ok(0);
        }
        let desc = OpDesc::counted(OpAction::Rename, flips.len());
        // The whole rename and its undo row are one transaction: a rename that
        // stopped halfway through the batch would otherwise be recorded as one
        // step undoable back to the start, while some of the rows still held
        // their old titles.
        undo::apply_atomic(conn, |tx| {
            for flip in &flips {
                assets::update(
                    tx,
                    flip.id,
                    &crate::model::AssetPatch {
                        title: Some(flip.after.clone()),
                        ..Default::default()
                    },
                )?;
            }
            self.undo.record(tx, Op::SetTitles { flips }, &desc)
        })?;
        Ok(count)
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
        let mut flips = Vec::with_capacity(ids.len());
        for id in ids {
            let was = assets::get(conn, *id)?
                .map(|a| a.is_favorite)
                .unwrap_or(false);
            flips.push(Flip {
                id: *id,
                before: was,
                after: favorite,
            });
        }
        let desc = OpDesc::counted(
            if favorite {
                OpAction::Favorite
            } else {
                OpAction::Unfavorite
            },
            ids.len(),
        );
        let changed = undo::apply_atomic(conn, |tx| {
            let changed = batch::set_favorite_many(tx, ids, favorite)?;
            if changed > 0 {
                self.undo.record(tx, Op::SetFavorite { flips }, &desc)?;
            }
            Ok(changed)
        })?;
        Ok(changed)
    }

    /// Attach many assets to a collection (idempotent). Only the actual
    /// membership delta is recorded for undo.
    pub fn add_assets_to_collection(&self, collection_id: Uuid, ids: &[Uuid]) -> Result<u64> {
        let conn = self.store.conn();
        let target = collections::get(conn, collection_id)?.map(|c| c.name);
        let members = collections::asset_ids(conn, collection_id)?;
        let added: Vec<Uuid> = ids
            .iter()
            .filter(|id| !members.contains(id))
            .copied()
            .collect();
        let desc = OpDesc::new(OpAction::AddedToCollection, target, added.len());
        let changed = undo::apply_atomic(conn, |tx| {
            let changed = batch::add_to_collection_many(tx, collection_id, ids)?;
            if !added.is_empty() {
                self.undo.record(
                    tx,
                    Op::MembershipAdd {
                        collection: collection_id,
                        added,
                    },
                    &desc,
                )?;
            }
            Ok(changed)
        })?;
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
        let count = removed.len();
        let desc = OpDesc::new(OpAction::RemovedFromCollection, target, count);
        undo::apply_atomic(conn, |tx| {
            for id in &removed {
                collections::remove_asset(
                    tx,
                    crate::model::CollectionId(collection_id),
                    crate::model::AssetId(*id),
                )?;
            }
            if count > 0 {
                self.undo.record(
                    tx,
                    Op::MembershipRemove {
                        collection: collection_id,
                        removed,
                    },
                    &desc,
                )?;
            }
            Ok(())
        })?;
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
        let desc = OpDesc::new(OpAction::Edit, Some(asset.file_name), 1);
        undo::apply_atomic(conn, |tx| {
            assets::update(tx, asset_id, patch)?;
            self.undo.record(
                tx,
                Op::PatchAsset {
                    id: asset_id,
                    before: Box::new(before),
                    after: Box::new(patch.clone()),
                },
                &desc,
            )
        })?;
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

    /// Ensure a *linked* asset exists for `path` and return its id.
    ///
    /// Used for files Trove writes itself beside the user's media — a
    /// subtitle `.srt` — so they show in the library like any imported file.
    /// A record already linking this exact path is refreshed (its hash and
    /// size move when the file was rewritten) rather than duplicated; a hash
    /// lookup would not do, because editing a subtitle changes its content
    /// hash and would insert a second row for the same file.
    pub fn ensure_linked_file(&self, path: &Path) -> Result<Uuid> {
        let source = path.to_string_lossy().into_owned();
        if let Some(existing) = assets::find_linked_by_path(self.store.conn(), &source)? {
            let (hash, size) = crate::media::hash::hash_file_cached(self.cache(), path)?;
            let changed = existing.content_hash.as_deref() != Some(hash.as_str());
            assets::set_linked_media_columns(
                self.store.conn(),
                existing.id,
                &hash,
                size,
                None,
                None,
            )?;
            // Rewritten content (an edited subtitle) keys a *new* thumbnail
            // path, and the grid would find nothing there and fall back to the
            // kind icon. Redraw the card for the new hash so the tile keeps a
            // picture.
            if changed {
                let _ = crate::media::thumb::regenerate(self.cache(), &hash, existing.kind, path);
            }
            return Ok(existing.id);
        }
        let report = self.link_files(&[path.to_path_buf()], None)?;
        report
            .imported
            .first()
            .map(|item| item.asset_id)
            .ok_or_else(|| crate::Error::Validation("linked file produced no asset".into()))
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
        if asset.placement().is_trashed() || asset.kind != crate::model::AssetKind::Image {
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

        let old_hash = asset.content_hash.clone();
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
        if let Some(old_hash) = old_hash.as_deref()
            && assets::count_by_content_hash(self.store.conn(), old_hash)? == 0
        {
            media::thumb::remove_derived(self.cache(), old_hash);
        }
        media::thumb::regenerate(self.cache(), &hash, asset.kind, &source);
        Ok(true)
    }

    /// Write an edited text buffer back over a *linked* asset's source file.
    ///
    /// The write-back twin of [`Self::batch_edit_images`]'s linked arm, with
    /// one guard the image edit does not need: the viewer holds a whole-file
    /// read from an earlier moment, so the save is a compare-and-swap on the
    /// file's `mtime` and size as the read observed them — if anything else
    /// touched the file since, the answer is [`TextSaveOutcome::Conflict`]
    /// and nothing is written. (The image edit re-derives its bytes from the
    /// file at save time, so it needs no such guard; the text viewer's bytes
    /// are stale by construction the moment they arrive.)
    ///
    /// A stored asset answers [`TextSaveOutcome::NotWritable`] rather than
    /// the write: its bytes live in a content-addressed blob, and overwriting
    /// a blob in place would leave the record's hash pointing at content the
    /// file no longer has. Relinking the original is the honest path.
    ///
    /// On a write the record follows the content exactly as the image edit
    /// does — hash and size recomputed, the old thumbnail removed once
    /// nothing references it, a fresh one generated from the new bytes — and
    /// the row update trips the search trigger, so the indexed body tracks
    /// the file the same way it does on import.
    pub fn save_linked_text(
        &self,
        id: Uuid,
        text: &str,
        encoding: &str,
        bom: bool,
        expected_mtime: std::time::SystemTime,
        expected_size: u64,
    ) -> Result<TextSaveOutcome> {
        let conn = self.store.conn();
        let Some(asset) = assets::get(conn, id)? else {
            return Ok(TextSaveOutcome::NotWritable);
        };
        if asset.placement().is_trashed() {
            return Ok(TextSaveOutcome::NotWritable);
        }
        let AssetLocation::Linked { source_path } = asset.location() else {
            return Ok(TextSaveOutcome::NotWritable);
        };
        let source = PathBuf::from(source_path);
        let Ok(stat) = std::fs::metadata(&source) else {
            // The linked original is gone: relinking is the fix, not a save.
            return Ok(TextSaveOutcome::NotWritable);
        };
        match stat.modified() {
            Ok(mtime) if mtime == expected_mtime && stat.len() == expected_size => {}
            _ => return Ok(TextSaveOutcome::Conflict),
        }

        let bytes = media::text::encode_for_write(text, encoding, bom)?;
        let hash = media::hash::hash_bytes(&bytes);
        if asset
            .content_hash
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(&hash))
        {
            return Ok(TextSaveOutcome::Unchanged);
        }

        // Atomic replace, the same shape the image edit uses: the bytes land
        // on a hidden sibling first and a rename moves them over the
        // original, so a crash mid-write costs at most the previous content,
        // never a truncated file.
        let tmp = source.with_file_name(format!(
            ".{}.trove-text-{}",
            source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file"),
            uuid::Uuid::new_v4().simple()
        ));
        let write = (|| -> Result<()> {
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &source)?;
            Ok(())
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }

        let old_hash = asset.content_hash.clone();
        assets::set_linked_media_columns(conn, asset.id, &hash, bytes.len() as u64, None, None)?;
        if let Some(old_hash) = old_hash.as_deref()
            && assets::count_by_content_hash(conn, old_hash)? == 0
        {
            media::thumb::remove_derived(self.cache(), old_hash);
        }
        media::thumb::regenerate(self.cache(), &hash, asset.kind, &source);
        Ok(TextSaveOutcome::Written)
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
        let old_hash = asset.content_hash.clone();
        let old_rel = match asset.location() {
            AssetLocation::Stored { rel_path } => Some(rel_path),
            _ => None,
        };
        if old_hash
            .as_deref()
            .is_some_and(|old| staged.content_hash.eq_ignore_ascii_case(old))
        {
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
        let old_hash_ref = old_hash.as_deref().unwrap_or("");
        if assets::count_by_content_hash(self.store.conn(), old_hash_ref)? == 0
            && let Some(rel) = &old_rel
        {
            self.remove_blob_file(rel);
            media::thumb::remove_derived(self.cache(), old_hash_ref);
        }

        // Thumbnail and visual fingerprint describe the old pixels; both
        // must follow the content to its new hash.
        media::thumb::regenerate(self.cache(), &staged.content_hash, asset.kind, &new_blob);
        crate::store::visual_search::compute_and_store_signature(&self.store, &self.root, id)?;
        Ok(())
    }
}
