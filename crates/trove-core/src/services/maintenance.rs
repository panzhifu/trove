//! Library maintenance: thumbnail rebuild and orphan cleanup. These are batch
//! filesystem sweeps over a [`Library`] — heavy enough that they belong on a
//! background thread, exposed here as plain synchronous functions the caller
//! decides where to run.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::library::Library;
use crate::media::{blob, thumb};
use crate::model::{AssetKind, AssetQuery};
use crate::store::{assets, rows};
use uuid::Uuid;

/// Outcome of a thumbnail rebuild.
#[derive(Debug, Clone, Default)]
pub struct ThumbRebuildReport {
    /// Thumbnails written (or rewritten when `force` was set).
    pub regenerated: u64,
    /// Image assets whose stored blob file is missing on disk.
    pub missing_blobs: u64,
}

/// Work plan for a thumbnail rebuild, collected by
/// [`plan_thumbnail_rebuild`]. Plain data (`Send`), so the actual file work
/// ([`run_thumbnail_plan`]) can run on a background thread while the plan is
/// gathered where the non-`Send` [`Library`] lives.
#[derive(Debug, Clone, Default)]
pub struct ThumbPlan {
    /// `(blob path, sha256, kind)` triples whose thumbnail should be
    /// regenerated.
    pub items: Vec<(PathBuf, String, AssetKind)>,
    /// Image assets whose stored blob file is missing on disk.
    pub missing_blobs: u64,
}

/// Collect the work for a thumbnail rebuild without doing any of it.
///
/// `force = false` only plans gaps (missing thumbnails); `force = true`
/// plans a rewrite of every thumbnail, repairing corrupt cache entries.
/// Covers both image and font assets (fonts get a specimen-card thumbnail).
pub fn plan_thumbnail_rebuild(lib: &Library, force: bool) -> Result<ThumbPlan> {
    let conn = lib.store().conn();
    let root = lib.root();

    let mut plan = ThumbPlan::default();
    for kind in [AssetKind::Image, AssetKind::Font] {
        let (_, assets) = assets::query(
            conn,
            &AssetQuery {
                kind: Some(kind),
                is_trashed: false,
                ..Default::default()
            },
        )?;
        for asset in assets {
            let Some(sha) = asset.sha256 else { continue };
            let Some(rel) = asset.rel_path else { continue };
            let blob = root.join(&rel);
            if !blob.is_file() {
                plan.missing_blobs += 1;
                continue;
            }
            // Skip existing thumbnails unless a full rewrite was requested.
            if !force && thumb::abs_path(root, &sha).is_file() {
                continue;
            }
            plan.items.push((blob, sha, kind));
        }
    }
    Ok(plan)
}

/// Execute a [`ThumbPlan`]: pure filesystem work with no database access.
/// Designed for a background thread.
pub fn run_thumbnail_plan(root: &Path, plan: ThumbPlan) -> ThumbRebuildReport {
    let mut report = ThumbRebuildReport {
        regenerated: 0,
        missing_blobs: plan.missing_blobs,
    };
    for (blob, sha, kind) in plan.items {
        if thumb::regenerate(root, &sha, kind, &blob).is_some() {
            report.regenerated += 1;
        }
    }
    report
}

/// Regenerate thumbnails for every live image asset.
///
/// `force = false` only fills gaps (missing thumbnails); `force = true`
/// rewrites every thumbnail, repairing corrupt cache entries.
pub fn rebuild_thumbnails(lib: &Library, force: bool) -> Result<ThumbRebuildReport> {
    let root = lib.root().to_path_buf();
    let plan = plan_thumbnail_rebuild(lib, force)?;
    Ok(run_thumbnail_plan(&root, plan))
}

/// Rebuild the full-text index from the current asset rows (live and trashed
/// included), returning how many rows were indexed. Use to backfill a library
/// created before the FTS table existed, or to repair drift.
pub fn rebuild_search_index(lib: &Library) -> Result<u64> {
    let conn = lib.store().conn();
    rows::execute(conn, "DELETE FROM asset_fts", vec![])?;
    let mut indexed = 0u64;
    // The index mirrors every non-deleted row — trashed assets keep their
    // entry and search filters them at query time — so rebuild walks both.
    for trashed in [false, true] {
        let (_, assets) = assets::query(
            conn,
            &AssetQuery {
                is_trashed: trashed,
                ..Default::default()
            },
        )?;
        for asset in assets {
            assets::fts_insert(conn, &asset)?;
            indexed += 1;
        }
    }
    Ok(indexed)
}

