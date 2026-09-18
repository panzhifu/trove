//! One cold `stage_all` per pass, at whatever width `TROVE_STAGE_THREADS` asks
//! for — the arm runner for a pool-width sweep.
//!
//! The staging pool is built once per process, so a sweep is process-per-
//! sample; interleave the arms rather than running a width to completion,
//! because the library filesystem drifts several-fold between sittings.
//!
//!   for r in 1 2 3 4 5; do for w in 1 2 4 6 8 12; do
//!     TROVE_STAGE_THREADS=$w target/release/examples/stage_sweep <src> 30 3
//!   done; done
//!
//! `passes` runs inside the process, each with an emptied cache, and each is
//! printed: the first pass of a process used to be several times slower than
//! the rest, and a single sample per process cannot tell that apart from a
//! slow filesystem.
//!
//! Output, one line per pass: `width=<n> pass=<i> <ms/file> <ms> failed=<n>`
//!
//! `width` is the width the process actually used, so a run with the sweep
//! variable **unset** doubles as a check that the arm a batch's average source
//! size implies is the arm staging chose.

use std::path::PathBuf;
use std::time::Instant;

use trove_core::media::import::{self, ImportStorage};

fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir: PathBuf = args
        .next()
        .expect("usage: stage_sweep <src-dir> [n] [passes]")
        .into();
    let limit: Option<usize> = args.next().and_then(|s| s.parse().ok());
    let passes: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);

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

    // A fixed root, wiped up front: a fresh directory name per process made
    // every sample pay first-touch costs the real import does not.
    let root = std::env::current_dir()
        .unwrap()
        .join("target/tmp")
        .join("stage-sweep");
    let _ = std::fs::remove_dir_all(&root);
    // The width this process will really use: the pinned `TROVE_STAGE_THREADS`
    // when set (the sweep's arms), otherwise whatever the adaptive choice makes
    // of this batch's average source size.
    let width = import::stage_thread_count_for(&paths);

    for pass in 0..passes {
        // Cold cache: a warm thumbnail cache measures the cached path, which is
        // not what pool width is here to speed up.
        let cache = root.join("cache");
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        let t = Instant::now();
        let staged = import::stage_all(&root.join("data"), &cache, &paths, ImportStorage::Link);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let failed = staged.iter().filter(|r| r.is_err()).count();
        println!(
            "width={width} pass={pass} {:.3} {:.1} failed={failed}",
            ms / n as f64,
            ms
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
