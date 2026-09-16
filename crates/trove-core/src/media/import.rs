//! File import pipeline: hash, probe and record a source file as an asset.
//!
//! A user import **links**: the file stays where the user keeps it, and the
//! record remembers the path. The library therefore holds no copy of the
//! user's media — only what it derived from it (a thumbnail in the cache root,
//! mined metadata in the database). [`ImportStorage::Copy`] exists for the
//! narrow case of a source Trove owns and is about to delete or overwrite.
//!
//! The pipeline is split into two phases so a UI can do the slow part (hash +
//! probe + thumbnail, pure filesystem work) on a background thread and the fast
//! part (database commit, which must not race the UI thread's reads) back on
//! the main thread:
//!
//! - [`stage_source`] — background: hash, probe, thumbnail under the cache root
//! - [`commit_staged`] — foreground: dedupe against the store, insert rows

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{blob, metadata, probe, search, thumb};
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
    Ok(commit_staged_all(
        store.conn(),
        into_collection,
        stage_all(data_root, cache_root, sources, storage),
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

/// Cap on staging threads, regardless of how many cores the machine has.
///
/// Staging is I/O-bound, not CPU-bound: each file costs two fresh on-disk
/// writes (blob + thumbnail, both temp-file + rename), so widening the pool
/// past a few threads only deepens contention on the filesystem's metadata
/// locks. The parallel speedup also depends strongly on file size — small
/// files are metadata-lock bound and barely scale at all, large ones scale
/// close to linearly — so this is a fixed ceiling rather than a tuned
/// optimum.
///
/// The value assumes a btrfs library. On a filesystem with cheaper
/// concurrent writes (XFS/ext4 on NVMe) a larger pool may win; if that ever
/// matters, make this adaptive on the target's fstype rather than raising it
/// blindly.
const STAGE_THREADS_MAX: usize = 4;

/// The pool staging runs on. Built once, outside the global rayon pool: the
/// global pool is sized to the core count, which is exactly the overshoot
/// [`STAGE_THREADS_MAX`] exists to avoid — and the global pool is also shared
/// with the PLY parser's own `par_iter`, which should keep its full width.
fn stage_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(stage_thread_count())
            .thread_name(|i| format!("trove-stage-{i}"))
            .build()
            .expect("build staging thread pool")
    })
}

/// How wide the staging pool is on this machine: [`STAGE_THREADS_MAX`], or
/// the core count when that is smaller. Exposed for benchmarks and logs.
pub fn stage_thread_count() -> usize {
    STAGE_THREADS_MAX.min(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

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
) -> Vec<std::result::Result<StagedFile, ImportSkip>> {
    use rayon::prelude::*;

    // Bounded parallel: every stage is independent file I/O (hash + blob copy
    // + thumbnail) and the two on-disk writes per file make the filesystem the
    // bottleneck, so the pool is deliberately narrower than the core count —
    // see [`STAGE_THREADS_MAX`]. `par_iter` preserves input order, and
    // blob/thumb writes are temp-file + rename, so concurrent staging of
    // identical content cannot corrupt anything.
    stage_pool().install(|| {
        sources
            .par_iter()
            .map(|src| {
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

/// Phase one (slow, pure I/O): hash + probe one source file.
/// [`ImportStorage::Link`] leaves the file where it is and the record points
/// at it; `Copy` writes a content-addressed blob into `data_root` first. The
/// thumbnail is always written under `cache_root`.
pub fn stage_source(
    data_root: &Path,
    cache_root: &Path,
    src: &Path,
    storage: ImportStorage,
) -> Result<StagedFile> {
    let file_name = file_name_of(src)?;
    let ext = probe::normalize_ext(
        &src.extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default(),
    );

    // Linked mode: hash the source in place; no blob is written. The probe
    // and thumbnail generation read the original file directly.
    let (sha256, size, rel_path, blob_path) = if storage.copies() {
        let staged = blob::stage(src, data_root, &ext)?;
        let blob_path = data_root.join(&staged.rel_path);
        (staged.sha256, staged.size, staged.rel_path, blob_path)
    } else {
        let (sha256, size) = blob::hash_file(src)?;
        (sha256, size, String::new(), src.to_path_buf())
    };
    let p = probe::probe(&ext);
    let (width, height, video_duration_ms) = match p.kind {
        AssetKind::Image => match probe::image_dimensions(&blob_path) {
            Some(d) => (Some(d.width), Some(d.height), None),
            None => (None, None, None),
        },
        // MP4-family containers carry track dimensions + duration in the moov
        // box (pure-Rust read); other containers stay empty until probed.
        AssetKind::Video => match probe::video_facts(&blob_path) {
            Some(f) => (Some(f.width), Some(f.height), f.duration_ms),
            None => (None, None, None),
        },
        _ => (None, None, None),
    };
    // Generate (or confirm) the thumbnail cache entry on the background thread.
    let thumb_path = thumb::ensure(cache_root, &sha256, p.kind, &blob_path);
    // Mine rich metadata (EXIF camera fields, audio tags/duration, font
    // tables, video container). Best-effort. The palette is read from the
    // thumbnail: the original has already been decoded once for the thumbnail,
    // and a second full decode of a 6000x4000 image costs ~120 ms per file.
    // (EXIF still comes from the original; thumbnails don't carry it.)
    let color_source = thumb_path.clone().unwrap_or_else(|| blob_path.clone());
    let mut mined = metadata::mine(&blob_path, p.kind, &color_source);
    // Visual fingerprint (pHash + colour histogram) for search-by-image and
    // search-by-colour, computed from the small thumbnail so a huge photo
    // costs no more than a tiny one. Stored in the visual facts and persisted
    // by `commit_staged` together with the rest of the mined metadata.
    if p.kind == AssetKind::Image {
        let sig_source = thumb_path.unwrap_or_else(|| blob_path.clone());
        let sig = search::VisualSignature::from_image(&sig_source);
        if sig.phash != search::PHash(0) {
            sig.apply_to_facts(&mut mined.facts);
        }
    }
    if mined.duration_ms.is_none() {
        mined.duration_ms = video_duration_ms;
    }

    Ok(StagedFile {
        path: src.to_path_buf(),
        file_name,
        ext,
        sha256,
        size,
        rel_path,
        kind: p.kind,
        mime: p.mime,
        width,
        height,
        mined,
        linked: !storage.copies(),
    })
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

    // Note: visual signature is computed in background after import to keep
    // the import pipeline fast. See `compute_visual_signature_background()`.

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

fn file_name_of(src: &Path) -> Result<String> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::Validation("path has no file name".into()))?
        .to_string_lossy()
        .to_string();
    if name.trim().is_empty() {
        return Err(Error::Validation("path has no file name".into()));
    }
    let mut name = name;
    name.truncate(crate::model::MAX_NAME_LEN);
    Ok(name)
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
        let staged = stage_all(
            &root,
            &cache,
            &[good.clone(), missing.clone()],
            ImportStorage::Link,
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
        let staged2 = stage_all(&root, &cache, &[good], ImportStorage::Link);
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

        let staged = stage_all(
            &root,
            &cache,
            std::slice::from_ref(&src),
            ImportStorage::Link,
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

        let staged = stage_all(
            &root,
            &cache,
            std::slice::from_ref(&src),
            ImportStorage::Copy,
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
}
