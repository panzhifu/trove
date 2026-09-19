//! Process-wide metrics: lock-free counters and gauges for the handful of
//! things worth watching while Trove runs — import throughput, the search
//! outbox's backlog, thumbnail-cache efficiency, slow queries.
//!
//! Deliberately not a metrics *stack*: no exporter, no scrape schedule, no
//! new dependency. The registry has two consumers — the collect service's
//! `GET /health`, which serves [`snapshot`] as JSON to scripts and curl, and
//! the slow-event warn logs, which fire off these thresholds without anyone
//! scraping. Everything is a static atomic, so instrumentation is a relaxed
//! add on a hot path (the thumbnail cache probes one on every re-import)
//! and a snapshot is a dozen loads.
//!
//! Counters only ever go up and are lost at process exit, which is the
//! right shape for a desktop session: a `/health` scrape answers "is this
//! install healthy right now", not long-term analytics.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

// -- import -----------------------------------------------------------------

static IMPORT_RUNS: AtomicU64 = AtomicU64::new(0);
static IMPORT_ASSETS: AtomicU64 = AtomicU64::new(0);
static IMPORT_SKIPPED: AtomicU64 = AtomicU64::new(0);
static IMPORT_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Duration of the last settled run, in whole milliseconds.
static IMPORT_LAST_MS: AtomicU64 = AtomicU64::new(0);
/// Assets the last settled run imported; with [`IMPORT_LAST_MS`] this is the
/// import speed a scrape renders as assets/s.
static IMPORT_LAST_ASSETS: AtomicU64 = AtomicU64::new(0);

// -- search outbox ----------------------------------------------------------

static OUTBOX_DRAINED_ROWS: AtomicU64 = AtomicU64::new(0);
/// Rows the most recent non-empty drain consumed — the closest thing to a
/// live queue depth: the drain's contract is to empty the queue, so a
/// non-zero `last_rows` says how big the last burst of writes was.
static OUTBOX_LAST_ROWS: AtomicU64 = AtomicU64::new(0);
/// How long that drain took, in whole milliseconds.
static OUTBOX_LAST_MS: AtomicU64 = AtomicU64::new(0);
static OUTBOX_SLOW_DRAINS: AtomicU64 = AtomicU64::new(0);

// -- thumbnail cache --------------------------------------------------------

static THUMB_HITS: AtomicU64 = AtomicU64::new(0);
static THUMB_MISSES: AtomicU64 = AtomicU64::new(0);

// -- queries ----------------------------------------------------------------

static QUERIES_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERIES_SLOW: AtomicU64 = AtomicU64::new(0);

// -- library ----------------------------------------------------------------

static LIBRARY_OPEN: AtomicU64 = AtomicU64::new(0);
static LIBRARY_ASSETS: AtomicU64 = AtomicU64::new(0);

/// Process start, anchored at the first instrumented event — in practice the
/// library opening during app startup.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// A paged asset query that ran longer than this logs a warn — the app's
/// slow-query log. Generous on purpose: the store is a local SQLite file,
/// so a normal browse sits in the single-digit milliseconds and half a
/// second means something (a lock, a pathological filter) genuinely stalled.
pub const SLOW_QUERY: Duration = Duration::from_millis(500);

