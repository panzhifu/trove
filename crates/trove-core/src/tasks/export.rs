//! The export job: hand the selected assets' files to a user-chosen folder —
//! originals copied verbatim, images re-encoded through the convert pipeline
//! when a raster target is chosen, videos transcoded or remuxed through the
//! system ffmpeg, every other kind copied as it is.
//!
//! One item at a time on this job's thread, with the same cooperative shape
//! every job has: pause/cancel checkpoints between items, progress after
//! each one, and per-item failures collected into the report instead of
//! ending the run — one undecodable image must not sink the other nine
//! hundred. The step primitives live in [`crate::media::export`]; this
//! module is the loop, the naming and the accounting.

use std::collections::HashSet;
use std::path::PathBuf;

use super::JobContext;
use crate::error::Result;
use crate::media::{convert, export};
use crate::model::AssetKind;

/// Everything one export run needs: the destination, the per-kind targets
/// and the items with their sources already resolved. Built on the main
/// thread (the store is thread-confined) and cloned per retry attempt, hence
/// plain `Clone` data throughout.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// The folder every output lands in, flat.
    pub dest: PathBuf,
    pub items: Vec<ExportItem>,
    /// Images become this raster format; `None` copies the original bytes.
    pub image_format: Option<convert::ConvertFormat>,
    /// JPEG quality, 1–100; applies when `image_format` is JPEG.
    pub jpeg_quality: u8,
    pub video_format: export::VideoExportFormat,
    pub video_quality: export::VideoQuality,
    /// Cap on the exported video's height. Never enlarges.
    pub video_max_height: Option<u32>,
}

/// One planned export: the source file, the name it should land under
/// (before collision suffixes) and the kind that picks its rule.
#[derive(Debug, Clone)]
pub struct ExportItem {
    pub asset_id: uuid::Uuid,
    /// Absolute source path: the library blob or a linked original.
    pub source: PathBuf,
    pub title: String,
    pub kind: AssetKind,
}

/// One item that did not make it, with the reason the step reported.
#[derive(Debug, Clone)]
pub struct ExportSkip {
    pub source: PathBuf,
    pub reason: String,
}

/// What the run produced: the files written and the ones that failed.
#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub written: Vec<PathBuf>,
    pub failed: Vec<ExportSkip>,
}

/// The job's outcome. `cancelled` is true when the run stopped early on the
/// user's request — through the manager this surfaces as
/// [`TaskEvent::Cancelled`](super::TaskEvent::Cancelled) and the value is
/// dropped; direct callers (tests, the CLI) read it here.
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub report: ExportReport,
    pub cancelled: bool,
}

/// Run the export to completion. Synchronous and self-contained: tests call
/// it directly, [`super::TaskManager`] runs it on a thread.
pub fn run(options: &ExportOptions, ctx: &JobContext) -> Result<ExportOutcome> {
    let total = options.items.len() as u64;
    ctx.set_total(total);

    // One set for the whole run: two assets named the same land as
    // `name.ext` and `name (2).ext`, in folder order — the same rule a
    // second visit to the same destination folder obeys via the `exists`
    // check in `unique_path`.
    let mut used: HashSet<String> = HashSet::new();
    let mut report = ExportReport::default();
    let mut cancelled = false;

    for (index, item) in options.items.iter().enumerate() {
        ctx.park_if_paused();
        if ctx.cancelled() {
            cancelled = true;
            break;
        }
        match export_one(options, item, &mut used, ctx) {
            Ok(path) => report.written.push(path),
            // A step that died *because* the user cancelled is not a
            // failure — the kill switch is exactly what felled it. The
            // top-of-loop check below ends the run either way.
            Err(reason) if !ctx.cancelled() => {
                report.failed.push(ExportSkip {
                    source: item.source.clone(),
                    reason,
                });
            }
            Err(_) => {}
        }
        ctx.progress(index as u64 + 1, total);
    }

    ctx.set_summary(format!(
        "{} exported, {} failed",
        report.written.len(),
        report.failed.len()
    ));
    Ok(ExportOutcome { report, cancelled })
}

/// Export one item by its kind's rule: images through the convert pipeline
/// (or copied when no raster target was chosen), videos through ffmpeg
/// (or copied when the target is `Original`), everything else copied.
fn export_one(
    options: &ExportOptions,
    item: &ExportItem,
    used: &mut HashSet<String>,
    ctx: &JobContext,
) -> std::result::Result<PathBuf, String> {
    let base = convert::sanitize_title(&item.title);
    match item.kind {
        AssetKind::Image => match options.image_format {
            Some(format) => convert::convert_item(
                &options.dest,
                &convert::ConvertItem {
                    asset_id: item.asset_id,
                    source: item.source.clone(),
                    title: item.title.clone(),
                },
                &convert::ConvertOptions {
                    format,
                    quality: options.jpeg_quality,
                    // The export dialog offers no size cap; an export is a
                    // hand-off at full resolution unless a future option
                    // says otherwise.
                    max_dimension: None,
                },
                used,
            ),
            None => {
                let out = convert::unique_path(&options.dest, &base, &source_ext(item), used);
                export::copy_file(&item.source, &out)
            }
        },
        AssetKind::Video => match options.video_format {
            export::VideoExportFormat::Original => {
                let out = convert::unique_path(&options.dest, &base, &source_ext(item), used);
                export::copy_file(&item.source, &out)
            }
            target => {
                let ext = target.ext().unwrap_or("bin");
                let out = convert::unique_path(&options.dest, &base, ext, used);
                export::export_video(
                    &item.source,
                    &out,
                    target,
                    options.video_quality,
                    options.video_max_height,
                    ctx.cancel_flag(),
                )
            }
        },
        _ => {
            let out = convert::unique_path(&options.dest, &base, &source_ext(item), used);
            export::copy_file(&item.source, &out)
        }
    }
}

