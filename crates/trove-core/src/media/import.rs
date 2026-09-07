//! File import pipeline: copy sources into the content-addressed media store
//! and record them as assets.
//!
//! The pipeline is split into two phases so a UI can do the slow part (copy +
//! hash + probe, pure filesystem work) on a background thread and the fast
//! part (database commit, which must not race the UI thread's reads) back on
//! the main thread:
//!
//! - [`stage_source`] — background: copy into `media/…`, hash, probe
//! - [`commit_staged`] — foreground: dedupe against the store, insert rows

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{blob, metadata, probe, thumb};
use crate::error::{Error, Result};
use crate::model::{Asset, AssetKind, Origin, now};
use crate::store::{Store, assets, collections};

/// How an import batch is automatically grouped into (auto-created) root
/// collections, independent of any explicit `into_collection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCollection {
    /// Each file joins a collection named after its source directory, e.g.
    /// `~/Photos/Holiday/IMG_01.jpg` -> "Holiday".
    SourceFolder,
    /// Each file joins `YYYY-MM` named after its EXIF capture date (falling
    /// back to the import date when unknown).
    CaptureYearMonth,
    /// Each file joins `YYYY-MM` named after the day it was imported.
    ImportYearMonth,
}

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
    pub rel_path: String,
    pub kind: AssetKind,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Extracted EXIF / audio metadata (best-effort).
    pub mined: metadata::MinedMetadata,
}

/// Synchronous all-in-one import (tests, small batches). Equivalent to
/// `stage_source` + `commit_staged` per file, without auto-categorization.
pub fn import_files(
    store: &Store,
    root: &Path,
    sources: &[PathBuf],
    into_collection: Option<Uuid>,
) -> Result<ImportReport> {
    import_files_assigned(store, root, sources, into_collection, None)
}

/// As [`import_files`], but with an [`AutoCollection`] strategy that groups the
/// batch into auto-created root collections (in addition to any fixed target).
pub fn import_files_assigned(
    store: &Store,
    root: &Path,
    sources: &[PathBuf],
    into_collection: Option<Uuid>,
    auto: Option<AutoCollection>,
) -> Result<ImportReport> {
    if let Some(cid) = into_collection
        && collections::get(store.conn(), cid)?.is_none()
    {
        return Err(Error::NotFound("collection"));
    }
    Ok(commit_staged_all(
        store,
        into_collection,
        auto,
        stage_all(root, sources),
    ))
}

/// Phase one for a batch: stage every source file (copy + hash + probe + thumbnail).
/// Pure filesystem work, safe to run on a background thread. Individual
/// failures never abort the batch; they are collected as [`ImportSkip`]s.
pub fn stage_all(
    root: &Path,
    sources: &[PathBuf],
) -> Vec<std::result::Result<StagedFile, ImportSkip>> {
    sources
        .iter()
        .map(|src| {
            stage_source(root, src).map_err(|e| ImportSkip {
                path: src.clone(),
                reason: e.to_string(),
            })
        })
        .collect()
}

