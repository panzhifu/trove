//! Library maintenance: thumbnail rebuild, metadata re-mining and orphan
//! cleanup. These are batch filesystem sweeps over a [`Library`] — heavy enough
//! that they belong on a background thread, exposed here as plain synchronous
//! functions the caller decides where to run.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::library::Library;
use crate::media::thumb;
use crate::model::{AssetKind, AssetQuery};
use crate::store::assets;
use uuid::Uuid;

/// Outcome of re-mining embedded metadata on existing assets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemineReport {
    /// Assets whose facts changed and were written back.
    pub updated: u64,
    /// Assets whose file was read, whether or not anything changed.
    pub scanned: u64,
    /// Assets of a re-minable kind whose file could not be located.
    pub missing_files: u64,
}

/// Whether an asset already carries what re-mining would produce, so an
/// unforced pass can leave it alone.
///
/// A file with genuinely no tags looks identical to one that was never mined, so
/// those stay in every sweep and get re-read. Repeating a header read is the
/// honest cost: a "backfilled" marker would have to claim knowledge the facts
/// themselves do not carry.
fn remine_complete(asset: &crate::model::Asset, kind: AssetKind) -> bool {
    match kind {
        AssetKind::Audio => {
            asset.facts.audio.sample_rate.is_some() || asset.facts.audio.channels.is_some()
        }
        AssetKind::Font => asset.facts.font.family.is_some(),
        _ => true,
    }
}

/// Re-read the metadata the import pipeline mines, for assets already in the
/// library.
///
/// ## Why this exists
///
/// `metadata::mine` runs only inside the pipeline, so every field added since a
/// library was imported is simply absent from existing rows — for audio that is
/// the sample rate, channel count, bit depth and bitrate, and for anything
/// imported before a tag landed, the artist and album beside them. Without a way
/// back, the only fix is "delete and re-import", which costs the user their
/// tags, collections and ratings.
///
/// ## Audio and fonts only, on purpose
///
/// Images are excluded because `mine_image` also produces the dominant palette,
/// which lives in the same [`crate::model::AssetFacts`] as the visual signature
/// the pipeline's own stages compute. Getting that boundary right needs a rule
/// per key rather than per group, and the payoff is smaller: image dimensions and
/// capture time are already stored, and the search fingerprint has its own
/// backfill in Settings. Adding image re-mining later should be its own change
/// with that rule written down, not a boolean flipped here.
///
/// ## What it will not overwrite
///
/// `mined.facts` holds only what the file itself says. The stored facts also
/// carry the visual signature and palette, the AI tags and analysis markers, and
/// the `unknown` keys nothing else claims. Replacing the whole map from a
/// re-mine would delete AI output and every asset's search fingerprint — a large,
/// silent data loss behind a maintenance button. So each kind copies across
/// exactly the groups re-mining is authoritative for and preserves the rest.
///
/// The asset's `title` is left alone too: import seeds it from an embedded tag,
/// but the user may have renamed the asset since. Duration needs no touch here
/// either — `mine_audio` has returned it since before this function existed, so
/// stored rows already carry it.
///
/// ## Cost
///
/// One header read per asset, no decoders and no subprocesses: lofty and
/// ttf-parser both stop at the tag tables. It is the rare maintenance job cheap
/// enough to offer without a warning.
/// The work a re-mine would do, collected where the non-`Send` [`Library`]
/// lives so the read/write itself can run on a background thread.
#[derive(Debug, Clone, Default)]
pub struct ReminePlan {
    /// `(asset id, blob path, kind)` triples to re-read.
    pub items: Vec<(Uuid, PathBuf, AssetKind)>,
    /// Live assets of a re-minable kind whose file could not be located.
    pub missing_files: u64,
}

