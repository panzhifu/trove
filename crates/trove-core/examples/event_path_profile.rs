//! Task-event path profiler: what the `TaskManager`'s locks cost per import,
//! and what one watcher's poll costs — now that the queue is a bucket per job
//! and a poll takes only its own bucket.
//!
//! Run:
//!   cargo run --release -p trove-core --example event_path_profile [calls] [polls] [per_file_ms]
//!
//! ## What is measured
//!
//! Progress reports take the registry lock, then the event-queue lock — both
//! of which the UI thread also takes when it drains events
//! (`TaskManager::poll_events_for`) and reads the job registry. The import job
//! reports once per file, so the question is whether those locks are a
//! throughput limiter — if they are not, a per-job channel / events-channel
//! rewrite cannot pay for itself on speed.
//!
//! A. `progress()`, one writer, uncontended — the per-file cost the import job
//!    actually pays.
//! B. `poll_events_for()` on an empty bucket — the per-poll cost the UI pays.
//! B2. The same poll with four *other* jobs' full buckets in the queue — the
//!     case the buckets were added for: a watcher's cost must not follow how
//!     much backlog someone else left.
//! C. Four writers hammering the event lock while this thread polls — the
//!    *worst case* the refactor is meant to protect against (every staging
//!    thread reporting independently), reported as poll latency percentiles
//!    plus the writers' aggregate call rate.
//! D. The share: (A) against a real per-file import cost.
//! E. Tiny files: the shape where the per-file work is smallest, so the event
//!    path is proportionally largest — the only case where the locks could
//!    plausibly show up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use trove_core::media::import::{self, ImportStorage};
use trove_core::tasks::{TaskKind, TaskManager};

