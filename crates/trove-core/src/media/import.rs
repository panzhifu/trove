//! File import pipeline: hash, probe and record a source file as an asset.
//!
//! A user import **links**: the file stays where the user keeps it, and the
//! record remembers the path. The library therefore holds no copy of the
//! user's media — only what it derived from it (a thumbnail in the cache root,
//! mined metadata in the database). [`ImportStorage::Copy`] exists for the
//! narrow case of a source Trove owns and is about to delete or overwrite.
//!
//! The pipeline is split into two phases so a UI can do the slow part (hash +
//! probe + decode + thumbnail, pure filesystem work) on a background thread and
//! the fast part (database commit, which must not race the UI thread's reads)
//! back on the main thread:
//!
//! - [`stage_source`] — background: the stage pipeline in
//!   [`crate::media::pipeline`], which decodes each image once and hands that
//!   one buffer to the thumbnail writer, the palette miner and the visual
//!   signature
//! - [`commit_staged`] — foreground: dedupe against the store, insert rows

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use super::metadata;
use super::pipeline::{self, StageIo};
use crate::error::{Error, Result};
use crate::model::{Asset, AssetKind, Origin, UsageStatus, now};
use crate::store::{Store, assets, collections};
use rusqlite::Connection;

/// One successfully imported (or deduplicated) file.
#[derive(Debug, Clone)]
pub struct ImportItem {
    pub asset_id: Uuid,
    pub file_name: String,
    pub kind: AssetKind,
    pub sha256: String,
    /// `true` when the content already existed and the existing asset was
    /// reused instead of inserting a new record.
    pub reused: bool,
}

/// One file that could not be imported, with the reason.
#[derive(Debug, Clone)]
pub struct ImportSkip {
    pub path: PathBuf,
    pub reason: String,
}

/// Outcome of importing a batch of files. Individual failures never abort the
/// batch; they are collected in [`ImportReport::skipped`].
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub imported: Vec<ImportItem>,
    pub skipped: Vec<ImportSkip>,
    /// Files handed to the job that the library already held (same name and
    /// size) and so were never staged — the resident inbox sweep re-runs over
    /// a directory that keeps its files, and this is the "nothing new"
    /// counter that keeps those sweeps quiet. Not a skip: nothing went wrong.
    pub already_imported: u64,
}

impl ImportReport {
    pub fn imported_count(&self) -> usize {
        self.imported.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }
}

/// A source file that has been copied into the media store and probed, but not
/// yet committed to the database.
#[derive(Debug, Clone)]
pub struct StagedFile {
    /// The original source path (for diagnostics / skip reports).
    pub path: PathBuf,
    pub file_name: String,
    pub ext: String,
    pub sha256: String,
    pub size: u64,
    /// Library-relative blob path; empty when [`StagedFile::linked`] is set
    /// (the file stays at its original location).
    pub rel_path: String,
    pub kind: AssetKind,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Extracted EXIF / audio metadata (best-effort).
    pub mined: metadata::MinedMetadata,
    /// Linked import: the file was not copied; the record points at the
    /// original location via `extra["source_path"]`.
    pub linked: bool,
}

/// Synchronous all-in-one import (tests, small batches). Equivalent to
/// `stage_source` + `commit_staged` per file.
/// Imported assets go directly to "All Assets" unless `into_collection` is set.
pub fn import_files(
    store: &Store,
    data_root: &Path,
    cache_root: &Path,
    sources: &[PathBuf],
    storage: ImportStorage,
    into_collection: Option<Uuid>,
) -> Result<ImportReport> {
    if let Some(cid) = into_collection
        && collections::get(store.conn(), cid)?.is_none()
    {
        return Err(Error::NotFound("collection"));
    }
    // The synchronous path has no cancellation story (it is the library
    // import/export round-trip), so staging here runs to completion.
    let never_cancelled = AtomicBool::new(false);
    Ok(commit_staged_all(
        store.conn(),
        into_collection,
        stage_all(data_root, cache_root, sources, storage, &never_cancelled),
    ))
}

