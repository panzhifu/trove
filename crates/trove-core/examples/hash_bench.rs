//! Hash-stage benchmark: what each tier of `media::pipeline::HashStage` costs,
//! measured through the real pipeline on sources where the read dominates.
//!
//!   cargo run --release -p trove-core --example hash_bench [mib ...]
//!
//! ## Why a non-image
//!
//! The three tiers differ in *how much of the file they read*, so the file has
//! to be one where reading it is the cost. A photograph is the wrong shape:
//! its decode dwarfs its hash (see `import_profile`, where a 3000x2000 JPEG
//! spends ~35 ms in the thumbnail and ~2 ms being read), and the tier
//! difference disappears into that. A large `.bin` is a `Document`: the probe
//! is a name lookup, the decode stage is a no-op, there is no thumbnail — so
//! what is left on the clock is the hash stage.
//!
//! ## The rows, and what they say about a re-import
//!
//! | row | tier |
//! |---|---|
//! | `read` — `blob::hash_file` alone | 3 — one full read, all cores past 4 MiB |
//! | `cold` — the pipeline with the cache cleared | 3, plus the stages either side |
//! | `warm` — the pipeline with the file remembered | 1 — `stat` + lookup, nothing read |
//! | `sample` — `hash::fingerprint` | 2 — three blocks |
//!
//! `cold - warm` is what the hash cache is worth per file, and **`cold` is
//! also what the previous build paid on every pass**, because it had no memory
//! of what it had already read: a watched folder that re-offered a file, or a
//! folder dropped twice, cost a full read each time. `sample` is the price of
//! the cheap check the hash stage takes before deciding that a large source
//! needs reading at all — only offered past `SAMPLE_MIN_BYTES`.
//!
//! Files are created once, untimed: *creating* a file is what this sandbox
//! serialises, not reading one, which is why nothing here times a write.

use std::path::Path;
use std::time::{Duration, Instant};

use trove_core::media::import::{self, ImportStorage};
use trove_core::media::{blob, hash, hash_cache};

/// Sizes to measure, in MiB.
const DEFAULT_SIZES: [u64; 3] = [8, 64, 256];

/// Rounds per row; the best is reported, so one round losing the CPU to
/// something else does not become the number.
const ROUNDS: u32 = 3;

fn main() {
    let sizes: Vec<u64> = {
        let from_args: Vec<u64> = std::env::args()
            .skip(1)
            .filter_map(|a| a.parse::<u64>().ok())
            .collect();
        if from_args.is_empty() {
            DEFAULT_SIZES.to_vec()
        } else {
            from_args
        }
    };

    let base = std::env::current_dir()
        .unwrap()
        .join("target/tmp/hash-bench");
    let _ = std::fs::create_dir_all(&base);
    let root = base.join("library");
    let cache = base.join("cache");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&cache).unwrap();

    println!("hash_bench: {} sizes, best of {ROUNDS}", sizes.len());
    println!(
        "tiers: read = full read · cold = pipeline, cache cleared · warm = pipeline, remembered · sample = cheap check"
    );
    println!();
    println!(
        "{:>6}  {:>9}  {:>9}  {:>9}  {:>9}  {:>9}  {:>8}  {:>9}",
        "size", "read ms", "cold ms", "warm ms", "sample ms", "read MiB/s", "cold/warm", "saved ms"
    );

    for mib in sizes {
        let path = base.join(format!("source-{mib}mib.bin"));
        if !path.is_file() {
            write_payload(&path, mib * 1024 * 1024);
        }

        // Tier 3, on its own: the read and the hash, nothing else.
        let read = time_best(ROUNDS, || {
            let _ = blob::hash_file(&path).unwrap();
        });

        // Tier 3, inside the pipeline. The cache is emptied *outside* the
        // clock each round, so what is timed is the pass that has to read.
        let mut cold = Duration::MAX;
        for _ in 0..ROUNDS {
            hash_cache::clear(&cache);
            let start = Instant::now();
            import::stage_source(&root, &cache, &path, ImportStorage::Link).unwrap();
            cold = cold.min(start.elapsed());
        }

        // Tier 1: the file is remembered from the pass above.
        let warm = time_best(ROUNDS, || {
            import::stage_source(&root, &cache, &path, ImportStorage::Link).unwrap();
        });

        // Tier 2: the sample, which is only offered past the threshold.
        let sample = time_best(ROUNDS, || {
            let _ = hash::fingerprint(&path).unwrap();
        });

        println!(
            "{mib:>4}Mi  {:>9.2}  {:>9.2}  {:>9.3}  {:>9.3}  {:>9.0}  {:>7.1}x  {:>9.2}",
            read.as_secs_f64() * 1000.0,
            cold.as_secs_f64() * 1000.0,
            warm.as_secs_f64() * 1000.0,
            sample.as_secs_f64() * 1000.0,
            mib as f64 / read.as_secs_f64().max(1e-9),
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-9),
            (cold - warm).as_secs_f64() * 1000.0,
        );
    }

    println!();
    println!(
        "cold = what the previous build paid on every pass; warm = what this one pays on a re-import"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Time `f` `rounds` times and keep the fastest. `f` must be side-effect free
/// in the ways that matter to the measurement.
fn time_best(rounds: u32, mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..rounds {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

/// Write `len` bytes of incompressible-enough data, once.
fn write_payload(path: &Path, len: u64) {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).unwrap();
    let mut block = vec![0u8; 1 << 20];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut left = len;
    while left > 0 {
        for chunk in block.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        let take = left.min(block.len() as u64) as usize;
        file.write_all(&block[..take]).unwrap();
        left -= take as u64;
    }
    file.flush().unwrap();
}