/// A 1x1 PNG. Trailing bytes vary per file so nothing dedupes by content.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn ns(d: Duration) -> f64 {
    d.as_secs_f64() * 1e9
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let calls: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let polls: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let per_file_ms: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(16.4);

    println!("event_path_profile: {calls} progress calls, {polls} polls");
    println!("(reference per-file import cost: {per_file_ms:.1} ms)");
    println!(
        "(one TaskEvent is {} B)",
        std::mem::size_of::<trove_core::tasks::TaskEvent>()
    );
    println!();

    // --- A: one writer, uncontended -----------------------------------------
    let manager = TaskManager::new();
    let (probe_id, rx) = manager
        .start(TaskKind::Import, "probe", move |ctx| {
            for _ in 0..10_000 {
                ctx.progress(0, calls);
            }
            let t = Instant::now();
            for i in 0..calls {
                ctx.progress(i, calls);
            }
            Ok(t.elapsed())
        })
        .expect("start probe job");
    let a = rx.recv().expect("probe value");
    let per_call = ns(a) / calls as f64;
    println!("A progress(), 1 writer      : {per_call:>7.1} ns/call   ({a:?} / {calls})");
    println!(
        "  → as a share of one file    : {:.5} %  of {per_file_ms:.1} ms",
        per_call / 1e6 / per_file_ms * 100.0
    );

    // --- B: an idle poll of one's own bucket ---------------------------------
    manager.poll_events_for(probe_id); // take what job A left behind
    let t = Instant::now();
    for _ in 0..polls {
        std::hint::black_box(manager.poll_events_for(probe_id));
    }
    let b = t.elapsed();
    println!(
        "B poll_events_for(), idle     : {:>7.1} ns/call",
        ns(b) / polls as f64
    );

    // --- B2: the same poll, with four foreign backlogs in the queue ----------
    // The shared queue made a watcher scan every job's backlog to find its own
    // events; the buckets should make this line read the same as B's.
    let mut releases = Vec::new();
    let mut fillers = Vec::new();
    for kind in [
        TaskKind::CollectInbox,
        TaskKind::Maintenance,
        TaskKind::ModelPreview,
        TaskKind::VideoDecode,
    ] {
        // One channel per job: `recv` wakes a single waiter, and every filler
        // has to be let go for its thread to end.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        releases.push(release_tx);
        let (_, rx) = manager
            .start(kind, "backlog", move |ctx| {
                for i in 0..1_000 {
                    ctx.set_total(i); // an unthrottled event per call
                }
                let _ = release_rx.recv(); // hold the bucket until measured
                Ok(())
            })
            .expect("start backlog job");
        fillers.push(rx);
    }
    // Give the four fillers time to publish. Nothing polls their buckets here:
    // taking them out would be taking them *out of the queue*, and the point is
    // to poll a live backlog.
    std::thread::sleep(Duration::from_millis(200));

    let t = Instant::now();
    for _ in 0..polls {
        std::hint::black_box(manager.poll_events_for(probe_id));
    }
    let b2 = t.elapsed();
    println!(
        "B2 same poll, 4 foreign buckets full: {:>7.1} ns/call",
        ns(b2) / polls as f64
    );
    for release_tx in releases {
        let _ = release_tx.send(()); // let the fillers finish
    }
    for rx in fillers {
        let _ = rx.recv();
    }

    // --- C: four writers, this thread polls ---------------------------------
    let stop = Arc::new(AtomicBool::new(false));
    let mut rxs = Vec::new();
    for kind in [
        TaskKind::Import,
        TaskKind::CollectInbox,
        TaskKind::Maintenance,
        TaskKind::ModelPreview,
    ] {
        let stop = stop.clone();
        let (_, rx) = manager
            .start(kind, "hammer", move |ctx| {
                let t = Instant::now();
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    ctx.progress(i, u64::MAX);
                    i += 1;
                }
                Ok((i, t.elapsed()))
            })
            .expect("start hammer job");
        rxs.push(rx);
    }

    let mut lat = Vec::with_capacity(polls);
    for _ in 0..polls {
        let t = Instant::now();
        std::hint::black_box(manager.poll_events_for(probe_id));
        lat.push(t.elapsed());
    }
    stop.store(true, Ordering::Relaxed);

    let mut total_calls = 0u64;
    let mut writers_wall = Duration::ZERO;
    for rx in rxs {
        let (n, d) = rx.recv().expect("hammer value");
        total_calls += n;
        writers_wall = writers_wall.max(d);
    }
    lat.sort();
    println!(
        "C poll_events_for under 4 writers: p50 {:>7.1} ns   p99 {:>8.1} ns   max {:>8.1} ns",
        ns(pct(&lat, 0.50)),
        ns(pct(&lat, 0.99)),
        ns(lat[lat.len() - 1])
    );
    println!(
        "  writers (4 threads)        : {:.1} M calls/s ({:.1} ns/call aggregate)",
        total_calls as f64 / writers_wall.as_secs_f64() / 1e6,
        ns(writers_wall) / total_calls as f64
    );

    // --- D/E: tiny files, where per-file work is smallest --------------------
    let base = std::env::current_dir().unwrap().join("target/tmp");
    let root = base.join("event-probe-tiny");
    let src = root.join("src");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&src).expect("make tiny src");

    let n = 2000usize;
    let mut paths = Vec::with_capacity(n);
    for i in 0..n {
        let p = src.join(format!("t{i:04}.png"));
        let mut bytes = PNG_1X1.to_vec();
        bytes.extend_from_slice(&(i as u32).to_le_bytes());
        std::fs::write(&p, &bytes).expect("write tiny png");
        paths.push(p);
    }

    let t = Instant::now();
    let staged = import::stage_all(
        &root,
        &root.join("cache"),
        &paths,
        ImportStorage::Link,
        &std::sync::atomic::AtomicBool::new(false),
    );
    let d = t.elapsed();
    let failed = staged.iter().filter(|r| r.is_err()).count();
    let per_file = d.as_secs_f64() * 1e3 / n as f64;
    println!();
    println!(
        "D tiny files ({n} x {} B)     : stage_all {per_file:>7.3} ms/file  ({failed} failed, pool {} threads)",
        PNG_1X1.len() + 4,
        import::stage_thread_count_for(&paths)
    );
    println!(
        "  → event path as a share     : {:.5} %  of that budget",
        per_call / 1e6 / per_file * 100.0
    );

    let _ = std::fs::remove_dir_all(&root);
}