/// How an import treats its sources.
///
/// A user import always links: the file stays where it is, the library only
/// remembers where. Copying is reserved for the two cases where Trove owns
/// the source and will delete or overwrite it — the temporary extraction of a
/// media package, and the re-encoded output of an in-place edit. Copying a
/// user's file into the library would duplicate their disk usage for nothing,
/// since every reader opens the original anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStorage {
    /// Leave the file where it is; the record points at it.
    Link,
    /// Copy into the data root's `media/` store and record a library-relative
    /// path. For sources that are about to disappear.
    Copy,
}

impl ImportStorage {
    /// Whether the source is copied into the store.
    pub fn copies(self) -> bool {
        matches!(self, Self::Copy)
    }
}

/// Staging has two regimes and they want opposite pool widths.
///
/// Per-file medians from the arm sweep (`stage_sweep`, 5 sittings x 3 passes,
/// btrfs on NVMe, 20 hardware threads):
///
/// | batch | w1 | w4 | w12 |
/// |---|---|---|---|
/// | 30 x 3000x2000 JPEG | 30.63 ms | 9.19 ms | **4.98 ms** |
/// | 300 x 1x1 PNG | 0.0445 ms | **0.0190 ms** | 0.0210 ms |
///
/// A photo-sized file spends ~30 ms of CPU at width 1 while all of its I/O —
/// one fresh thumbnail plus a few hundred KB of reads — costs under 0.2 ms, so
/// it is decode-bound and keeps scaling to 12. A 1x1 PNG has no decode to
/// speak of: at 0.019 ms/file it is already sitting on the ~0.0135 ms cost of
/// creating one file, so extra threads only deepen contention on the
/// filesystem's metadata locks and the curve flattens at four.
///
/// Hence two pools, chosen per batch by average source size — the cheapest
/// proxy for "how much decoding is in there" available before the pipeline
/// runs. The narrow arm is the old fixed width, which is right for the
/// thumbnail-sized end; the wide arm is for batches of real photographs, where
/// the narrow arm was leaving ~1.85x on the table.
///
/// Both are ceilings rather than tuned optima: [`STAGE_THREADS_WIDE_MAX`] is
/// where the sweep was still descending when it stopped, so a machine with
/// fewer cores gets a proportionally smaller pool, and nobody gets more than
/// 12. The width also assumes a local filesystem — on a network or fuse mount
/// [`STAGE_THREADS_ENV`] can pin it without a rebuild.
const STAGE_THREADS_NARROW: usize = 4;
const STAGE_THREADS_WIDE_MAX: usize = 12;

/// Average source size at which a batch switches to the wide pool: ~1 Mpx of
/// JPEG, a file whose decode costs a few milliseconds.
///
/// This threshold is interpolated, not measured: the sweep's two sets sit at
/// ~90 B and ~290 KB, so the crossover is bracketed but not pinned. The `mid`
/// set in `target/tmp/bench-real.sh` exists to close that gap.
const STAGE_WIDE_MIN_AVG_BYTES: u64 = 64 * 1024;

/// Sources sampled to estimate a batch's average size. The estimate only picks
/// an arm, so a sample is enough — and it bounds the `stat` cost on a slow
/// mount to a few dozen calls rather than one per file.
const STAGE_SIZE_SAMPLE: usize = 64;

fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// [`STAGE_THREADS_ENV`] as a positive integer, when set.
fn env_thread_count() -> Option<usize> {
    std::env::var(STAGE_THREADS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// The narrow arm's width: the fixed floor, the core count when that is
/// smaller, or [`STAGE_THREADS_ENV`] when it is set to a positive integer.
///
/// Exposed for benchmarks and logs. Note this is the *floor* width — a batch
/// of large sources may run on [`stage_thread_ceiling`] threads; use
/// [`stage_thread_count_for`] to report what a given batch actually chose.
pub fn stage_thread_count() -> usize {
    env_thread_count().unwrap_or_else(|| STAGE_THREADS_NARROW.min(cores()))
}

/// The wide arm's width, honouring the same override. The ceiling for what any
/// batch can use.
pub fn stage_thread_ceiling() -> usize {
    env_thread_count().unwrap_or_else(|| STAGE_THREADS_WIDE_MAX.min(cores()))
}

/// Whether an average source size puts a batch on the wide arm.
fn wide_enough(avg_bytes: Option<u64>) -> bool {
    avg_bytes.is_some_and(|avg| avg >= STAGE_WIDE_MIN_AVG_BYTES)
}

/// Average size of up to [`STAGE_SIZE_SAMPLE`] sources, sampled with a stride
/// so a huge batch costs the same to size up as a small one.
fn sample_avg_bytes(sources: &[PathBuf]) -> Option<u64> {
    let n = sources.len();
    if n == 0 {
        return None;
    }
    let take = n.min(STAGE_SIZE_SAMPLE);
    let stride = n.div_ceil(take);
    let mut total: u64 = 0;
    let mut seen: u64 = 0;
    for src in sources.iter().step_by(stride).take(take) {
        if let Ok(meta) = std::fs::metadata(src) {
            total += meta.len();
            seen += 1;
        }
    }
    (seen > 0).then(|| total / seen)
}

/// Which arm this batch stages on. One decision for the whole batch: a mixed
/// drop of photos and icons still gets one pool, and its average is what
/// decides.
fn wide_arm(sources: &[PathBuf]) -> bool {
    env_thread_count().is_none() && wide_enough(sample_avg_bytes(sources))
}

/// How many threads `stage_all` will use for `sources`: the pinned override
/// when [`STAGE_THREADS_ENV`] is set, otherwise the arm the batch's average
/// source size selects. Exposed for benchmarks and logs.
pub fn stage_thread_count_for(sources: &[PathBuf]) -> usize {
    if wide_arm(sources) {
        stage_thread_ceiling()
    } else {
        stage_thread_count()
    }
}

/// The pool staging runs on, one per arm. Both are built once, outside the
/// global rayon pool: the global pool is sized to the core count, which is
/// exactly the overshoot the narrow arm exists to avoid — and the global pool
/// is also shared with the PLY parser's own `par_iter`, which should keep its
/// full width. At most one arm is ever doing work at a time, so the two
/// together still add up to less than the global pool.
fn stage_pool(wide: bool) -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;
    static NARROW: OnceLock<rayon::ThreadPool> = OnceLock::new();
    static WIDE: OnceLock<rayon::ThreadPool> = OnceLock::new();
    let (slot, width) = if wide {
        (&WIDE, stage_thread_ceiling())
    } else {
        (&NARROW, stage_thread_count())
    };
    slot.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(width)
            .thread_name(|i| format!("trove-stage-{i}"))
            .build()
            .expect("build staging thread pool")
    })
}

/// Environment override for the staging pool width, both arms. Benchmarks
/// sweep it to find where the filesystem stops scaling; users on an exotic
/// mount (network, fuse) can pin a width without a rebuild.
pub const STAGE_THREADS_ENV: &str = "TROVE_STAGE_THREADS";

/// Phase one for a batch: stage every source file (hash + probe + thumbnail).
/// Pure filesystem work, safe to run on a background thread. Individual
/// failures never abort the batch; they are collected as [`ImportSkip`]s.
/// Sources are hashed + probed where they lie; only [`ImportStorage::Copy`]
/// writes a blob into `data_root`.
pub fn stage_all(
    data_root: &Path,
    cache_root: &Path,
    sources: &[PathBuf],
    storage: ImportStorage,
    cancelled: &AtomicBool,
) -> Vec<std::result::Result<StagedFile, ImportSkip>> {
    use rayon::prelude::*;

    // Bounded parallel: every stage is independent file I/O (hash + blob copy
    // + thumbnail), and how wide it should run depends entirely on how much
    // decoding that I/O carries — see [`STAGE_THREADS_NARROW`]. `par_iter`
    // preserves input order, and blob/thumb writes are temp-file + rename, so
    // concurrent staging of identical content cannot corrupt anything.
    stage_pool(wide_arm(sources)).install(|| {
        sources
            .par_iter()
            .map(|src| {
                // Per-file cancellation checkpoint: with this, a cancelled
                // job stops staging the moment each file starts, so a
                // cancel-and-wait (the library swap) is bounded by the few
                // in-flight decodes instead of the whole window.
                if cancelled.load(Ordering::Relaxed) {
                    return Err(ImportSkip {
                        path: src.clone(),
                        reason: "cancelled".to_string(),
                    });
                }
                stage_source(data_root, cache_root, src, storage).map_err(|e| ImportSkip {
                    path: src.clone(),
                    reason: e.to_string(),
                })
            })
            .collect()
    })
}

