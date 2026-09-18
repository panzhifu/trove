//! Import pipeline profiler: attributes staging time to individual sub-steps
//! so optimisation work targets a measured cost, not a guess.
//!
//! Run with a directory of source files:
//!   cargo run --release -p trove-core --example import_profile -- <src-dir> [n]
//!
//! ## What it measures
//!
//! Two things, deliberately separated:
//!
//! **The real pipeline** — `import::stage_all` (and `stage_source` for one
//! file) as the import job runs it, plus the commit loop. This is the number
//! to optimise and the only one to compare between builds.
//!
//! **The components** — each sub-step timed on its own, as a reference for
//! where the time inside the pipeline goes:
//!
//!   copy+hash   `blob::stage`      — one sequential read, copy into media/
//!   thumb       `thumb::ensure`    — full decode + resize (temp+rename)
//!   mine        `metadata::mine`   — EXIF + dominant colours
//!   colors      `color::dominant_colors` — the decode hidden inside `mine`
//!   phash       `search::VisualSignature::from_image`
//!   commit      `commit_staged` in COMMIT_BATCH transactions
//!
//! The component rows measure each step *as if it had to do its own I/O*, so
//! they sum to more than the pipeline: the pipeline decodes once and hands the
//! same buffer to the thumbnail writer, the palette miner and the signature
//! (see `media::pipeline`). Read them as "what this would cost alone", and
//! compare the pipeline rows between builds.
//!
//! ## Duplicate imports
//!
//! The last section stages the same batch twice. The second pass finds
//! thumbnails in the cache, so the decode stage reads those instead of the
//! originals — the gap to a batch where nothing is cached is what a re-import
//! costs, and before the pipeline existed that pass still decoded every
//! original once.
//!
//! ## What the single decode is actually worth (measured)
//!
//! 30 files, 3000x2000 JPEGs, 8.7 MiB, interleaved A/B of two release builds
//! in one sitting (2026-09-18):
//!
//! | row                          | before | after  |
//! |------------------------------|--------|--------|
//! | `stage_all` (cold, per file) | 16.4 / 16.9 ms | 16.2 / 17.0 ms |
//! | `stage_all` (2nd pass, cached) | 1.12 / 1.04 ms | 0.90 / 0.91 ms |
//!
//! So: a **cold** import is unchanged within noise, and a cached re-import is
//! ~20 % cheaper. That is the honest size of this change for plain images —
//! the two decodes it removes are of a ≤512px JPEG, which costs a fraction of
//! a millisecond, not the ~120 ms the code comments used to quote (that number
//! belonged to the older shape where the palette decoded the *original*).
//!
//! Where it does pay off beyond that, for the record: camera RAW and
//! HEIF/AVIF, whose "header" read is itself a full decode — the decode stage
//! is the only remaining reader, so those go from two full decodes to one.
//! And the structural wins are not throughput at all: staging in windows
//! bounds memory and cancellation latency, the directory walk left the UI
//! thread, and a new pixel consumer is one stage instead of a fifth decode.
//!
//! ## Why medians
//!
//! The library filesystem is btrfs at high usage; a single round fights COW
//! rewrites of the same paths and drifts 2-3x. Every phase is run `rounds`
//! times and the median is reported.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use trove_core::media::import::{self, ImportStorage};
use trove_core::media::{blob, color, metadata, probe, search, thumb};
use trove_core::store::Store;

/// One phase of staging, timed across the whole batch.
#[derive(Clone, Copy)]
enum Step {
    CopyHash,
    Thumb,
    Mine,
    Colors,
    Phash,
    Pipeline,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::CopyHash => "copy+hash",
            Step::Thumb => "thumb",
            Step::Mine => "mine (colors from thumb)",
            Step::Colors => "  └ colors (old: full decode)",
            Step::Phash => "phash",
            Step::Pipeline => "pipeline (stage_source)",
        }
    }
}

const STEPS: [Step; 6] = [
    Step::CopyHash,
    Step::Thumb,
    Step::Mine,
    Step::Colors,
    Step::Phash,
    Step::Pipeline,
];

