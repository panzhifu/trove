//! Import pipeline benchmark: measures staging (I/O-bound, bounded pool) vs
//! commit (serial, batched) so optimization work targets the real bottleneck.
//!
//! Run with a directory of source files:
//!   cargo run --release -p trove-core --example import_bench -- <src-dir> [n] [rounds]
//!
//! ## Why it reports a median
//!
//! This machine's library filesystem is btrfs at ~80% usage. A single round
//! is useless as a measurement: the first round runs against a cold-ish
//! filesystem and later rounds fight COW rewrites of the same paths, so runs
//! drift by 2-3x. The harness therefore does one warm-up round (discarded)
//! then `rounds` timed rounds, and prints the median alongside min/max so the
//! spread is visible. Compare medians, never single runs — and always A/B two
//! binaries interleaved in one sitting, because the disk's condition changes
//! over minutes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use trove_core::media::import::{self, ImportStorage};
use trove_core::store::Store;

/// One measured round: stage every file into a fresh root, then commit.
fn one_round(paths: &[PathBuf], root: &PathBuf, stage_only: bool) -> (Duration, Duration) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();

    let t0 = Instant::now();
    let staged = import::stage_all(root, &root.join("cache"), paths, ImportStorage::Link, &std::sync::atomic::AtomicBool::new(false));
    let stage_time = t0.elapsed();

    if stage_only {
        return (stage_time, Duration::ZERO);
    }

    let store = Store::open(&root.join("library.db")).unwrap();
    let t1 = Instant::now();
    {
        let conn = store.conn();
        // Mirror tasks/import.rs: COMMIT_BATCH = 16, savepoint per file.
        for chunk in staged.chunks(16) {
            let _ = conn.execute_batch("BEGIN");
            for file in chunk.iter().flatten() {
                let _ = import::commit_staged(conn, None, file);
            }
            let _ = conn.execute_batch("COMMIT");
        }
    }
    (stage_time, t1.elapsed())
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

fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir: PathBuf = args
        .next()
        .expect("usage: import_bench <src-dir> [n] [rounds]")
        .into();
    let limit: Option<usize> = args.next().and_then(|s| s.parse().ok());
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

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

    let me = std::process::id();
    let root = std::env::current_dir()
        .unwrap()
        .join("target/tmp")
        .join(format!("import-bench-{me}"));

    // Warm-up: lets the filesystem settle before the timed rounds.
    let _ = one_round(&paths, &root, false);

    let mut stages = Vec::new();
    let mut commits = Vec::new();
    for _ in 0..rounds {
        let (s, c) = one_round(&paths, &root, false);
        stages.push(s.as_secs_f64());
        commits.push(c.as_secs_f64());
    }

    let smed = median(stages.clone());
    let cmed = median(commits.clone());
    let total = smed + cmed;

    println!(
        "{} files, {} rounds (+1 warm-up), stage pool = {} threads",
        paths.len(),
        rounds,
        trove_core::media::import::stage_thread_count_for(&paths)
    );
    println!(
        "  stage  median {:>7.3} s   min {:>7.3}  max {:>7.3}   ({:>6.1} files/s)",
        smed,
        stages.iter().cloned().fold(f64::INFINITY, f64::min),
        stages.iter().cloned().fold(0.0_f64, f64::max),
        paths.len() as f64 / smed
    );
    println!(
        "  commit median {:>7.3} s   min {:>7.3}  max {:>7.3}   ({:>6.1} files/s)",
        cmed,
        commits.iter().cloned().fold(f64::INFINITY, f64::min),
        commits.iter().cloned().fold(0.0_f64, f64::max),
        paths.len() as f64 / cmed
    );
    println!(
        "  TOTAL  {:.3} s   (stage {:.0}% / commit {:.0}%)",
        total,
        100.0 * smed / total,
        100.0 * cmed / total
    );

    let _ = std::fs::remove_dir_all(&root);
}