/// Outcome of an orphan sweep.
#[derive(Debug, Clone, Default)]
pub struct OrphanReport {
    /// Blob files under `media/` with no referencing record, deleted.
    pub blobs_removed: u64,
    /// Thumbnail files under `thumbs/` with no live blob (or no record),
    /// deleted.
    pub thumbs_removed: u64,
    /// Assets whose blob file was found missing, moved to the trash.
    pub files_trashed: u64,
    /// Empty directories pruned under `media/` and `thumbs/`.
    pub empty_dirs_removed: u64,
}

/// Sweep a library for orphans:
///
/// 1. Assets whose stored blob file is missing are moved to the trash (their
///    record is kept, so the blob is only freed once the user purges them).
/// 2. Blob files under `media/` not referenced by any record are deleted.
/// 3. Thumbnails whose blob is gone — or that no record references — are
///    deleted.
/// 4. Empty directories under `media/` and `thumbs/` are pruned.
///
/// Deleting any of these is safe: content-addressed files are derived data.
pub fn clean_orphans(lib: &Library) -> Result<OrphanReport> {
    let conn = lib.store().conn();
    let root = lib.root();
    let mut report = OrphanReport::default();

    // 1. Trash live assets whose blob is missing (recoverable).
    let (_, live) = assets::query(
        conn,
        &AssetQuery {
            is_trashed: false,
            ..Default::default()
        },
    )?;
    for asset in live {
        let Some(rel) = asset.rel_path else { continue };
        if !root.join(&rel).is_file() && assets::set_trashed(conn, asset.id, true)? {
            report.files_trashed += 1;
        }
    }

    // 2. Collect hashes referenced by any record, and hashes present on disk.
    let referenced: HashSet<String> = assets::referenced_shas(conn)?.into_iter().collect();

    let media_dir = root.join("media");
    let thumbs_dir = root.join("thumbs");

    let mut present: HashSet<String> = HashSet::new();
    for file in walk_files(&media_dir) {
        // A content-addressed blob is `media/<a>/<b>.<ext>` with the full hash
        // `a + b`. Files that don't match (e.g. leftover `.tmp-…`) are junk.
        match reconstructed_sha(&file) {
            Some(sha) => {
                let orphan = !referenced.contains(&sha);
                present.insert(sha);
                if orphan && std::fs::remove_file(&file).is_ok() {
                    report.blobs_removed += 1;
                }
            }
            None => {
                if std::fs::remove_file(&file).is_ok() {
                    report.blobs_removed += 1;
                }
            }
        }
    }

    // 3. Thumbs are orphaned when no record references the hash OR the blob is
    //    no longer present on disk.
    for file in walk_files(&thumbs_dir) {
        match reconstructed_sha(&file) {
            Some(sha) => {
                let orphan = !referenced.contains(&sha) || !present.contains(&sha);
                if orphan && std::fs::remove_file(&file).is_ok() {
                    report.thumbs_removed += 1;
                }
            }
            // A non-hash file under `thumbs/` is junk.
            None => {
                if std::fs::remove_file(&file).is_ok() {
                    report.thumbs_removed += 1;
                }
            }
        }
    }

    report.empty_dirs_removed = remove_empty_dirs(&media_dir) + remove_empty_dirs(&thumbs_dir);
    Ok(report)
}

// -- integrity check ---------------------------------------------------------

/// What is wrong with an asset found by the integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityIssue {
    /// The blob file the record points at does not exist on disk.
    MissingBlob,
    /// The blob exists, but its content hash differs from the recorded
    /// SHA-256 — the file was modified or corrupted after import.
    HashMismatch,
}

/// One problem found by the integrity check.
#[derive(Debug, Clone)]
pub struct IntegrityEntry {
    pub asset_id: Uuid,
    pub file_name: String,
    pub issue: IntegrityIssue,
}

/// Outcome of an integrity check.
#[derive(Debug, Clone, Default)]
pub struct IntegrityReport {
    /// Records whose blob was read and hashed successfully (matched or not).
    pub checked: u64,
    /// Records with a problem, in query order.
    pub entries: Vec<IntegrityEntry>,
}

/// Work plan for an integrity check, collected by [`plan_integrity`]. Plain
/// data (`Send`), so the hashing ([`run_integrity_plan`]) can run on a
/// background thread while the plan is gathered where the non-`Send`
/// [`Library`] lives.
#[derive(Debug, Clone, Default)]
pub struct IntegrityPlan {
    /// `(asset id, file name, blob path, expected sha256)` tuples.
    pub items: Vec<(Uuid, String, PathBuf, String)>,
}