/// Phase two for a batch: commit staged files (or pass through staging
/// failures) into the database, collecting per-file outcomes into an
/// [`ImportReport`]. Runs on whatever thread owns the connection; the
/// connection may be a transaction / savepoint (batched commits).
pub fn commit_staged_all(
    store: &Connection,
    into_collection: Option<Uuid>,
    staged: Vec<std::result::Result<StagedFile, ImportSkip>>,
) -> ImportReport {
    let mut report = ImportReport::default();
    for item in staged {
        match item {
            Ok(file) => match commit_staged(store, into_collection, &file) {
                Ok(item) => report.imported.push(item),
                Err(e) => report.skipped.push(ImportSkip {
                    path: file.path,
                    reason: e.to_string(),
                }),
            },
            Err(skip) => report.skipped.push(skip),
        }
    }
    report
}

/// Phase one (slow, pure I/O): run the staged pipeline over one source file.
///
/// The stages themselves live in [`super::pipeline`]; this is the single-file
/// entry point onto them. [`ImportStorage::Link`] leaves the file where it is
/// and the record points at it; `Copy` writes a content-addressed blob into
/// `data_root` first. The thumbnail is always written under `cache_root`.
pub fn stage_source(
    data_root: &Path,
    cache_root: &Path,
    src: &Path,
    storage: ImportStorage,
) -> Result<StagedFile> {
    let mut io = StageIo::new(src, data_root, cache_root, storage)?;
    pipeline::default_pipeline().run(&mut io)?;
    Ok(staged_from(io))
}

/// A finished [`StageIo`] as the commit phase wants it.
fn staged_from(io: StageIo) -> StagedFile {
    StagedFile {
        path: io.src,
        file_name: io.file_name,
        ext: io.ext,
        sha256: io.sha256,
        size: io.size,
        rel_path: io.rel_path,
        kind: io.kind,
        mime: io.mime,
        width: io.width,
        height: io.height,
        mined: io.mined,
        linked: io.linked,
    }
}