/// Collect the work for a re-mine without doing any of it.
pub fn plan_remine(lib: &Library, force: bool) -> Result<ReminePlan> {
    let conn = lib.store().conn();
    let mut plan = ReminePlan::default();
    for kind in [AssetKind::Audio, AssetKind::Font] {
        let page = assets::query(
            conn,
            &AssetQuery {
                kind: Some(kind),
                is_trashed: false,
                ..Default::default()
            },
        )?;
        for asset in page.items {
            let Some(path) = lib.asset_file(asset.id) else {
                plan.missing_files += 1;
                continue;
            };
            if !force && remine_complete(&asset, kind) {
                continue;
            }
            plan.items.push((asset.id, path, kind));
        }
    }
    Ok(plan)
}

/// Execute a [`ReminePlan`]. Opens its own connection so it can run on a
/// background thread; `db_path` is the library's `library.db`.
pub fn run_remine_plan(db_path: &Path, plan: ReminePlan) -> RemineReport {
    let store = match crate::store::Store::open(db_path) {
        Ok(store) => store,
        Err(_) => return RemineReport::default(),
    };
    let conn = store.conn();
    let mut report = RemineReport {
        missing_files: plan.missing_files,
        ..Default::default()
    };
    for (id, path, kind) in plan.items {
        let Some(asset) = assets::get(conn, id).ok().flatten() else {
            continue;
        };
        report.scanned += 1;
        let mined = crate::media::metadata::mine(&path, kind, &path);
        let mut facts = asset.facts.clone();
        match kind {
            // Lofty: the tag group, plus the stream properties read beside them.
            AssetKind::Audio => {
                facts.media = mined.facts.media;
                facts.audio = mined.facts.audio;
            }
            // ttf-parser: family, style, weight, glyph count.
            AssetKind::Font => facts.font = mined.facts.font,
            _ => {}
        }
        if facts == asset.facts {
            continue;
        }
        let patch = crate::model::AssetPatch {
            facts: Some(facts),
            ..Default::default()
        };
        if assets::update(conn, id, &patch).is_ok_and(|updated| updated.is_some()) {
            report.updated += 1;
        }
    }
    report
}

/// Outcome of a thumbnail rebuild.
#[derive(Debug, Clone, Default)]
pub struct ThumbRebuildReport {
    /// Thumbnails written (or rewritten when `force` was set).
    pub regenerated: u64,
    /// Assets in the covered kinds whose media file is missing on disk.
    pub missing_blobs: u64,
}

/// Work plan for a thumbnail rebuild, collected by
/// [`plan_thumbnail_rebuild`]. Plain data (`Send`), so the actual file work
/// ([`run_thumbnail_plan`]) can run on a background thread while the plan is
/// gathered where the non-`Send` [`Library`] lives.
#[derive(Debug, Clone, Default)]
pub struct ThumbPlan {
    /// `(blob path, content_hash, kind)` triples whose thumbnail should be
    /// regenerated.
    pub items: Vec<(PathBuf, String, AssetKind)>,
    /// Assets in the covered kinds whose media file is missing on disk.
    pub missing_blobs: u64,
}

/// Collect the work for a thumbnail rebuild without doing any of it.
///
/// `force = false` only plans gaps (missing thumbnails); `force = true`
/// plans a rewrite of every thumbnail, repairing corrupt cache entries.
/// Covers image, font, model and audio assets (fonts get a specimen card,
/// audio files their cover art or waveform).
///
/// Linked assets are in scope: their blob is the file they were imported from,
/// not a copy under `media/`, so resolving it is `thumb::blob_path`'s job rather
/// than a `rel_path` read. Video is deliberately absent — a poster frame costs
/// an ffmpeg pass, and a library of them makes this job a long one. Text is
/// present, and cheap: a card is its own lines rasterized, with no subprocess.
pub fn plan_thumbnail_rebuild(lib: &Library, force: bool) -> Result<ThumbPlan> {
    let conn = lib.store().conn();
    let root = lib.root();
    let cache = lib.cache();

    let mut plan = ThumbPlan::default();
    for kind in [
        AssetKind::Image,
        AssetKind::Font,
        AssetKind::Model,
        AssetKind::Audio,
        AssetKind::Document,
        AssetKind::Other,
    ] {
        let assets = assets::query(
            conn,
            &AssetQuery {
                kind: Some(kind),
                is_trashed: false,
                ..Default::default()
            },
        )?;
        for asset in assets.items {
            // `Document` and `Other` are only in scope for their text members:
            // a PDF or an archive has no card to rebuild, and counting them
            // would promise work that does not exist. Text straddles the two
            // kinds because `.txt` was a document before this app could read it
            // and `.rs` never was one.
            if matches!(kind, AssetKind::Document | AssetKind::Other)
                && !crate::media::text::is_text_ext(&asset.ext)
            {
                continue;
            }
            let Some(sha) = asset.content_hash.clone() else {
                continue;
            };
            let Some(blob) = thumb::blob_path(root, &asset).filter(|p| p.is_file()) else {
                plan.missing_blobs += 1;
                continue;
            };
            // Skip existing thumbnails unless a full rewrite was requested.
            if !force && thumb::abs_path(cache, &sha).is_file() {
                continue;
            }
            plan.items.push((blob, sha, kind));
        }
    }
    Ok(plan)
}