/// The source's extension, lowercased for a consistent output name.
fn source_ext(item: &ExportItem) -> String {
    item.source
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "bin".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempdir::Temp;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    mod tempdir {
        use std::path::PathBuf;

        pub struct Temp(PathBuf);
        impl Temp {
            pub fn new(name: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "trove-task-export-{name}-{}-{}",
                    std::process::id(),
                    uuid::Uuid::new_v4().simple()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Temp(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Temp {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
    }

    fn item(title: &str, source: &Path, kind: AssetKind) -> ExportItem {
        ExportItem {
            asset_id: uuid::Uuid::new_v4(),
            source: source.to_path_buf(),
            title: title.into(),
            kind,
        }
    }

    fn options(dest: &Path, items: Vec<ExportItem>) -> ExportOptions {
        ExportOptions {
            dest: dest.to_path_buf(),
            items,
            image_format: None,
            jpeg_quality: 85,
            video_format: export::VideoExportFormat::Original,
            video_quality: export::VideoQuality::Balanced,
            video_max_height: None,
        }
    }

    /// The mixed-batch contract: each kind follows its own rule, one bad
    /// item is reported and the batch continues, and the report names
    /// every output.
    #[test]
    fn a_mixed_batch_exports_each_kind_by_its_rule() {
        let root = Temp::new("mixed");
        let dest = root.path().join("out");
        let png = root.path().join("photo.png");
        fs::write(&png, PNG_1X1).unwrap();
        let raw_txt = root.path().join("notes.txt");
        fs::write(&raw_txt, "hello").unwrap();
        let bad_video = root.path().join("clip.mp4");
        fs::write(&bad_video, b"not a video").unwrap();

        let mut opts = options(
            &dest,
            vec![
                item("Photo", &png, AssetKind::Image),
                item("Notes", &raw_txt, AssetKind::Document),
                item("Clip", &bad_video, AssetKind::Video),
                item("Second", &png, AssetKind::Image),
            ],
        );
        // The image target applies to every image: both photos re-encode
        // to JPEG, the document copies, the video transcodes so the
        // garbage file actually reaches ffmpeg (or its absence) and fails
        // as an item instead of being copied.
        opts.image_format = Some(convert::ConvertFormat::Jpeg);
        opts.video_format = export::VideoExportFormat::Mp4H264;

        let outcome = run(&opts, &JobContext::for_tests(false)).unwrap();
        assert_eq!(outcome.report.written.len(), 3, "{:?}", outcome.report);
        assert_eq!(outcome.report.failed.len(), 1);
        assert_eq!(
            outcome.report.failed[0].source, bad_video,
            "the garbage video is the failure"
        );
        assert!(!outcome.cancelled);

        let names: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"Photo.jpg".to_string()), "{names:?}");
        assert!(names.contains(&"Notes.txt".to_string()), "{names:?}");
        assert!(names.contains(&"Second.jpg".to_string()), "{names:?}");
        // The killed item left no output under its final name.
        assert!(!names.iter().any(|n| n.starts_with("Clip")), "{names:?}");
    }

    /// Two assets with the same title must not overwrite each other — the
    /// second gets the ` (2)` suffix, whether both are copies or the batch
    /// mixes rules.
    #[test]
    fn same_titled_copies_get_collision_suffixes() {
        let root = Temp::new("collide");
        let dest = root.path().join("out");
        let png = root.path().join("a.png");
        fs::write(&png, PNG_1X1).unwrap();
        let png2 = root.path().join("b.png");
        fs::write(&png2, PNG_1X1).unwrap();

        let opts = options(
            &dest,
            vec![
                item("Same", &png, AssetKind::Image),
                item("Same", &png2, AssetKind::Image),
            ],
        );
        let outcome = run(&opts, &JobContext::for_tests(false)).unwrap();
        assert_eq!(outcome.report.written.len(), 2);
        let names: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"Same.png".to_string()) && names.contains(&"Same (2).png".to_string()),
            "{names:?}"
        );
    }

    /// A video exported with the `Original` target is a byte-exact copy
    /// that never touches ffmpeg — the one video path that must work on a
    /// machine without it.
    #[test]
    fn an_original_video_is_copied_without_ffmpeg() {
        let root = Temp::new("video-copy");
        let dest = root.path().join("out");
        let bytes: Vec<u8> = (0..64u8).collect();
        let source = root.path().join("clip.mov");
        fs::write(&source, &bytes).unwrap();

        let opts = options(&dest, vec![item("Clip", &source, AssetKind::Video)]);
        let outcome = run(&opts, &JobContext::for_tests(false)).unwrap();
        assert!(outcome.report.failed.is_empty(), "{:?}", outcome.report);
        assert_eq!(outcome.report.written, vec![dest.join("Clip.mov")]);
        assert_eq!(fs::read(dest.join("Clip.mov")).unwrap(), bytes);
    }

    /// A cancellation requested before the run lands means nothing was
    /// written and the outcome says so.
    #[test]
    fn a_cancelled_run_writes_nothing() {
        let root = Temp::new("cancel");
        let dest = root.path().join("out");
        let png = root.path().join("a.png");
        fs::write(&png, PNG_1X1).unwrap();

        let opts = options(&dest, vec![item("A", &png, AssetKind::Image)]);
        let outcome = run(&opts, &JobContext::for_tests(true)).unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.report.written.is_empty());
        assert!(!dest.join("A.png").exists());
    }
}