/// Phase two (fast, database-only): dedupe or insert, attach to a collection.
///
/// Runs on whatever thread calls it, on any connection to the library —
/// including a transaction or savepoint, so callers can batch commits.
pub fn commit_staged(
    conn: &Connection,
    into_collection: Option<Uuid>,
    staged: &StagedFile,
) -> Result<ImportItem> {
    // Resolve the fixed target (if any) plus the auto-created collection, so
    // both the fresh-insert and dedup paths attach identically.
    let mut targets: Vec<Uuid> = Vec::new();
    if let Some(cid) = into_collection {
        targets.push(cid);
    }

    // Reuse an existing live asset with identical content.
    if let Some(existing) = assets::find_by_sha256(conn, &staged.sha256)? {
        // A placeholder record (metadata restore without media) becomes a
        // full asset the moment its content lands in the library. Linked
        // records keep pointing at their original location.
        if existing.rel_path.is_none() && existing.origin == Origin::Stored {
            assets::set_rel_path(conn, existing.id, &staged.rel_path)?;
        }
        for cid in &targets {
            collections::add_asset(conn, *cid, existing.id)?;
        }
        return Ok(ImportItem {
            asset_id: existing.id,
            file_name: existing.file_name.clone(),
            kind: existing.kind,
            sha256: staged.sha256.clone(),
            reused: true,
        });
    }

    let mined = &staged.mined;
    let mut facts = mined.facts.clone();
    // Remember where the file came from: the folders panel browses by it.
    facts.source_path = Some(staged.path.display().to_string());

    // The visual signature (pHash + histogram) is part of `mined.facts`
    // already: the pipeline's `visual-sig` stage computes it from the same
    // decode the thumbnail and the palette were read from, so nothing is
    // decoded twice and nothing is deferred.

    let asset = Asset {
        id: Uuid::new_v4(),
        origin: if staged.linked {
            Origin::Linked
        } else {
            Origin::Stored
        },
        rel_path: if staged.linked {
            None
        } else {
            Some(staged.rel_path.clone())
        },
        file_name: staged.file_name.clone(),
        ext: staged.ext.clone(),
        mime: staged.mime.clone(),
        size_bytes: staged.size,
        sha256: Some(staged.sha256.clone()),
        kind: staged.kind,
        width: staged.width,
        height: staged.height,
        duration_ms: mined.duration_ms,
        captured_at: mined.captured_at,
        title: mined.title.clone(),
        description: None,
        rating: None,
        is_favorite: false,
        source_url: None,
        usage_status: UsageStatus::Unused,
        commercial_use: None,
        facts,
        created_at: now(),
        updated_at: now(),
        trashed_at: None,
    };
    assets::insert(conn, &asset)?;
    for cid in &targets {
        collections::add_asset(conn, *cid, asset.id)?;
    }

    Ok(ImportItem {
        asset_id: asset.id,
        file_name: asset.file_name.clone(),
        kind: asset.kind,
        sha256: staged.sha256.clone(),
        reused: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssetQuery;
    use crate::store::Store;

    /// A minimal valid 1x1 PNG.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("trove-import-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn batch_pipeline_collects_skips_and_dedupes() {
        let root = temp_root("batch");
        let cache = root.join("cache");
        let store = Store::in_memory().unwrap();

        let good = root.join("pic.png");
        std::fs::write(&good, PNG_1X1).unwrap();
        let missing = root.join("nope.png");

        // Phase one: staging collects failures instead of aborting the batch.
        let no_cancel = AtomicBool::new(false);
        let staged = stage_all(
            &root,
            &cache,
            &[good.clone(), missing.clone()],
            ImportStorage::Link,
        &no_cancel,
        );
        assert_eq!(staged.len(), 2);
        assert!(staged[0].is_ok());
        assert!(staged[1].is_err());

        // Phase two: the commit loop turns everything into an ImportReport.
        let report = commit_staged_all(store.conn(), None, staged);
        assert_eq!(report.imported_count(), 1);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped[0].path, missing);
        assert!(!report.skipped[0].reason.is_empty());

        // No auto-collection is created; asset goes directly to "All Assets".
        let conn = store.conn();
        let roots = collections::roots(conn).unwrap();
        assert_eq!(roots.len(), 0, "no auto-collection should be created");

        // Re-importing identical content dedupes (reused = true).
        let no_cancel = AtomicBool::new(false);
        let staged2 = stage_all(&root, &cache, &[good], ImportStorage::Link, &no_cancel);
        let report2 = commit_staged_all(store.conn(), None, staged2);
        assert_eq!(report2.imported_count(), 1);
        assert!(report2.imported[0].reused);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn linked_import_keeps_the_file_in_place() {
        let root = temp_root("linked");
        let cache = root.join("cache");
        let store = Store::in_memory().unwrap();

        // The source lives OUTSIDE the library root.
        let outside = std::env::temp_dir().join(format!("trove-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        let src = outside.join("linked.png");
        std::fs::write(&src, PNG_1X1).unwrap();

        let no_cancel = AtomicBool::new(false);
        let staged = stage_all(
            &root,
            &cache,
            std::slice::from_ref(&src),
            ImportStorage::Link,
        &no_cancel,
        );
        assert!(staged[0].is_ok(), "{:?}", staged[0].as_ref().err());
        let report = commit_staged_all(store.conn(), None, staged);
        assert_eq!(report.imported_count(), 1);

        let conn = store.conn();
        let all = assets::query(conn, &AssetQuery::default()).unwrap();
        let asset = &all.items[0];
        // Linked record: no blob copied, origin linked, original location
        // recorded in the facts.
        assert_eq!(asset.origin, Origin::Linked);
        assert!(asset.rel_path.is_none());
        assert!(walk_blobs(&root.join("media")).is_empty());
        assert_eq!(
            asset.facts.source_path.as_deref(),
            Some(src.display().to_string().as_str())
        );
        let (src_sha, _) = super::super::blob::hash_file(&src).unwrap();
        assert_eq!(asset.sha256.as_deref(), Some(src_sha.as_str()));

        // The thumbnail was generated from the original file, into the cache
        // root — not next to the database.
        let thumb = super::super::thumb::abs_path(&cache, asset.sha256.as_deref().unwrap());
        assert!(thumb.is_file());
        assert!(
            !super::super::thumb::abs_path(&root, asset.sha256.as_deref().unwrap()).exists(),
            "thumbnails must not land in the data root"
        );

        // The source file was never modified or moved.
        assert!(src.is_file());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// The one storage mode that still copies: a source Trove owns and is
    /// about to delete (a media package's extraction) must end up in the
    /// store, not as a link to a directory that is about to vanish.
    #[test]
    fn copied_import_writes_a_blob_into_the_data_root() {
        let root = temp_root("copied");
        let cache = root.join("cache");
        let store = Store::in_memory().unwrap();

        let src = root.join("packaged.png");
        std::fs::write(&src, PNG_1X1).unwrap();

        let no_cancel = AtomicBool::new(false);
        let staged = stage_all(
            &root,
            &cache,
            std::slice::from_ref(&src),
            ImportStorage::Copy,
            &no_cancel,
        );
        assert!(staged[0].is_ok(), "{:?}", staged[0].as_ref().err());
        let report = commit_staged_all(store.conn(), None, staged);
        assert_eq!(report.imported_count(), 1);

        let all = assets::query(store.conn(), &AssetQuery::default()).unwrap();
        let asset = &all.items[0];
        assert_eq!(asset.origin, Origin::Stored);
        assert!(asset.rel_path.is_some());
        // The blob is inside the data root, so deleting the source afterwards
        // leaves the asset intact.
        assert_eq!(walk_blobs(&root.join("media")).len(), 1);
        std::fs::remove_file(&src).unwrap();
        assert!(root.join(asset.rel_path.as_deref().unwrap()).is_file());

        std::fs::remove_dir_all(&root).ok();
    }

    /// Collect every file under the media store.
    fn walk_blobs(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk_blobs(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn average_size_picks_the_arm_at_the_threshold() {
        assert!(
            !wide_enough(None),
            "no readable source falls back to narrow"
        );
        assert!(!wide_enough(Some(0)));
        assert!(!wide_enough(Some(STAGE_WIDE_MIN_AVG_BYTES - 1)));
        assert!(wide_enough(Some(STAGE_WIDE_MIN_AVG_BYTES)));
        assert!(wide_enough(Some(STAGE_WIDE_MIN_AVG_BYTES * 8)));
    }

    #[test]
    fn average_size_samples_the_batch_by_size_not_by_name() {
        let root = temp_root("avg");
        let big = root.join("big.jpg");
        let small = root.join("small.png");
        std::fs::write(&big, vec![0u8; 200 * 1024]).unwrap();
        std::fs::write(&small, PNG_1X1).unwrap();

        // Four large sources average over the threshold; the same count of
        // small ones stays under it.
        let wide: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = root.join(format!("w{i}.jpg"));
                std::fs::write(&p, vec![0u8; 200 * 1024]).unwrap();
                p
            })
            .chain([big.clone()])
            .collect();
        let narrow: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = root.join(format!("n{i}.png"));
                std::fs::write(&p, PNG_1X1).unwrap();
                p
            })
            .chain([small])
            .collect();

        let wide_avg = sample_avg_bytes(&wide).unwrap();
        let narrow_avg = sample_avg_bytes(&narrow).unwrap();
        assert!(wide_avg >= 200 * 1024, "got {wide_avg}");
        assert!(narrow_avg < 1024, "got {narrow_avg}");
        assert!(wide_enough(Some(wide_avg)));
        assert!(!wide_enough(Some(narrow_avg)));

        // A batch where nothing can be stat'ed (deleted sources) reads as
        // unknowable, not as large.
        let gone: Vec<PathBuf> = (0..3).map(|i| root.join(format!("gone{i}"))).collect();
        assert_eq!(sample_avg_bytes(&gone), None);
        assert_eq!(sample_avg_bytes(&[]), None);

        std::fs::remove_dir_all(&root).ok();
    }
}