/// Phase two for a batch: commit staged files (or pass through staging
/// failures) into the database, collecting per-file outcomes into an
/// [`ImportReport`]. Runs on whatever thread owns the [`Store`]; callers that
/// keep a `Store` on the UI thread should commit from that thread.
pub fn commit_staged_all(
    store: &Store,
    into_collection: Option<Uuid>,
    auto: Option<AutoCollection>,
    staged: Vec<std::result::Result<StagedFile, ImportSkip>>,
) -> ImportReport {
    let mut report = ImportReport::default();
    for item in staged {
        match item {
            Ok(file) => match commit_staged(store, into_collection, auto, &file) {
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

/// Phase one (slow, pure I/O): copy + hash + probe one source file.
pub fn stage_source(root: &Path, src: &Path) -> Result<StagedFile> {
    let file_name = file_name_of(src)?;
    let ext = probe::normalize_ext(
        &src.extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default(),
    );

    let staged = blob::stage(src, root, &ext)?;
    let p = probe::probe(&ext);
    let blob_path = root.join(&staged.rel_path);
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
    thumb::ensure(root, &staged.sha256, p.kind, &blob_path);
    // Mine rich metadata (EXIF camera fields, audio tags/duration, font
    // tables, video container). Best-effort.
    let mut mined = metadata::mine(&blob_path, p.kind);
    if mined.duration_ms.is_none() {
        mined.duration_ms = video_duration_ms;
    }

    Ok(StagedFile {
        path: src.to_path_buf(),
        file_name,
        ext,
        sha256: staged.sha256,
        size: staged.size,
        rel_path: staged.rel_path,
        kind: p.kind,
        mime: p.mime,
        width,
        height,
        mined,
    })
}

/// Phase two (fast, database-only): dedupe or insert, attach to a collection.
///
/// Runs on whatever thread calls it; callers that keep a `Store` on the UI
/// thread should commit from that thread.
pub fn commit_staged(
    store: &Store,
    into_collection: Option<Uuid>,
    auto: Option<AutoCollection>,
    staged: &StagedFile,
) -> Result<ImportItem> {
    // Resolve the fixed target (if any) plus the auto-created collection, so
    // both the fresh-insert and dedup paths attach identically.
    let mut targets: Vec<Uuid> = Vec::new();
    if let Some(cid) = into_collection {
        targets.push(cid);
    }
    if let Some(auto) = auto {
        let name = auto_collection_name(auto, staged);
        targets.push(collections::ensure_root_named(store.conn(), &name)?.id);
    }

    // Reuse an existing live asset with identical content.
    if let Some(existing) = assets::find_by_sha256(store.conn(), &staged.sha256)? {
        for cid in &targets {
            collections::add_asset(store.conn(), *cid, existing.id)?;
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
    let mut extra = std::collections::BTreeMap::new();
    extra.extend(mined.extra.clone());

    // Note: visual signature is computed in background after import to keep
    // the import pipeline fast. See `compute_visual_signature_background()`.

    let asset = Asset {
        id: Uuid::new_v4(),
        origin: Origin::Stored,
        rel_path: Some(staged.rel_path.clone()),
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
        extra,
        created_at: now(),
        updated_at: now(),
        trashed_at: None,
    };
    assets::insert(store.conn(), &asset)?;
    for cid in &targets {
        collections::add_asset(store.conn(), *cid, asset.id)?;
    }

    Ok(ImportItem {
        asset_id: asset.id,
        file_name: asset.file_name.clone(),
        kind: asset.kind,
        sha256: staged.sha256.clone(),
        reused: false,
    })
}

/// Derive the auto-collection name for a staged file under a strategy. Result
/// is always a non-empty, name-length-safe string so `ensure_root_named` never
/// rejects it.
fn auto_collection_name(auto: AutoCollection, staged: &StagedFile) -> String {
    let raw = match auto {
        AutoCollection::SourceFolder => staged
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().to_string())
            .filter(|s| !s.is_empty() && s != "." && s != "/")
            .unwrap_or_else(|| "Miscellaneous".to_string()),
        AutoCollection::CaptureYearMonth => staged
            .mined
            .captured_at
            .map(|dt| dt.format("%Y-%m").to_string())
            .unwrap_or_else(utc_now_month),
        AutoCollection::ImportYearMonth => utc_now_month(),
    };
    let mut name = raw.trim().to_string();
    if name.is_empty() {
        name = "Miscellaneous".to_string();
    }
    name.truncate(crate::model::MAX_NAME_LEN);
    name
}

fn utc_now_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
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
        let store = Store::in_memory().unwrap();

        let good = root.join("pic.png");
        std::fs::write(&good, PNG_1X1).unwrap();
        let missing = root.join("nope.png");

        // Phase one: staging collects failures instead of aborting the batch.
        let staged = stage_all(&root, &[good.clone(), missing.clone()]);
        assert_eq!(staged.len(), 2);
        assert!(staged[0].is_ok());
        assert!(staged[1].is_err());

        // Phase two: the commit loop turns everything into an ImportReport.
        let report = commit_staged_all(&store, None, Some(AutoCollection::SourceFolder), staged);
        assert_eq!(report.imported_count(), 1);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped[0].path, missing);
        assert!(!report.skipped[0].reason.is_empty());

        // The auto collection named after the source folder exists.
        let conn = store.conn();
        let roots = collections::roots(conn).unwrap();
        assert!(
            roots
                .iter()
                .any(|c| c.name == "trove-import-batch" || c.name.starts_with("trove-import-"))
        );

        // Re-importing identical content dedupes (reused = true).
        let staged2 = stage_all(&root, &[good]);
        let report2 = commit_staged_all(&store, None, Some(AutoCollection::SourceFolder), staged2);
        assert_eq!(report2.imported_count(), 1);
        assert!(report2.imported[0].reused);

        std::fs::remove_dir_all(&root).ok();
    }
}