/// Record a settled import run. `failed` marks a run whose batches started
/// erroring (`ImportOutcome::error`), not one that merely skipped files.
pub fn note_import_run(assets: usize, skipped: usize, failed: bool, duration: Duration) {
    IMPORT_RUNS.fetch_add(1, Ordering::Relaxed);
    IMPORT_ASSETS.fetch_add(assets as u64, Ordering::Relaxed);
    IMPORT_SKIPPED.fetch_add(skipped as u64, Ordering::Relaxed);
    if failed {
        IMPORT_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    IMPORT_LAST_MS.store(duration.as_millis() as u64, Ordering::Relaxed);
    IMPORT_LAST_ASSETS.store(assets as u64, Ordering::Relaxed);
}

/// Record one outbox drain pass that consumed `rows` rows. `slow` mirrors
/// the drain's own warn threshold; the counter lets a scrape see how often
/// the index struggled without reading logs.
pub fn note_drain(rows: u64, duration: Duration, slow: bool) {
    OUTBOX_DRAINED_ROWS.fetch_add(rows, Ordering::Relaxed);
    OUTBOX_LAST_ROWS.store(rows, Ordering::Relaxed);
    OUTBOX_LAST_MS.store(duration.as_millis() as u64, Ordering::Relaxed);
    if slow {
        OUTBOX_SLOW_DRAINS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record a thumbnail-cache probe that found a usable entry.
pub fn note_thumb_hit() {
    THUMB_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Record a thumbnail-cache probe that had to decode (or fail) instead.
pub fn note_thumb_miss() {
    THUMB_MISSES.fetch_add(1, Ordering::Relaxed);
}

/// Record a finished paged asset query. Past [`SLOW_QUERY`] it counts and
/// logs as slow — the warn is the slow-query log's payload.
pub fn note_query(duration: Duration) {
    QUERIES_TOTAL.fetch_add(1, Ordering::Relaxed);
    if duration >= SLOW_QUERY {
        QUERIES_SLOW.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(elapsed_ms = duration.as_millis() as u64, "slow asset query");
    }
}

/// Record that a library is now open, and how many assets it holds.
pub fn set_library_open(assets: u64) {
    LIBRARY_OPEN.store(1, Ordering::Relaxed);
    LIBRARY_ASSETS.store(assets, Ordering::Relaxed);
}

/// The registry as plain JSON-ready data — the `/health` body.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    version: &'static str,
    uptime_secs: u64,
    library: LibraryHealth,
    import: ImportHealth,
    outbox: OutboxHealth,
    thumb_cache: ThumbCacheHealth,
    queries: QueryHealth,
}

#[derive(Debug, Serialize)]
pub struct LibraryHealth {
    open: bool,
    assets: u64,
}

#[derive(Debug, Serialize)]
pub struct ImportHealth {
    runs_total: u64,
    assets_total: u64,
    skipped_total: u64,
    failures_total: u64,
    /// Speed of the last settled run, when it both imported something and
    /// lasted long enough for the division to mean anything.
    last_assets_per_sec: Option<f64>,
    last_duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct OutboxHealth {
    drained_rows_total: u64,
    last_rows: u64,
    last_duration_ms: u64,
    slow_drains_total: u64,
}

#[derive(Debug, Serialize)]
pub struct ThumbCacheHealth {
    hits_total: u64,
    misses_total: u64,
    /// Hits over probes, when any probe has happened.
    hit_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct QueryHealth {
    total: u64,
    slow_total: u64,
    slow_threshold_ms: u64,
}

/// Hits over probes, when any probe has happened.
fn hit_rate(hits: u64, misses: u64) -> Option<f64> {
    (hits > 0 || misses > 0).then(|| hits as f64 / (hits + misses) as f64)
}

/// Import speed of a run, when it both imported something and lasted long
/// enough for the division to mean anything.
fn assets_per_sec(assets: u64, ms: u64) -> Option<f64> {
    (ms > 0).then(|| assets as f64 / (ms as f64 / 1000.0))
}

/// Read every metric at once. Cheap enough to scrape per request.
pub fn snapshot() -> Snapshot {
    let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);

    let last_ms = load(&IMPORT_LAST_MS);
    let last_assets = load(&IMPORT_LAST_ASSETS);
    let hits = load(&THUMB_HITS);
    let misses = load(&THUMB_MISSES);

    Snapshot {
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: STARTED.get_or_init(Instant::now).elapsed().as_secs(),
        library: LibraryHealth {
            open: load(&LIBRARY_OPEN) == 1,
            assets: load(&LIBRARY_ASSETS),
        },
        import: ImportHealth {
            runs_total: load(&IMPORT_RUNS),
            assets_total: load(&IMPORT_ASSETS),
            skipped_total: load(&IMPORT_SKIPPED),
            failures_total: load(&IMPORT_FAILURES),
            last_assets_per_sec: assets_per_sec(last_assets, last_ms),
            last_duration_ms: last_ms,
        },
        outbox: OutboxHealth {
            drained_rows_total: load(&OUTBOX_DRAINED_ROWS),
            last_rows: load(&OUTBOX_LAST_ROWS),
            last_duration_ms: load(&OUTBOX_LAST_MS),
            slow_drains_total: load(&OUTBOX_SLOW_DRAINS),
        },
        thumb_cache: ThumbCacheHealth {
            hits_total: hits,
            misses_total: misses,
            hit_rate: hit_rate(hits, misses),
        },
        queries: QueryHealth {
            total: load(&QUERIES_TOTAL),
            slow_total: load(&QUERIES_SLOW),
            slow_threshold_ms: SLOW_QUERY.as_millis() as u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global and other tests' imports, drains and
    /// cache probes run concurrently with this one, so assertions stick to
    /// deltas that only ever grow (monotonic counters) — never exact gauge
    /// reads, which a concurrent note can overwrite.
    #[test]
    fn notes_move_the_snapshot() {
        let before = snapshot();
        note_import_run(120, 3, true, Duration::from_millis(600));
        note_thumb_hit();
        note_thumb_miss();
        note_drain(4_000, Duration::from_millis(90), false);
        note_query(Duration::from_millis(700));
        let after = snapshot();

        assert!(
            after.import.runs_total > before.import.runs_total,
            "one more run"
        );
        assert!(after.import.assets_total >= before.import.assets_total + 120);
        assert!(after.import.skipped_total >= before.import.skipped_total + 3);
        assert!(
            after.import.failures_total > before.import.failures_total,
            "the run carried an error"
        );
        assert!(after.import.last_assets_per_sec.is_some(), "600 ms > 0");
        assert!(after.outbox.drained_rows_total >= before.outbox.drained_rows_total + 4_000);
        assert!(after.queries.slow_total > before.queries.slow_total);
        assert_eq!(
            after.queries.slow_threshold_ms,
            SLOW_QUERY.as_millis() as u64
        );
        assert!(after.uptime_secs < 60, "test process just started");
    }

    /// The derived numbers are pure math, tested where no concurrent test
    /// can move them underneath the assertion.
    #[test]
    fn derived_rates() {
        let rate = hit_rate(1, 2).expect("probes happened");
        assert!((rate - 1.0 / 3.0).abs() < 1e-9, "{rate} ≈ 1/3");
        assert_eq!(hit_rate(0, 0), None, "no probes yet");
        assert_eq!(hit_rate(3, 0), Some(1.0));

        let speed = assets_per_sec(120, 600).expect("600 ms > 0");
        assert!((speed - 200.0).abs() < 1.0, "{speed} ≈ 200 assets/s");
        assert_eq!(assets_per_sec(120, 0), None, "a zero-length run has none");
    }

    /// A snapshot is valid JSON with the headline fields present.
    #[test]
    fn snapshot_serializes() {
        let json = serde_json::to_string(&snapshot()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert!(value["library"]["assets"].is_u64());
        assert!(
            value["import"]["last_assets_per_sec"].is_null()
                || value["import"]["last_assets_per_sec"].is_number()
        );
        assert!(
            value["thumb_cache"]["hit_rate"].is_null()
                || value["thumb_cache"]["hit_rate"].is_number()
        );
    }
}
