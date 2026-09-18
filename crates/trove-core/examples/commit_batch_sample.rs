//! One sample of a small-file import at a fixed commit-batch size, for the
//! `COMMIT_BATCH` calibration in `docs/IMPORT-PIPELINE.md` §4.
//!
//! Process-per-sample on purpose (the same convention as [`stage_sweep`]):
//! the batch size is read once per process, so sweeping it means re-running
//! this with a different `TROVE_COMMIT_BATCH`:
//!
//! ```text
//! for batch in 16 64 128; do
//!   for pass in 0 1 2; do
//!     TROVE_COMMIT_BATCH=$batch \
//!     commit_batch_sample target/tmp/tiny-src 1000
//!   done
//! done
//! ```
//!
//! The import runs through the *real* job (`tasks::import::run` on a real
//! [`TaskManager`] thread) — the batch size only exists there, not in the
//! lower-level stage/commit pieces.
//!
//! [`stage_sweep`]: crate::stage_sweep

use std::path::PathBuf;
use std::time::Instant;

use trove_core::tasks::import::{ImportOptions, ImportSource};
use trove_core::tasks::{TaskKind, TaskManager};

fn main() {
    let mut args = std::env::args().skip(1);
    let src: PathBuf = args.next().expect("usage: commit_batch_sample <src> <files>").into();
    let files: usize = args.next().expect("usage: commit_batch_sample <src> <files>").parse().expect("files is a number");

    let batch = std::env::var("TROVE_COMMIT_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            eprintln!("TROVE_COMMIT_BATCH not set; sampling the default");
            0
        });

    // Fresh library per sample: a second pass into a populated database would
    // measure the dedup path, not the commit batch.
    let root = std::env::temp_dir().join(format!(
        "trove-commit-batch-{}-{}",
        std::process::id(),
        trove_core::model::new_id().simple()
    ));
    std::fs::create_dir_all(&root).unwrap();

    let options = ImportOptions {
        data_root: root.clone(),
        cache_root: root.join("cache"),
        storage: trove_core::media::import::ImportStorage::Link,
        source: ImportSource::Paths {
            paths: (0..files)
                .map(|i| src.join(format!("tiny{i}.png")))
                .collect(),
            into_collection: None,
        },
    };

    let started = Instant::now();
    let manager = TaskManager::new();
    let (_, rx) = manager
        .start(TaskKind::Import, "bench", move |ctx| {
            trove_core::tasks::import::run(&options, ctx)
        })
        .expect("the bench job starts");
    let outcome = rx.recv().expect("the bench job finishes");
    let elapsed = started.elapsed();

    if outcome.report.imported_count() != files {
        for (i, skip) in outcome.report.skipped.iter().take(3).enumerate() {
            eprintln!("skip {i}: {} — {}", skip.path.display(), skip.reason);
        }
        eprintln!(
            "sanity failed: {} imported, {} skipped (expected {files})",
            outcome.report.imported_count(),
            outcome.report.skipped_count(),
        );
        std::process::exit(1);
    }
    {
        let per_file = elapsed.as_secs_f64() * 1000.0 / files as f64;
        println!(
            "batch {batch:>3} : {per_file:>7.3} ms/file  ({imported} imported, total {elapsed:.1?})",
            imported = outcome.report.imported_count(),
        );
    }

    std::fs::remove_dir_all(&root).ok();
}
