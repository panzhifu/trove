//! Catalog metadata: pretty-JSON export and its import, plus bulk purge.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

impl Library {
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
        for raw in file.smart_collections {
            let Ok(sc) = serde_json::from_value::<crate::model::SmartCollection>(raw) else {
                report.skipped += 1;
                continue;
            };
            // A tree this build cannot read (or compile) is a saved search to
            // skip: the rest of the catalog still restores.
            let Some(query) = sc.query.node().cloned() else {
                report.skipped += 1;
                continue;
            };
            let input = NewSmartCollection {
                parent_id: None,
                name: sc.name.clone(),
                query,
                position: sc.position,
            };
            if input.validate().is_ok() && smart::validate(&input.query).is_ok() {
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
                placement: Placement::Live,
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
                    collections::add_asset(
                        conn,
                        crate::model::CollectionId(*c),
                        crate::model::AssetId(*a),
                    )?;
                }
                _ => report.skipped += 1,
            }
        }
        for (old_asset, old_tag) in file.asset_tags {
            match (asset_map.get(&old_asset), tag_map.get(&old_tag)) {
                (Some(a), Some(t)) => {
                    tags::add_to_asset(conn, crate::model::AssetId(*a), crate::model::TagId(*t))?;
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
                if let Some(hash) = hash.as_deref() {
                    if !derived_tx.iter().any(|seen| seen == hash) {
                        derived_tx.push(hash.to_string());
                    }
                    if let Some(rel) = rel
                        && assets::count_by_content_hash(tx, hash)? == 0
                    {
                        freed_tx.push((rel, hash.to_string()));
                    }
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
    pub(super) fn remove_blob_file(&self, rel: &str) {
        if rel.starts_with("media/") {
            let _ = std::fs::remove_file(self.root.join(rel));
        }
    }
}
