//! The folder-watch resident job: watches the collect-service inbox and the
//! configured watch roots, reporting discoveries over a channel so the UI
//! layer can start imports.
//!
//! The job never starts imports itself — that is the embedder's call, so
//! progress toasts and controller state stay in one place. The channel
//! closes when the job exits (cancelled or panicked); the embedder treats a
//! closed channel as "watch stopped".
//!
//! Two sources feed the same report:
//!
//! - kernel events through [`notify`], which is what makes a dropped folder
//!   show up within a tick instead of within [`WATCH_INTERVAL`];
//! - a periodic full sweep, kept because events are not always available: a
//!   network or fuse mount has none to offer, the kernel queue can overflow
//!   and drop them silently, and a root that only came into existence after
//!   the watcher was attached would otherwise never be seen.
//!
//! Scan semantics: the inbox is signalled whenever files are waiting; watch
//! roots are baselined on first sight (attaching a watch never retro-imports
//! what is already there) and only *new* files are signalled. A file is only
//! offered once it has stopped changing — see [`SETTLE`]. The embedder marks
//! acceptance — a refused import (another import still running) must retry on
//! a later sweep, so the job keeps re-signalling files it has not been told to
//! forget. In practice the embedder accepts everything and the
//! one-import-at-a-time rule in [`super::TaskManager`] makes the next sweep a
//! no-op retry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use notify::{RecursiveMode, Watcher};

use super::JobContext;
use crate::config::{AppConfig, LibraryConfig};

/// Full-sweep cadence for the watch roots, and the interval the two configs
/// are re-read at, so settings changes apply without a restart. It is now the
/// *fallback* cadence: a file the kernel reports is picked up in [`TICK`].
pub const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// How often the event buffer is drained. Short enough that a drop feels
/// immediate, long enough that a burst (a folder copy) arrives as one batch.
const TICK: Duration = Duration::from_millis(400);

/// How much longer the full sweep may wait once every root is actually watched
/// (12 × [`WATCH_INTERVAL`] = a minute): the kernel covers the fast path, so the
/// sweep is only there for what it drops.
const SWEEP_COVERED_FACTOR: u32 = 12;

/// How long a file must have gone untouched before it is offered.
///
/// The kernel reports a create the moment the writer opens the file, so
/// without this an editor saving a large document — or a browser still
/// downloading — would be imported half-written. The library *links* its files
/// and records the hash it read, so that mistake cannot be repaired later.
const SETTLE: Duration = Duration::from_millis(1500);

/// Something discovered on a sweep that the embedder should act on.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchSignal {
    /// The collect-service inbox has files waiting.
    Inbox,
    /// New files appeared under the watched roots.
    Files(Vec<PathBuf>),
}

/// Run the resident watch loop until cancelled. Sends every discovery to
/// `signals`; sleeps in small cancellable slices between ticks.
///
/// `library_dir` is the open library's data directory: its `library.json`
/// holds the watched-root list, which belongs to that library rather than to
/// the application.
pub fn run(
    interval: Duration,
    library_dir: std::path::PathBuf,
    inbox_dir: std::path::PathBuf,
    signals: Sender<WatchSignal>,
    ctx: &JobContext,
) -> Result<(), String> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut baselined: HashSet<PathBuf> = HashSet::new();
    let mut events: Option<Events> = None;
    // Paths already offered, with the tick they were offered on. A path is
    // re-offered at most once per full sweep, so the fast path does not
    // multiply the embedder's retries (and the re-hashing behind them) by the
    // tick rate.
    let mut recent: HashMap<PathBuf, Instant> = HashMap::new();
    let mut next_settings = Instant::now();
    let mut next_sweep = Instant::now();
    let mut config = AppConfig::load();
    let mut library = LibraryConfig::load(&library_dir);
    let mut roots: Vec<PathBuf> = library.watched_folders.clone();

    loop {
        if ctx.cancelled() {
            return Ok(());
        }
        let now = Instant::now();

        // Settings are re-read on the sweep cadence, not on every tick: a tick
        // is [`TICK`] long and each load is two file reads plus a parse, so
        // re-reading them per tick would put that cost on the idle path where
        // nothing has happened. This stays at `interval`, so a folder added in
        // Settings starts being watched as promptly as before.
        let settings_due = now >= next_settings;
        if settings_due {
            next_settings = now + interval;
            config = AppConfig::load();
            library = LibraryConfig::load(&library_dir);
            roots = library.watched_folders.clone();
        }

        // The inbox is cheap to list and is what the browser extension feeds, so
        // it keeps the tick cadence.
        if config.collect_enabled()
            && !crate::services::collect::inbox_items_in(&inbox_dir).is_empty()
            && signals.send(WatchSignal::Inbox).is_err()
        {
            return Ok(()); // embedder hung up; stop watching
        }

        if library.watch_folders_enabled() {
            // A watcher that could not be started is retried on the settings
            // cadence rather than every tick: a mount can appear later, but a
            // platform with no backend must not cost a syscall storm.
            if events.is_none() && settings_due {
                events = Events::start(&roots);
            }

            let mut fresh = Vec::new();
            if let Some(watch) = events.as_mut() {
                watch.sync(&roots);
                fresh = watch.drain(&roots, &mut recent, interval, SETTLE);
            }
            if now >= next_sweep {
                // The sweep exists to catch what the kernel cannot report. When
                // every root is actually watched there is little left for it to
                // find, so it backs off — walking the whole tree every five
                // seconds to prove nothing changed was most of what the old
                // watcher did, and it is not free on a large library.
                let covered = events.as_ref().is_some_and(|watch| watch.covers(&roots));
                next_sweep = now
                    + if covered {
                        interval * SWEEP_COVERED_FACTOR
                    } else {
                        interval
                    };
                fresh.extend(sweep(&roots, &mut seen, &mut baselined, SETTLE));
                // Keep the cooldown map from growing with the library.
                recent.retain(|_, at| at.elapsed() < interval * 4);
            }
            fresh.sort();
            fresh.dedup();
            if !fresh.is_empty() && signals.send(WatchSignal::Files(fresh)).is_err() {
                return Ok(());
            }
        } else if events.is_some() {
            // Release the kernel watches while watching is switched off.
            events = None;
        }

        // Sleep in slices so cancellation stays responsive.
        let mut waited = Duration::ZERO;
        while waited < TICK {
            if ctx.cancelled() {
                return Ok(());
            }
            let slice = Duration::from_millis(100).min(TICK - waited);
            std::thread::sleep(slice);
            waited += slice;
        }
    }
}