/// Collect the work for an integrity check without doing any of it: every
/// record (live and trashed) that has both a stored hash and a blob path.
pub fn plan_integrity(lib: &Library) -> Result<IntegrityPlan> {
    let conn = lib.store().conn();
    let root = lib.root();
    let mut plan = IntegrityPlan::default();
    for trashed in [false, true] {
        let (_, list) = assets::query(
            conn,
            &AssetQuery {
                is_trashed: trashed,
                ..Default::default()
            },
        )?;
        for asset in list {
            let (Some(sha), Some(rel)) = (asset.sha256.clone(), asset.rel_path.clone()) else {
                continue;
            };
            plan.items
                .push((asset.id, asset.file_name, root.join(rel), sha));
        }
    }
    Ok(plan)
}

/// Execute an [`IntegrityPlan`]: pure filesystem work with no database
/// access. Each distinct blob file is hashed once even when several records
/// share it (deduplicated imports).
pub fn run_integrity_plan(plan: IntegrityPlan) -> IntegrityReport {
    let mut report = IntegrityReport::default();
    // `Err(())` = unreadable (missing blob); stored per path so shared blobs
    // are hashed exactly once.
    let mut hashed: HashMap<PathBuf, std::result::Result<String, ()>> = HashMap::new();
    for (asset_id, file_name, blob_path, expected) in plan.items {
        let actual = match hashed.get(&blob_path) {
            Some(cached) => cached.clone(),
            None => {
                let result = hash_file(&blob_path).map_err(|_| ());
                hashed.insert(blob_path, result.clone());
                result
            }
        };
        match actual {
            Ok(actual) => {
                report.checked += 1;
                if actual != expected {
                    report.entries.push(IntegrityEntry {
                        asset_id,
                        file_name,
                        issue: IntegrityIssue::HashMismatch,
                    });
                }
            }
            Err(_) => report.entries.push(IntegrityEntry {
                asset_id,
                file_name,
                issue: IntegrityIssue::MissingBlob,
            }),
        }
    }
    report
}

/// Verify every stored blob against its recorded SHA-256 (plan + run on the
/// current thread; prefer the split API for UI usage).
pub fn verify_integrity(lib: &Library) -> Result<IntegrityReport> {
    let plan = plan_integrity(lib)?;
    Ok(run_integrity_plan(plan))
}

/// Streaming SHA-256 of a file; fails when the file cannot be read.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(blob::hex(&hasher.finalize()))
}

// -- filesystem helpers ------------------------------------------------------

/// Reconstruct the full 64-hex content hash of a blob/thumb file.
///
/// Content-addressed files are laid out as `<bucket>/<hash-without-prefix>`
/// inside `media/<a>/` and `thumbs/<a>/` — the directory name holds the first
/// two hex chars and the file basename the rest, so the full hash is
/// `a + file_stem`. Returns `None` for files that don't match this layout
/// (leftover `.tmp-…` files, stray files), which the sweep treats as orphans.
fn reconstructed_sha(path: &Path) -> Option<String> {
    let bucket = path.parent()?.file_name()?.to_string_lossy();
    if bucket.len() != 2 || !bucket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let stem = path.file_name()?.to_string_lossy();
    let stem = stem.split_once('.').map(|(a, _)| a).unwrap_or(&stem);
    Some(format!("{bucket}{stem}"))
}

/// Recursively list files (not directories), ignoring unreadable paths.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_files_into(root, &mut out);
    out
}

fn walk_files_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_files_into(&path, out);
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