/// Run one step over one file, into `root`. Setup (hashing, thumbnail
/// creation) happens OUTSIDE the timed section: in the real pipeline those
/// are shared work measured by their own steps.
fn run_step(step: Step, path: &Path, root: &Path) -> Duration {
    // Setup, untimed.
    let sha = match step {
        Step::Thumb | Step::Mine => blob::hash_file(path).unwrap_or_default().0,
        _ => String::new(),
    };
    let kind = match step {
        Step::Thumb | Step::Mine => probe::probe("png").kind,
        _ => probe::probe("bin").kind,
    };
    // For Mine the thumbnail must exist before the timed read (as it does in
    // the real pipeline, where Step::Thumb created it).
    let thumb_path = match step {
        Step::Mine => thumb::ensure(root, &sha, kind, path),
        _ => None,
    };

    // Timed section.
    let t = Instant::now();
    match step {
        Step::CopyHash => {
            let _ = blob::stage(path, root, "bin");
        }
        Step::Thumb => {
            // regenerate (not ensure) so the exists-check cannot short-circuit.
            let _ = thumb::regenerate(root, &sha, kind, path);
        }
        Step::Mine => {
            // Colors from the thumbnail, EXIF from the original — the real
            // pipeline shape. The old behaviour (colors from the original)
            // is measured separately by Step::Colors.
            let color_source = thumb_path.as_deref().unwrap_or(path);
            let _ = metadata::mine(path, kind, color_source);
        }
        Step::Colors => {
            // Cost of the OLD behaviour: a full decode of the ORIGINAL just
            // to shrink it to 24x24 — the work the thumbnail path removes.
            let _ = color::dominant_colors(path);
        }
        Step::Phash => {
            let _ = search::VisualSignature::from_image(path);
        }
        Step::Pipeline => {
            // Everything the import job does to one file, as it does it:
            // hash → probe → one decode → thumbnail → metadata → signature.
            let _ = import::stage_source(root, &root.join("cache"), path, ImportStorage::Link);
        }
    }
    t.elapsed()
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// Time `stage_all` + the commit loop (the real pipeline, for a baseline).
fn full_pipeline(paths: &[PathBuf], root: &Path) -> (Duration, Duration) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();

    let t0 = Instant::now();
    let staged = import::stage_all(root, &root.join("cache"), paths, ImportStorage::Link);
    let stage = t0.elapsed();

    let store = Store::open(&root.join("library.db")).unwrap();
    let t1 = Instant::now();
    {
        let conn = store.conn();
        for chunk in staged.chunks(16) {
            let _ = conn.execute_batch("BEGIN");
            for item in chunk.iter().flatten() {
                let _ = import::commit_staged(conn, None, item);
            }
            let _ = conn.execute_batch("COMMIT");
        }
    }
    (stage, t1.elapsed())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir: PathBuf = args
        .next()
        .expect("usage: import_profile <src-dir> [n] [rounds]")
        .into();
    let limit: Option<usize> = args.next().and_then(|s| s.parse().ok());
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);

    let mut paths: Vec<PathBuf> = std::fs::read_dir(&src_dir)
        .expect("read src dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    if let Some(n) = limit {
        paths.truncate(n);
    }
    let n = paths.len();
    assert!(n > 0, "no files in {}", src_dir.display());

    let me = std::process::id();
    let base = std::env::current_dir().unwrap().join("target/tmp");
    let root = base.join(format!("import-profile-{me}"));
    let _ = std::fs::create_dir_all(&root);

    // Total bytes processed, for throughput columns.
    let bytes: u64 = paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    println!(
        "import_profile: {n} files, {:.1} MiB, {rounds} rounds",
        bytes as f64 / 1048576.0
    );
    println!(
        "stage pool = {} threads",
        import::stage_thread_count_for(&paths)
    );
    println!();

    // --- per-step attribution ------------------------------------------------
    // Warm-up round so the first (cold) pass does not skew the medians.
    for step in STEPS {
        for p in paths.iter().take(4) {
            let _ = run_step(step, p, &root);
        }
    }

    let mut per_step: Vec<(Step, Vec<f64>)> = STEPS
        .iter()
        .map(|s| (*s, Vec::with_capacity(rounds)))
        .collect();

    for _ in 0..rounds {
        for (step, samples) in per_step.iter_mut() {
            // Each step gets its own scratch root so a previous step's output
            // cannot short-circuit this one (thumb's exists-check in
            // particular), and files are re-read clean.
            let scratch = root.join(format!("step-{}", *step as usize));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch).unwrap();
            let mut total = Duration::ZERO;
            for p in &paths {
                total += run_step(*step, p, &scratch);
            }
            samples.push(total.as_secs_f64());
            let _ = std::fs::remove_dir_all(&scratch);
        }
    }

    println!(
        "{:<22} {:>10} {:>12} {:>12}",
        "step", "total ms", "ms/file", "MiB/s"
    );
    println!("{}", "-".repeat(60));
    let mut step_sum = 0.0_f64;
    for (step, samples) in &per_step {
        let med = median(samples.clone());
        // "colors" is a sub-step of "mine", and "pipeline" is every step at
        // once: neither belongs in the sum.
        let is_substep = matches!(step, Step::Colors | Step::Pipeline);
        if !is_substep {
            step_sum += med;
        }
        println!(
            "{:<22} {:>10.1} {:>12.2} {:>12.1}",
            step.label(),
            med * 1000.0,
            med * 1000.0 / n as f64,
            bytes as f64 / 1048576.0 / med.max(1e-9)
        );
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<22} {:>10.1} {:>12.2}",
        "sum of components",
        step_sum * 1000.0,
        step_sum * 1000.0 / n as f64
    );

    // --- real pipeline baseline ---------------------------------------------
    println!();
    let _ = full_pipeline(&paths, &root); // warm-up, discarded
    let mut stages = Vec::new();
    let mut commits = Vec::new();
    for _ in 0..rounds {
        let (s, c) = full_pipeline(&paths, &root);
        stages.push(s.as_secs_f64());
        commits.push(c.as_secs_f64());
    }
    let smed = median(stages);
    let cmed = median(commits);
    let total = smed + cmed;
    println!(
        "{:<22} {:>10.1} {:>12.2}",
        "stage_all (real)",
        smed * 1000.0,
        smed * 1000.0 / n as f64
    );
    println!(
        "{:<22} {:>10.1} {:>12.2}",
        "commit (real)",
        cmed * 1000.0,
        cmed * 1000.0 / n as f64
    );
    println!("{:<22} {:>10.1}", "TOTAL", total * 1000.0);
    println!(
        "stage {:.0}% / commit {:.0}%",
        100.0 * smed / total,
        100.0 * cmed / total
    );

    // --- duplicate-import cost ----------------------------------------------
    // Import the same batch twice. On the second pass the decode stage finds
    // the cached thumbnails and decodes *those* instead of the originals, so
    // what is left is the hash (unavoidable — it is the dedupe key), the
    // palette, the signature and a stat per file. This is the number to watch
    // when touching the decode stage: it used to pay a full decode per file
    // even when nothing else had to change.
    println!();
    println!("--- duplicate import (same files, second pass) ---");
    let dup_root = base.join(format!("import-profile-dup-{me}"));
    let _ = std::fs::remove_dir_all(&dup_root);
    std::fs::create_dir_all(&dup_root).unwrap();
    let mut dup_samples = Vec::new();
    for _ in 0..rounds {
        // First pass populates the store; only the second is timed.
        let _ = import::stage_all(
            &dup_root,
            &dup_root.join("cache"),
            &paths,
            ImportStorage::Link,
        );
        let t = Instant::now();
        let staged = import::stage_all(
            &dup_root,
            &dup_root.join("cache"),
            &paths,
            ImportStorage::Link,
        );
        dup_samples.push(t.elapsed().as_secs_f64());
        drop(staged);
    }
    let dmed = median(dup_samples);
    println!(
        "{:<22} {:>10.1} {:>12.2}",
        "stage_all (2nd pass)",
        dmed * 1000.0,
        dmed * 1000.0 / n as f64
    );
    println!(
        "vs first pass: {:.2}x  ({} if fully cached)",
        dmed / smed.max(1e-9),
        if dmed > smed * 0.5 {
            "NOT short-circuited"
        } else {
            "mostly cached"
        }
    );
    let _ = std::fs::remove_dir_all(&dup_root);

    // --- collection insert (add_asset cost, fairly batched) ------------------
    // Both arms run the identical batched-commit loop (COMMIT_BATCH
    // transactions); the ONLY difference is `into_collection`. An earlier
    // version committed the collection arm in autocommit mode, which measured
    // the fsync-per-statement tax (~7.8 ms/file on this btrfs), not
    // add_asset — the historic search benchmarks showed autocommit → batched
    // at 4.49 → 0.048 ms/row. Never compare autocommit against batched.
    println!();
    println!("--- into_collection: add_asset cost (both batched) ---");
    for (label, with_coll) in [
        ("commit, no collection", false),
        ("commit into collection", true),
    ] {
        let coll_root = base.join(format!("import-profile-coll-{me}-{}", with_coll as u8));
        let _ = std::fs::remove_dir_all(&coll_root);
        std::fs::create_dir_all(&coll_root).unwrap();
        let store = Store::open(&coll_root.join("library.db")).unwrap();
        let coll = if with_coll {
            Some(
                trove_core::store::collections::create(
                    store.conn(),
                    &trove_core::model::NewCollection {
                        parent_id: None,
                        name: "bench".to_string(),
                        position: 0,
                    },
                )
                .unwrap(),
            )
        } else {
            None
        };
        let cid = coll.map(|c| c.id);
        let mut samples = Vec::new();
        for _ in 0..rounds {
            let staged = import::stage_all(
                &coll_root,
                &coll_root.join("cache"),
                &paths,
                ImportStorage::Link,
            );
            let t = Instant::now();
            {
                let conn = store.conn();
                for chunk in staged.chunks(16) {
                    let _ = conn.execute_batch("BEGIN");
                    for item in chunk.iter().flatten() {
                        let _ = import::commit_staged(conn, cid, item);
                    }
                    let _ = conn.execute_batch("COMMIT");
                }
            }
            samples.push(t.elapsed().as_secs_f64());
            // Empty the store so the next round starts fresh.
            let _ = conn_wipe(&store);
        }
        let med = median(samples);
        println!(
            "{:<26} {:>10.1} {:>12.2}",
            label,
            med * 1000.0,
            med * 1000.0 / n as f64
        );
        let _ = std::fs::remove_dir_all(&coll_root);
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// Delete every asset row so a collection-cost round starts fresh.
fn conn_wipe(store: &Store) -> rusqlite::Result<()> {
    let conn = store.conn();
    conn.execute_batch(
        "DELETE FROM asset_collection; DELETE FROM assets; DELETE FROM search_queue;
         DELETE FROM collections;",
    )
}