/// Execute a [`ThumbPlan`]: pure filesystem work with no database access.
/// `cache_root` is the library's cache directory — where thumbnails live, not
/// where the blobs do. Designed for a background thread.
pub fn run_thumbnail_plan(cache_root: &Path, plan: ThumbPlan) -> ThumbRebuildReport {
    let mut report = ThumbRebuildReport {
        regenerated: 0,
        missing_blobs: plan.missing_blobs,
    };
    for (blob, sha, kind) in plan.items {
        if thumb::regenerate(cache_root, &sha, kind, &blob).is_some() {
            report.regenerated += 1;
        }
    }
    report
}

/// Regenerate thumbnails for every live asset of a covered kind.
///
/// `force = false` only fills gaps (missing thumbnails); `force = true`
/// rewrites every thumbnail, repairing corrupt cache entries.
pub fn rebuild_thumbnails(lib: &Library, force: bool) -> Result<ThumbRebuildReport> {
    let cache = lib.cache().to_path_buf();
    let plan = plan_thumbnail_rebuild(lib, force)?;
    Ok(run_thumbnail_plan(&cache, plan))
}

/// Rebuild the Tantivy text index from the current asset rows (live and
/// trashed included), returning how many documents were indexed. Use to
/// repair drift or to recreate a lost index directory.
pub fn rebuild_search_index(lib: &Library) -> Result<u64> {
    lib.rebuild_text_index()
}

/// Outcome of an orphan sweep.
#[derive(Debug, Clone, Default)]
pub struct OrphanReport {
    /// Blob files under `media/` with no referencing record, deleted.
    pub blobs_removed: u64,
    /// Thumbnail files under `thumbs/` no record references (or whose stored
    /// blob is gone), deleted.
    pub thumbs_removed: u64,
    /// Waveform envelopes under `waveforms/` no record is alive for, deleted.
    pub waves_removed: u64,
    /// Contact sheets under `contact-sheet/` no record is alive for, deleted.
    pub sheets_removed: u64,
    /// Assets whose blob file was found missing, moved to the trash.
    pub files_trashed: u64,
    /// Empty directories pruned under `media/`, `thumbs/`, `waveforms/` and
    /// `contact-sheet/`.
    pub empty_dirs_removed: u64,
}