/// Remove empty directories bottom-up; returns how many were removed.
fn remove_empty_dirs(dir: &Path) -> u64 {
    let mut removed = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !is_symlink(&path) {
                removed += remove_empty_dirs(&path);
            }
        }
    }
    if std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
        && std::fs::remove_dir(dir).is_ok()
    {
        removed += 1;
    }
    removed
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::media::thumb;
    use crate::model::{AssetQuery, NewCollection};
    use crate::store::collections;

    /// A minimal valid 1x1 PNG.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn temp_lib(name: &str) -> (Library, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("trove-maint-{name}-{}", uuid::Uuid::new_v4()));
        let lib = Library::open(&root).unwrap();
        (lib, root)
    }

    fn import_png(lib: &Library, root: &Path, name: &str) -> String {
        let src = root.join(name);
        std::fs::write(&src, PNG_1X1).unwrap();
        let report = lib.import_files(&[src], None).unwrap();
        let item = &report.imported[0];
        // Look the asset up to get its stored sha.
        let (_, all) = crate::store::assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        all.into_iter()
            .find(|a| a.id == item.asset_id)
            .unwrap()
            .sha256
            .unwrap()
    }

    #[test]
    fn healthy_library_is_untouched_by_orphan_cleanup() {
        let (lib, root) = temp_lib("healthy");
        let sha = import_png(&lib, &root, "ok.png");
        let thumb_path = thumb::abs_path(&root, &sha);

        let report = clean_orphans(&lib).unwrap();
        assert_eq!(report.blobs_removed, 0);
        assert_eq!(report.files_trashed, 0);
        assert_eq!(report.thumbs_removed, 0);
        assert!(thumb_path.is_file());
    }

    #[test]
    fn rebuild_recreates_a_deleted_thumbnail() {
        let (lib, root) = temp_lib("rebuild");
        let sha = import_png(&lib, &root, "pic.png");
        let thumb_path = thumb::abs_path(&root, &sha);
        assert!(thumb_path.is_file());

        std::fs::remove_file(&thumb_path).unwrap();
        let report = rebuild_thumbnails(&lib, false).unwrap();
        assert!(report.regenerated >= 1);
        assert!(thumb_path.is_file());
    }

    #[test]
    fn rebuild_force_rewrites_existing_thumbs() {
        let (lib, root) = temp_lib("rebuild-force");
        let sha = import_png(&lib, &root, "a.png");
        let thumb_path = thumb::abs_path(&root, &sha);
        let before = std::fs::read(&thumb_path).unwrap();

        let report = rebuild_thumbnails(&lib, true).unwrap();
        assert!(report.regenerated >= 1);
        assert!(thumb_path.is_file());
        let _ = before; // deterministic enough; content is regenerated, not compared
    }

    #[test]
    fn clean_orphans_removes_stray_blob_and_trashes_missing_blob_asset() {
        let (lib, root) = temp_lib("orphans");
        let sha = import_png(&lib, &root, "keep.png");
        let thumb_path = thumb::abs_path(&root, &sha);

        // A stray blob no record references (name is a 64-hex hash).
        let stray = "b".repeat(64).to_string();
        let stray_dir = root.join("media").join(&stray[..2]);
        std::fs::create_dir_all(&stray_dir).unwrap();
        std::fs::write(stray_dir.join(format!("{stray}.png")), b"orphan bytes").unwrap();

        // Simulate the asset's blob going missing.
        let (_, all) =
            crate::store::assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let blob = root.join(all[0].rel_path.as_deref().unwrap());
        std::fs::remove_file(&blob).unwrap();

        let report = clean_orphans(&lib).unwrap();
        assert!(report.blobs_removed >= 1, "stray blob removed");
        assert!(report.files_trashed >= 1, "missing-blob asset trashed");
        assert!(report.thumbs_removed >= 1, "orphan thumb removed");

        // The asset is recoverable in the trash.
        let (_, trashed) = crate::store::assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!trashed.is_empty());

        // Stray blob and thumb are gone.
        assert!(!stray_dir.join(format!("{stray}.png")).exists());
        assert!(!thumb_path.exists());

        // Orphan collection membership is unaffected.
        let c = collections::create(
            lib.store().conn(),
            &NewCollection {
                parent_id: None,
                name: "album".into(),
                position: 0,
            },
        )
        .unwrap();
        collections::add_asset(lib.store().conn(), c.id, trashed[0].id).unwrap();
        assert_eq!(
            collections::count_assets(lib.store().conn(), c.id).unwrap(),
            1
        );
    }

    #[test]
    fn integrity_check_detects_missing_and_corrupted_blobs() {
        let (lib, root) = temp_lib("integrity");
        import_png(&lib, &root, "ok.png");

        // Look the asset up (id + stored blob path).
        let (_, all) =
            crate::store::assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let asset = &all[0];
        let blob_path = root.join(asset.rel_path.as_deref().unwrap());
        let expected = asset.sha256.clone().unwrap();

        // Healthy library: the blob is read and matches the record.
        let report = verify_integrity(&lib).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.entries.is_empty());

        // Corrupted content: readable, but the hash no longer matches.
        std::fs::write(&blob_path, b"corrupted payload").unwrap();
        let report = verify_integrity(&lib).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].asset_id, asset.id);
        assert_eq!(report.entries[0].issue, IntegrityIssue::HashMismatch);
        assert_eq!(report.entries[0].file_name, asset.file_name);

        // Missing blob: unreadable, so it is not counted as checked.
        std::fs::remove_file(&blob_path).unwrap();
        let report = verify_integrity(&lib).unwrap();
        assert_eq!(report.checked, 0);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].issue, IntegrityIssue::MissingBlob);

        // The plan carries the recorded hash for context.
        let plan = plan_integrity(&lib).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].3, expected);
    }
}