/// The event side of the watch: a kernel watcher whose callback deposits
/// changed paths into a buffer this job drains.
struct Events {
    watcher: notify::RecommendedWatcher,
    /// Paths the kernel has reported since the last drain.
    reported: Arc<Mutex<Vec<PathBuf>>>,
    /// Reported paths held back until they stop changing.
    held: Vec<PathBuf>,
    /// Roots currently watched, so a settings change can be followed.
    roots: Vec<PathBuf>,
}

impl Events {
    /// Start a watcher over `roots`. `None` when the platform has no backend,
    /// or the process is out of watches — the sweep then does all the work,
    /// which is exactly the behaviour before events existed.
    fn start(roots: &[PathBuf]) -> Option<Self> {
        let reported: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = Arc::clone(&reported);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                let mut paths = sink.lock().unwrap_or_else(|e| e.into_inner());
                paths.extend(event.paths);
            })
            .ok()?;
        let mut watched = Vec::new();
        for root in roots {
            if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                watched.push(root.clone());
            }
        }
        Some(Self {
            watcher,
            reported,
            held: Vec::new(),
            roots: watched,
        })
    }

    /// Follow a change in the configured root list: attach what is new, drop
    /// what is gone. A root that cannot be watched (deleted, unreadable, on a
    /// filesystem without events) is left to the sweep.
    fn sync(&mut self, roots: &[PathBuf]) {
        let gone: Vec<PathBuf> = self
            .roots
            .iter()
            .filter(|root| !roots.contains(root))
            .cloned()
            .collect();
        for root in gone {
            let _ = self.watcher.unwatch(&root);
            self.roots.retain(|r| r != &root);
        }
        for root in roots {
            if !self.roots.contains(root)
                && self.watcher.watch(root, RecursiveMode::Recursive).is_ok()
            {
                self.roots.push(root.clone());
            }
        }
    }

    /// Whether every one of `roots` is actually being watched — what the sweep
    /// backs off on.
    fn covers(&self, roots: &[PathBuf]) -> bool {
        !roots.is_empty() && roots.iter().all(|root| self.roots.contains(root))
    }

    /// Paths the kernel reported that are new, real and settled. A candidate
    /// that is still changing goes back into [`Self::held`] for the next tick;
    /// one that vanished (a temporary file an app deleted) is dropped.
    fn drain(
        &mut self,
        roots: &[PathBuf],
        recent: &mut HashMap<PathBuf, Instant>,
        cooldown: Duration,
        settle: Duration,
    ) -> Vec<PathBuf> {
        let mut candidates = std::mem::take(&mut self.held);
        candidates.extend(std::mem::take(
            &mut *self.reported.lock().unwrap_or_else(|e| e.into_inner()),
        ));
        candidates.sort();
        candidates.dedup();

        let now = SystemTime::now();
        let mut fresh = Vec::new();
        for path in candidates {
            let Some(root) = roots.iter().find(|root| path.starts_with(root)) else {
                continue; // an unwatched root: its events are stale
            };
            if hidden_under(root, &path) {
                continue;
            }
            if !settled(&path, now, settle) {
                if path.is_file() {
                    self.held.push(path);
                }
                continue;
            }
            if recent
                .get(&path)
                .is_some_and(|offered| offered.elapsed() < cooldown)
            {
                continue;
            }
            recent.insert(path.clone(), Instant::now());
            fresh.push(path);
        }
        fresh
    }
}