/// Sweep a library for orphans:
///
/// 1. Assets whose stored blob file is missing are moved to the trash (their
///    record is kept, so the blob is only freed once the user purges them).
///    Linked assets are skipped: they have no blob, and their originals belong
///    to the user — a missing one is reported by the integrity check, never
///    acted on here.
/// 2. Blob files under `media/` not referenced by any record are deleted.
/// 3. Thumbnails under the cache root whose blob is gone — or that no record
///    references — are deleted.
/// 4. Waveform envelopes, by the same rule as thumbnails.
/// 5. Empty directories under `media/`, `thumbs/`, `waveforms/` and
///    `contact-sheet/` are pruned.
///
/// Deleting any of these is safe: content-addressed files are derived data.
pub fn clean_orphans(lib: &Library) -> Result<OrphanReport> {
    let conn = lib.store().conn();
    let root = lib.root();
    let mut report = OrphanReport::default();

    // 1. Trash live assets whose blob is missing (recoverable).
    let live = assets::query(
        conn,
        &AssetQuery {
            is_trashed: false,
            ..Default::default()
        },
    )?;
    for asset in live.items {
        let Some(rel) = asset.rel_path else { continue };
        if !root.join(&rel).is_file() && assets::set_trashed(conn, asset.id, true)? {
            report.files_trashed += 1;
        }
    }

    // 2. Collect hashes referenced by any record, and hashes present on disk.
    let referenced: HashSet<String> = assets::referenced_hashes(conn)?.into_iter().collect();
    // A linked asset's file is wherever it was imported from, never under
    // `media/`, so that directory cannot say its derived files are orphaned —
    // only the absence of a record can.
    let linked: HashSet<String> = assets::linked_hashes(conn)?.into_iter().collect();

    let media_dir = root.join("media");
    let thumbs_dir = lib.cache().join("thumbs");
    let waves_dir = lib.cache().join("waveforms");
    let sheets_dir = lib.cache().join("contact-sheet");

    let mut present: HashSet<String> = HashSet::new();
    for file in walk_files(&media_dir) {
        // A content-addressed blob is `media/<a>/<b>.<ext>` with the full hash
        // `a + b`. Files that don't match (e.g. leftover `.tmp-…`) are junk.
        match reconstructed_hash(&file) {
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

    // 3+4. Derived files are orphaned when no record references the hash, or
    //      when a *stored* asset's blob has left the library.
    let orphaned =
        |sha: &str| !referenced.contains(sha) || (!linked.contains(sha) && !present.contains(sha));
    for (dir, counter) in [
        (&thumbs_dir, &mut report.thumbs_removed),
        (&waves_dir, &mut report.waves_removed),
        (&sheets_dir, &mut report.sheets_removed),
    ] {
        for file in walk_files(dir) {
            let orphan = match reconstructed_hash(&file) {
                // A non-hash file in a derived cache is junk.
                None => true,
                Some(sha) => orphaned(&sha),
            };
            if orphan && std::fs::remove_file(&file).is_ok() {
                *counter += 1;
            }
        }
    }

    report.empty_dirs_removed = remove_empty_dirs(&media_dir)
        + remove_empty_dirs(&thumbs_dir)
        + remove_empty_dirs(&waves_dir)
        + remove_empty_dirs(&sheets_dir);
    Ok(report)
}

// -- integrity check ---------------------------------------------------------

/// What is wrong with an asset found by the integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityIssue {
    /// The blob file the record points at does not exist on disk.
    MissingBlob,
    /// The blob exists, but its content hash differs from the recorded
    /// BLAKE3 content hash — the file was modified or corrupted after import.
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
    /// `(asset id, file name, blob path, expected content_hash)` tuples.
    pub items: Vec<(Uuid, String, PathBuf, String)>,
}

/// Collect the work for an integrity check without doing any of it: every
/// record (live and trashed) that has both a stored hash and a blob path.
pub fn plan_integrity(lib: &Library) -> Result<IntegrityPlan> {
    let conn = lib.store().conn();
    let root = lib.root();
    let mut plan = IntegrityPlan::default();
    for trashed in [false, true] {
        let list = assets::query(
            conn,
            &AssetQuery {
                is_trashed: trashed,
                ..Default::default()
            },
        )?;
        for asset in list.items {
            let (Some(sha), Some(rel)) = (asset.content_hash.clone(), asset.rel_path.clone())
            else {
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

/// Verify every stored blob against its recorded content hash (plan + run on
/// the
/// current thread; prefer the split API for UI usage).
pub fn verify_integrity(lib: &Library) -> Result<IntegrityReport> {
    let plan = plan_integrity(lib)?;
    Ok(run_integrity_plan(plan))
}

/// Streaming content hash of a file; fails when the file cannot be read.
///
/// One line on purpose: the integrity check and the importer have to agree
/// about what "the hash of this file" is, and the only way to guarantee that
/// is for both to ask [`crate::media::hash`]. Its own loop would be a second
/// definition, free to drift, and it would also miss the parallel path the
/// shared one takes for large blobs.
fn hash_file(path: &Path) -> std::io::Result<String> {
    Ok(crate::media::hash::hash_file(path)?.0)
}

// -- filesystem helpers ------------------------------------------------------

/// Reconstruct the full 64-hex content hash of a blob/thumb file.
///
/// Content-addressed files are laid out as `<bucket>/<hash-without-prefix>`
/// inside `media/<a>/` and `thumbs/<a>/` — the directory name holds the first
/// two hex chars and the file basename the rest, so the full hash is
/// `a + file_stem`. Returns `None` for files that don't match this layout
/// (leftover `.tmp-…` files, stray files), which the sweep treats as orphans.
fn reconstructed_hash(path: &Path) -> Option<String> {
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
        let lib = Library::open(&root, root.join("cache")).unwrap();
        (lib, root)
    }

    fn import_png(lib: &Library, root: &Path, name: &str) -> String {
        let src = root.join(name);
        std::fs::write(&src, PNG_1X1).unwrap();
        let report = lib.import_into_store(&[src], None).unwrap();
        let item = &report.imported[0];
        // Look the asset up to get its stored sha.
        let all = crate::store::assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: false,
                ..Default::default()
            },
        )
        .unwrap();
        all.items
            .into_iter()
            .find(|a| a.id == item.asset_id)
            .unwrap()
            .content_hash
            .unwrap()
    }

    #[test]
    fn healthy_library_is_untouched_by_orphan_cleanup() {
        let (lib, root) = temp_lib("healthy");
        let sha = import_png(&lib, &root, "ok.png");
        let thumb_path = thumb::abs_path(lib.cache(), &sha);

        let report = clean_orphans(&lib).unwrap();
        assert_eq!(report.blobs_removed, 0);
        assert_eq!(report.files_trashed, 0);
        assert_eq!(report.thumbs_removed, 0);
        assert!(thumb_path.is_file());
    }

    /// A linked asset's file is not under `media/`, so that directory cannot
    /// speak for its derived cache: only the absence of a record can. Before
    /// that distinction existed the sweep deleted the thumbnail of every linked
    /// asset it passed, on every run.
    #[test]
    fn a_linked_asset_keeps_its_thumbnail_through_the_sweep() {
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

        let (lib, root) = temp_lib("linked-sweep");
        let src = root.join("shot.png");
        std::fs::write(&src, PNG_1X1).unwrap();
        let staged = stage_all(
            &root,
            lib.cache(),
            std::slice::from_ref(&src),
            ImportStorage::Link,
            &std::sync::atomic::AtomicBool::new(false),
        );
        let report = commit_staged_all(lib.store().conn(), None, staged);
        assert_eq!(report.imported_count(), 1);
        let asset = crate::store::assets::get(lib.store().conn(), report.imported[0].asset_id)
            .unwrap()
            .unwrap();
        let thumb_path = thumb::abs_path(lib.cache(), asset.content_hash.as_deref().unwrap());
        assert!(thumb_path.is_file(), "a linked import gets a thumbnail");

        let report = clean_orphans(&lib).unwrap();
        assert_eq!(report.thumbs_removed, 0);
        assert_eq!(report.files_trashed, 0);
        assert!(thumb_path.is_file(), "and keeps it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Envelopes are cache as much as thumbnails are: they go with the record
    /// that needed them and stay while it lives.
    #[test]
    fn an_orphan_envelope_is_swept_and_a_live_one_is_kept() {
        use crate::media::waveform;

        let (lib, root) = temp_lib("wave-sweep");
        let sha = import_png(&lib, &root, "ok.png");
        let peaks = vec![9u8; waveform::PEAK_COUNT];
        waveform::store(lib.cache(), &sha, &peaks);
        let ghost = "9".repeat(64);
        waveform::store(lib.cache(), &ghost, &peaks);
        assert!(waveform::cached(lib.cache(), &sha).is_some());

        let report = clean_orphans(&lib).unwrap();
        assert_eq!(report.waves_removed, 1, "only the unreferenced envelope");
        assert!(
            waveform::cached(lib.cache(), &sha).is_some(),
            "a live asset keeps its envelope"
        );
        assert!(waveform::cached(lib.cache(), &ghost).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rebuild plan is where a track with no cover art gets its waveform
    /// card, so it has to reach audio — and reach it when the file was linked
    /// rather than copied in.
    #[test]
    fn the_thumbnail_plan_covers_audio_including_linked_files() {
        use crate::media::import::{ImportStorage, commit_staged_all, stage_all};

        let (lib, root) = temp_lib("plan-audio");
        // Two different byte strings: identical content would deduplicate into
        // one hash, and the plan would have one card to draw, not two.
        let stored = root.join("stored.mp3");
        std::fs::write(&stored, [PNG_1X1, &[1]].concat()).unwrap();
        lib.import_into_store(std::slice::from_ref(&stored), None)
            .unwrap();
        let linked = root.join("linked.mp3");
        std::fs::write(&linked, [PNG_1X1, &[2]].concat()).unwrap();
        let staged = stage_all(
            &root,
            lib.cache(),
            std::slice::from_ref(&linked),
            ImportStorage::Link,
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert_eq!(
            commit_staged_all(lib.store().conn(), None, staged).imported_count(),
            1
        );

        let planned = |plan: &ThumbPlan| {
            plan.items
                .iter()
                .filter(|(_, _, kind)| *kind == AssetKind::Audio)
                .count()
        };
        let gaps = plan_thumbnail_rebuild(&lib, false).unwrap();
        assert_eq!(
            planned(&gaps),
            2,
            "both tracks need a card and neither has one"
        );
        assert!(
            gaps.items.iter().any(|(blob, _, _)| blob == &linked),
            "the linked file is planned from where it lives"
        );
        // A track whose card exists is left alone until a full rewrite. (The
        // cards are written as bytes, not drawn: a fake `.mp3` has no envelope
        // to decode, and this test is about what gets planned.)
        for (_, sha, _) in &gaps.items {
            let path = thumb::abs_path(lib.cache(), sha);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"a card of some sort").unwrap();
        }
        let rewritten = plan_thumbnail_rebuild(&lib, false).unwrap();
        assert_eq!(planned(&rewritten), 0, "a drawn card is not a gap");
        assert_eq!(planned(&plan_thumbnail_rebuild(&lib, true).unwrap()), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuild_recreates_a_deleted_thumbnail() {
        let (lib, root) = temp_lib("rebuild");
        let sha = import_png(&lib, &root, "pic.png");
        let thumb_path = thumb::abs_path(lib.cache(), &sha);
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
        let thumb_path = thumb::abs_path(lib.cache(), &sha);
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
        let thumb_path = thumb::abs_path(lib.cache(), &sha);

        // A stray blob no record references (name is a 64-hex hash).
        let stray = "b".repeat(64).to_string();
        let stray_dir = root.join("media").join(&stray[..2]);
        std::fs::create_dir_all(&stray_dir).unwrap();
        std::fs::write(stray_dir.join(format!("{stray}.png")), b"orphan bytes").unwrap();

        // Simulate the asset's blob going missing.
        let all = crate::store::assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let blob = root.join(all.items[0].rel_path.as_deref().unwrap());
        std::fs::remove_file(&blob).unwrap();

        let report = clean_orphans(&lib).unwrap();
        assert!(report.blobs_removed >= 1, "stray blob removed");
        assert!(report.files_trashed >= 1, "missing-blob asset trashed");
        assert!(report.thumbs_removed >= 1, "orphan thumb removed");

        // The asset is recoverable in the trash.
        let trashed = crate::store::assets::query(
            lib.store().conn(),
            &AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!trashed.items.is_empty());

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
        collections::add_asset(lib.store().conn(), c.id, trashed.items[0].id).unwrap();
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
        let all = crate::store::assets::query(lib.store().conn(), &AssetQuery::default()).unwrap();
        let asset = &all.items[0];
        let blob_path = root.join(asset.rel_path.as_deref().unwrap());
        let expected = asset.content_hash.clone().unwrap();

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

    /// Re-mining must not destroy what it did not produce.
    ///
    /// The stored facts carry the visual signature, the AI markers and the
    /// `unknown` passthrough keys alongside the mined tags. `mine` returns
    /// those groups empty, so a whole-map replacement would silently delete an
    /// asset's search fingerprint and any AI output — this pins the merge that
    /// keeps them. The file here is deliberately unreadable as audio, which is
    /// the worst case: the re-mine contributes nothing at all.
    #[test]
    fn remine_preserves_everything_it_did_not_mine() {
        let (lib, root) = temp_lib("remine");
        let src = root.join("song.mp3");
        std::fs::write(&src, b"not really an mp3").unwrap();
        lib.import_into_store(&[src], None).unwrap();

        let conn = lib.store().conn();
        let asset = crate::store::assets::query(
            conn,
            &AssetQuery {
                kind: Some(AssetKind::Audio),
                ..Default::default()
            },
        )
        .unwrap()
        .items
        .pop()
        .expect("the audio asset imported");

        // Facts shaped like a row that has been through the full pipeline:
        // a palette, a signature, an AI marker and a key nothing claims.
        let mut facts = asset.facts.clone();
        facts.visual.dominant_color = Some("#112233".into());
        facts.visual.dominant_colors = Some(vec!["#112233".into(), "#445566".into()]);
        facts
            .unknown
            .insert("ai_tags".into(), serde_json::json!(["cat", "dusk"]));
        facts
            .unknown
            .insert("legacy_note".into(), serde_json::json!("keep me"));
        crate::store::assets::update(
            conn,
            asset.id,
            &crate::model::AssetPatch {
                facts: Some(facts),
                ..Default::default()
            },
        )
        .unwrap();

        // `force` because nothing about this row looks mined yet, and the
        // unforced pass is allowed to skip only rows that already have a value.
        let plan = plan_remine(&lib, true).unwrap();
        let report = run_remine_plan(&root.join("library.db"), plan);
        assert_eq!(report.scanned, 1, "the audio asset was read");

        let after = crate::store::assets::get(conn, asset.id).unwrap().unwrap();
        assert_eq!(
            after.facts.visual.dominant_color,
            Some("#112233".into()),
            "the palette survived a re-mine"
        );
        assert_eq!(
            after.facts.visual.dominant_colors.as_deref(),
            Some(&["#112233".to_string(), "#445566".to_string()][..])
        );
        assert_eq!(
            after.facts.unknown.get("ai_tags"),
            Some(&serde_json::json!(["cat", "dusk"])),
            "AI output survived a re-mine"
        );
        assert_eq!(
            after.facts.unknown.get("legacy_note"),
            Some(&serde_json::json!("keep me")),
            "an unrecognized key survived a re-mine"
        );
        assert_eq!(after.facts.source_path, asset.facts.source_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unforced pass leaves a row that already looks mined alone.
    #[test]
    fn remine_without_force_skips_completed_rows() {
        let (lib, root) = temp_lib("remine2");
        let conn = lib.store().conn();
        let src = root.join("song.mp3");
        std::fs::write(&src, b"not really an mp3").unwrap();
        lib.import_into_store(&[src], None).unwrap();
        let mut asset = crate::store::assets::query(
            conn,
            &AssetQuery {
                kind: Some(AssetKind::Audio),
                ..Default::default()
            },
        )
        .unwrap()
        .items
        .pop()
        .unwrap();
        asset.facts.audio.sample_rate = Some(44100);
        crate::store::assets::update(
            conn,
            asset.id,
            &crate::model::AssetPatch {
                facts: Some(asset.facts.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            plan_remine(&lib, false).unwrap().items.len(),
            0,
            "a row with a sample rate looks already mined"
        );
        assert_eq!(
            plan_remine(&lib, true).unwrap().items.len(),
            1,
            "force re-reads it regardless"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
