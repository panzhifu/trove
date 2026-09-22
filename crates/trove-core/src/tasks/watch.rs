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
//! Scan semantics: watch roots are baselined on first sight (attaching a watch
//! never retro-imports what is already there) and only *new* files are
//! signalled. A file is only offered once it has stopped changing — see
//! [`SETTLE`]. What the scanned folders ask to be left out with their
//! [`.gitignore` files](super::ignore) is left out of both halves of the scan,
//! so a burst the kernel reports is decided the same way a sweep decides it.
//!
//! The job keeps offering a file until the embedder says it has it: a refusal
//! (another import still running) has to retry, so a refused batch is simply
//! not acknowledged. `accepted` is the other half of that contract — the paths
//! the embedder imported. Without it the job re-offered every uncovered file on
//! every sweep, and since "offering" means "hash this file again" the embedder
//! paid for the same import over and over for the life of the process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use notify::{RecursiveMode, Watcher};

use super::JobContext;
use super::ignore::Ignores;
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
///
/// `accepted` carries the paths the embedder has taken responsibility for. It
/// is half of the retry contract: what is never acknowledged comes back on the
/// next sweep, and what is acknowledged stops being offered.
pub fn run(
    interval: Duration,
    library_dir: std::path::PathBuf,
    inbox_dir: std::path::PathBuf,
    signals: Sender<WatchSignal>,
    accepted: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    ctx: &JobContext,
) -> Result<(), String> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut baselined: HashSet<PathBuf> = HashSet::new();
    let mut events: Option<Events> = None;
    // The inbox has its own watcher, and it only ever has to answer "did
    // something land?": the embedder enumerates the directory itself, because
    // it needs the sidecars to record a source URL.
    let mut inbox_activity: Option<Activity> = None;
    // The listing as of the last inbox signal: the fallback signals on
    // *change* against this, not on "not empty" — see the collect block below.
    let mut inbox_listing: Option<Vec<(std::path::PathBuf, Option<std::path::PathBuf>)>> = None;
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
    // The ignore rules for the event path, which has no walk to carry them.
    // Re-read on the settings cadence, so an ignore file the user edits applies
    // within one tick of the sweep noticing.
    let mut ignores = Ignores::default();

    loop {
        if ctx.cancelled() {
            return Ok(());
        }
        let now = Instant::now();

        // Acknowledgements first: they make the rest of the tick cheaper.
        // `seen` holds both halves of "do not offer this again" — the files a
        // root had when it was first watched, and the files the embedder has
        // since imported. Without the second half the sweep re-offered every
        // file that appeared after the baseline, and offering a file means
        // hashing it again, for the life of the process.
        while let Ok(paths) = accepted.try_recv() {
            seen.extend(paths);
        }

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
            ignores.clear();
        }

        if config.collect_enabled() {
            if inbox_activity.is_none() && settings_due {
                inbox_activity = Activity::watch(&inbox_dir);
            }
            // The kernel says when something lands, and the collect service
            // renames its files into place, so that report is reliable. The
            // periodic scan stays as the fallback — it is also what notices a
            // file that was already waiting when the watcher started — but it
            // compares listings instead of just checking "not empty": the
            // inbox keeps its files forever (they are linked, not copied), so
            // an always-nonempty check would signal — and the embedder would
            // re-enumerate, re-stat and re-dedup — the entire inbox history
            // every interval, for the life of the process. Signalling on the
            // first look (so pre-existing files still import) and on change
            // (so an import that consumed sidecars re-arms the check) keeps
            // the steady state silent.
            let touched = inbox_activity.as_ref().is_some_and(|watch| watch.take());
            let listing = crate::services::collect::inbox_items_in(&inbox_dir);
            let changed = inbox_listing.as_ref() != Some(&listing);
            if !listing.is_empty() && (touched || changed) {
                if signals.send(WatchSignal::Inbox).is_err() {
                    return Ok(()); // embedder hung up; stop watching
                }
                inbox_listing = Some(listing);
            }
        } else {
            // Release the watch while collection is switched off.
            inbox_activity = None;
            inbox_listing = None;
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
                fresh = watch.drain(&roots, &seen, &mut recent, &mut ignores, interval, SETTLE);
            }
            if now >= next_sweep {
                // The sweep exists to catch what the kernel cannot report. When
                // every root is actually watched there is little left for it to
                // find, so it backs off — walking the whole tree every five
                // seconds to prove nothing changed was most of what the old
                // watcher did, and it is not free on a large library. What the
                // kernel does report is unaffected: those files were already
                // offered above.
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
    ///
    /// `seen` is the same filter the sweep applies — baselined files and files
    /// the embedder has already acknowledged — so the two sources agree on what
    /// still needs offering. `ignores` decides what the scanned folders ask to
    /// be left out, which the sweep gets from its walk and this path has to read.
    fn drain(
        &mut self,
        roots: &[PathBuf],
        seen: &HashSet<PathBuf>,
        recent: &mut HashMap<PathBuf, Instant>,
        ignores: &mut Ignores,
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
            if hidden_under(root, &path) || !still_wanted(&path, seen) {
                continue;
            }
            // After the two free filters, because this one reads: a file the
            // watcher reports again once it has been acknowledged costs nothing
            // here, and a new folder's rules are read once, not per file in it.
            if ignores.is_left_out(root, &path, false) {
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

/// A watcher that only reports *that* something happened under one directory.
///
/// The inbox needs no paths: the embedder lists the directory itself, because
/// the sidecar next to a captured file is what carries its source URL. So this
/// keeps one flag rather than a path buffer, and the per-tick cost is an atomic
/// swap instead of a directory scan — which matters, because the inbox is where
/// collected files *stay* (the library links them), so it only ever grows.
struct Activity {
    _watcher: notify::RecommendedWatcher,
    touched: Arc<AtomicBool>,
}

impl Activity {
    /// Watch `dir`, or `None` when the platform cannot (no backend) or the
    /// directory is not there yet — the periodic scan then does the work, and
    /// the caller retries on its next pass.
    fn watch(dir: &Path) -> Option<Self> {
        // Starts set, so the first pass scans once and picks up whatever was
        // already waiting before the watcher existed.
        let touched = Arc::new(AtomicBool::new(true));
        let sink = Arc::clone(&touched);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                if counts_as_activity(&event) {
                    sink.store(true, Ordering::Relaxed);
                }
            })
            .ok()?;
        // Non-recursive: captures land directly in the inbox.
        if watcher.watch(dir, RecursiveMode::NonRecursive).is_err() {
            return None;
        }
        Some(Self {
            _watcher: watcher,
            touched,
        })
    }

    /// Whether anything happened since the last call.
    fn take(&self) -> bool {
        self.touched.swap(false, Ordering::Relaxed)
    }
}

/// Whether an event means the inbox changed, as opposed to being read.
///
/// Only the write side counts: what lands, changes or goes away. Reads have to
/// be excluded by name because this job lists the inbox on *every* tick, and
/// reading a directory is reported as `Access(Open)` — counting "any event"
/// made the listing scan its own trigger, which signalled the embedder every
/// tick and left it starting an inbox import that had nothing to do (for as
/// long as a single file sits in the inbox, which is forever: the inbox keeps
/// its files, they are linked rather than copied).
///
/// `Any` and `Other` are backend catch-alls — on a platform whose events
/// cannot be classified, they are all there is — so they stay counted.
fn counts_as_activity(event: &notify::Event) -> bool {
    !matches!(event.kind, notify::EventKind::Access(_))
}

/// One full pass over the watch roots: baseline new roots, then report
/// everything under them that is not in `seen` and has stopped changing.
///
/// The caller must not add the result to `seen`: a file the embedder could not
/// import yet has to come back on a later sweep — that re-offer is the retry,
/// and it is the acknowledgement channel that ends it. Files too young to
/// import are skipped rather than held; not being in `seen`, they come back on
/// their own.
fn sweep(
    roots: &[PathBuf],
    seen: &mut HashSet<PathBuf>,
    baselined: &mut HashSet<PathBuf>,
    settle: Duration,
) -> Vec<PathBuf> {
    if roots.is_empty() {
        return Vec::new();
    }
    // Symlinks and non-UTF-8 names land in `unrepresentable` and are dropped:
    // the sweep's contract is "paths worth offering", and re-offering them
    // forever would only spin. A *dropped folder* import reports them (see
    // `expand_dirs`); the watch path never had a channel for skips.
    let mut unrepresentable = Vec::new();
    let files = super::import::all_files(roots, &mut unrepresentable);
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
        .filter(|file| still_wanted(file, seen) && settled(file, now, settle))
        .collect()
}

/// Whether a candidate still deserves offering: not baselined away when its
/// root was first watched, and not already in the embedder's hands.
fn still_wanted(path: &Path, seen: &HashSet<PathBuf>) -> bool {
    !seen.contains(path)
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
        .is_some_and(|modified| match now.duration_since(modified) {
            // A timestamp in the future — an archive that kept its mtimes, a
            // skewed clock — can never age into `settle` by waiting, so waiting
            // would hide the file forever. Take it as settled.
            Ok(age) => age >= settle,
            Err(_) => true,
        })
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

    /// What the folder leaves out with its own ignore files the sweep leaves
    /// out too — the same rule the kernel path applies, so the two agree on
    /// what still needs offering.
    #[test]
    fn the_sweep_leaves_out_what_the_folder_leaves_out() {
        let root = temp_dir("sweep-ignored");
        std::fs::write(root.join(".gitignore"), b"build/\n*.tmp\n").unwrap();
        std::fs::create_dir_all(root.join("build/nested")).unwrap();
        std::fs::write(root.join("build/nested/out.png"), b"x").unwrap();
        std::fs::write(root.join("scratch.tmp"), b"x").unwrap();
        std::fs::write(root.join("keep.png"), b"x").unwrap();
        let roots = std::slice::from_ref(&root);
        let mut seen = HashSet::new();
        let mut baselined = HashSet::new();

        // The baseline holds only the kept file: an ignored path is not a
        // candidate at all, so dropping the rule later presents it as new —
        // which is what removing an ignore rule is for.
        assert!(
            sweep(roots, &mut seen, &mut baselined, Duration::ZERO).is_empty(),
            "the first sweep baselines, so it reports nothing"
        );
        std::fs::write(root.join("keep2.png"), b"x").unwrap();
        assert_eq!(
            sweep(roots, &mut seen, &mut baselined, Duration::ZERO),
            vec![root.join("keep2.png")]
        );

        std::fs::remove_dir_all(&root).ok();
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

    /// An mtime in the future — an archive that kept its timestamps, a skewed
    /// clock — can never age into the settle window, so it must count as
    /// settled right away rather than hide from the importer forever.
    #[test]
    fn a_file_dated_in_the_future_is_settled() {
        let dir = temp_dir("settle-future");
        let file = dir.join("future.png");
        std::fs::write(&file, b"x").unwrap();
        let two_days_ahead = SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60);
        let handle = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(two_days_ahead))
            .unwrap();

        let now = SystemTime::now();
        assert!(settled(&file, now, Duration::from_secs(60)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reading the inbox is not activity.
    ///
    /// The listing scan runs every tick, and `read_dir` opens the directory —
    /// which inotify reports as `Access(Open)` (measured). Counting it made
    /// the scan its own trigger: an inbox signal every tick, and behind each
    /// one an inbox import with nothing to do, for as long as a single file
    /// sits in the inbox — which, since a screenshot or a collected file stays
    /// there for good, is the whole life of the process.
    ///
    /// inotify-shaped, so not run on macOS: notify's FSEvents backend works
    /// in imprecise mode by default, where every event arrives as
    /// `EventKind::Any` — reads and writes are indistinguishable there, and
    /// this filter cannot be expressed. The consequence on macOS is the one
    /// this test guards against on Linux: the listing scan may re-signal
    /// itself (a no-op import per tick), which is a wart, not a wrong import.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn reading_the_inbox_is_not_activity() {
        let dir = temp_dir("inbox-read");
        std::fs::write(dir.join("shot.png"), b"x").unwrap();
        let Some(activity) = Activity::watch(&dir) else {
            return; // no backend here: the periodic scan carries the inbox
        };
        // The watcher starts set, so the first pass sees files that predate it.
        assert!(activity.take());
        // A listing scan, exactly as the loop runs it every tick.
        assert_eq!(crate::services::collect::inbox_items_in(&dir).len(), 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!activity.take(), "a read of the inbox is not activity");

        // A file landing still is: the filter must not have deafened it.
        std::fs::write(dir.join("shot2.png"), b"x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !activity.take() {
            assert!(
                Instant::now() < deadline,
                "a file landing in the inbox went unnoticed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_write_side_events_count_as_inbox_activity() {
        use notify::EventKind;
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind};

        let read = notify::Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)));
        assert!(!counts_as_activity(&read));
        let read_close = notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)));
        assert!(!counts_as_activity(&read_close));

        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Remove(notify::event::RemoveKind::File),
            EventKind::Any,
        ] {
            assert!(
                counts_as_activity(&notify::Event::new(kind)),
                "{kind:?} must count as activity"
            );
        }
    }

    /// The kernel path is what makes a drop show up in under a second. If the
    /// platform hands us no watcher this test has nothing to say — the sweep
    /// covers that case — so it passes without asserting.
    ///
    /// Not run on macOS: FSEvents registers its stream asynchronously on a
    /// runloop thread with `SinceNow`, so a file written immediately after
    /// `Events::start` can land before the stream is live and never be
    /// reported. The sweep covers that case there (a root that `watch`ed
    /// successfully still backs off to the slower cadence, so files arrive
    /// late rather than never).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn kernel_events_reach_the_buffer_until_they_are_acknowledged() {
        let root = temp_dir("events");
        let Some(mut events) = Events::start(std::slice::from_ref(&root)) else {
            return;
        };
        std::fs::write(root.join("new.png"), b"x").unwrap();

        let roots = std::slice::from_ref(&root);
        let mut seen = HashSet::new();
        let mut recent = HashMap::new();
        let mut ignores = Ignores::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fresh = events.drain(
                roots,
                &seen,
                &mut recent,
                &mut ignores,
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

        // An acknowledgement is what takes a path out of the offering set —
        // without it the embedder would re-import (and re-hash) the same file
        // on every sweep for as long as the process lives.
        seen.insert(root.join("new.png"));
        assert!(
            events
                .drain(
                    roots,
                    &seen,
                    &mut recent,
                    &mut ignores,
                    Duration::ZERO,
                    Duration::ZERO
                )
                .is_empty()
        );

        // An unacknowledged path is held back by the cooldown instead, so the
        // tick rate does not multiply the retries.
        assert!(
            events
                .drain(
                    roots,
                    &HashSet::new(),
                    &mut recent,
                    &mut ignores,
                    Duration::from_secs(60),
                    Duration::ZERO
                )
                .is_empty()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The fast path reads the same ignore files the sweep does. The kernel
    /// reports what lands inside a folder the scan would never enter — a build
    /// directory a watcher filled in one go — so the decision has to be made
    /// per reported path, or the excluded tree arrives anyway.
    ///
    /// Not run on macOS, for the reason given on
    /// [`kernel_events_reach_the_buffer_until_they_are_acknowledged`].
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_reported_file_the_folder_leaves_out_is_not_offered() {
        let root = temp_dir("events-ignored");
        std::fs::write(root.join(".gitignore"), b"build/\n").unwrap();
        let Some(mut events) = Events::start(std::slice::from_ref(&root)) else {
            return;
        };
        std::fs::create_dir_all(root.join("build/nested")).unwrap();
        std::fs::write(root.join("build/nested/out.png"), b"x").unwrap();
        std::fs::write(root.join("kept.png"), b"x").unwrap();

        let roots = std::slice::from_ref(&root);
        let seen = HashSet::new();
        let mut recent = HashMap::new();
        let mut ignores = Ignores::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fresh = events.drain(
                roots,
                &seen,
                &mut recent,
                &mut ignores,
                Duration::ZERO,
                Duration::ZERO,
            );
            if fresh.iter().any(|p| p.ends_with("kept.png")) {
                assert!(
                    !fresh.iter().any(|p| p.ends_with("out.png")),
                    "a file under an ignored folder was offered: {fresh:?}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no event for a new file within 5s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        std::fs::remove_dir_all(&root).ok();
    }
}
