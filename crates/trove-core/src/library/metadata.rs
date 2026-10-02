//! Bulk purge: permanently deleting assets and freeing their blobs.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

impl Library {
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