/// One full pass over the watch roots: baseline new roots, then report
/// everything under them that is not in `seen` and has stopped changing.
///
/// The caller must not add the result to `seen`: a file the embedder could not
/// import yet has to come back on a later sweep — that re-offer is the retry.
/// Files too young to import are skipped rather than held; not being in `seen`,
/// they come back on their own.
fn sweep(
    roots: &[PathBuf],
    seen: &mut HashSet<PathBuf>,
    baselined: &mut HashSet<PathBuf>,
    settle: Duration,
) -> Vec<PathBuf> {
    if roots.is_empty() {
        return Vec::new();
    }
    let files = super::import::all_files(roots);
    for root in roots {
        if baselined.insert(root.clone()) {
            for file in &files {
                if file.starts_with(root) {
                    seen.insert(file.clone());
                }
            }
        }
    }
    let now = SystemTime::now();
    files
        .into_iter()
        .filter(|file| !seen.contains(file) && settled(file, now, settle))
        .collect()
}

/// Whether `path` sits under a hidden component of `root` — the rule the import
/// walk applies, so the fast path and the sweep agree on what is a candidate.
fn hidden_under(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .map(|rel| {
            rel.components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(true)
}

/// Whether a file exists, is a file, and was last modified at least `settle`
/// ago — see [`SETTLE`].
fn settled(path: &Path, now: SystemTime, settle: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    meta.modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= settle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "trove-watch-{name}-{}-{}",
            std::process::id(),
            crate::model::new_id().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sweep_baselines_then_reports_fresh_files_only() {
        let root = temp_dir("roots");
        std::fs::write(root.join("a.png"), b"x").unwrap();
        std::fs::write(root.join(".hidden.png"), b"x").unwrap();
        let mut seen = HashSet::new();
        let mut baselined = HashSet::new();

        // First sweep baselines: existing files are not reported.
        let fresh = sweep(
            std::slice::from_ref(&root),
            &mut seen,
            &mut baselined,
            Duration::ZERO,
        );
        assert!(fresh.is_empty(), "{fresh:?}");

        // A new file is reported, and keeps being reported until the caller
        // records it — that repeat is how a refused import gets retried.
        std::fs::write(root.join("b.jpg"), b"x").unwrap();
        let fresh = sweep(
            std::slice::from_ref(&root),
            &mut seen,
            &mut baselined,
            Duration::ZERO,
        );
        assert_eq!(fresh, vec![root.join("b.jpg")]);
        let fresh = sweep(
            std::slice::from_ref(&root),
            &mut seen,
            &mut baselined,
            Duration::ZERO,
        );
        assert_eq!(fresh, vec![root.join("b.jpg")]);

        // Once the embedder has it, it stops being offered.
        seen.insert(root.join("b.jpg"));
        assert!(
            sweep(
                std::slice::from_ref(&root),
                &mut seen,
                &mut baselined,
                Duration::ZERO
            )
            .is_empty()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hidden_components_are_never_candidates() {
        let root = PathBuf::from("/lib/photos");
        assert!(!hidden_under(&root, &root.join("trip/a.jpg")));
        assert!(hidden_under(&root, &root.join(".thumbs/a.jpg")));
        assert!(hidden_under(&root, &root.join("trip/.hidden.jpg")));
        // Outside the root: not ours to watch.
        assert!(hidden_under(&root, &PathBuf::from("/elsewhere/a.jpg")));
    }

    #[test]
    fn a_file_is_only_settled_once_it_stops_changing() {
        let dir = temp_dir("settle");
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let now = SystemTime::now();
        assert!(settled(&file, now, Duration::ZERO));
        assert!(!settled(&file, now, Duration::from_secs(60)));
        assert!(
            !settled(&dir, now, Duration::ZERO),
            "a directory is not a file"
        );
        assert!(!settled(&dir.join("gone.png"), now, Duration::ZERO));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The kernel path is what makes a drop show up in under a second. If the
    /// platform hands us no watcher this test has nothing to say — the sweep
    /// covers that case — so it passes without asserting.
    #[test]
    fn kernel_events_reach_the_buffer() {
        let root = temp_dir("events");
        let Some(mut events) = Events::start(std::slice::from_ref(&root)) else {
            return;
        };
        std::fs::write(root.join("new.png"), b"x").unwrap();

        let mut recent = HashMap::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fresh = events.drain(
                std::slice::from_ref(&root),
                &mut recent,
                Duration::ZERO,
                Duration::ZERO,
            );
            if fresh.iter().any(|p| p.ends_with("new.png")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no event for a new file within 5s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // Reports of the same path do not repeat within the cooldown.
        assert!(
            events
                .drain(
                    std::slice::from_ref(&root),
                    &mut recent,
                    Duration::from_secs(60),
                    Duration::ZERO
                )
                .is_empty()
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
