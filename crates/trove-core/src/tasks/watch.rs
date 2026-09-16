//! The folder-watch resident job: a periodic sweep over the collect-service
//! inbox and the configured watch roots, reporting discoveries over a
//! channel so the UI layer can start imports.
//!
//! The job never starts imports itself — that is the embedder's call, so
//! progress toasts and controller state stay in one place. The channel
//! closes when the job exits (cancelled or panicked); the embedder treats a
//! closed channel as "watch stopped".
//!
//! Scan semantics: the inbox is signalled whenever files are waiting; watch
//! roots are baselined on first sight (attaching a watch never
//! retro-imports what is already there) and only *new* files are signalled.
//! The embedder marks acceptance — a refused import (another import still
//! running) must retry on a later sweep, so the job keeps re-signalling
//! files it has not been told to forget. In practice the embedder accepts
//! everything and the one-import-at-a-time rule in [`super::TaskManager`]
//! makes the next sweep a no-op retry.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use super::JobContext;
use crate::config::{AppConfig, LibraryConfig};

/// Sweep cadence. Both configs are re-read every sweep, so changes in
/// Settings apply without a restart.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// Something discovered on a sweep that the embedder should act on.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchSignal {
    /// The collect-service inbox has files waiting.
    Inbox,
    /// New files appeared under the watched roots.
    Files(Vec<PathBuf>),
}

/// Run the resident watch loop until cancelled. Sends every discovery to
/// `signals`; sleeps in small cancellable slices between sweeps.
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
    loop {
        if ctx.cancelled() {
            return Ok(());
        }
        let config = AppConfig::load();
        let library = LibraryConfig::load(&library_dir);

        if config.collect_enabled()
            && !crate::services::collect::inbox_items_in(&inbox_dir).is_empty()
            && signals.send(WatchSignal::Inbox).is_err()
        {
            return Ok(()); // embedder hung up; stop watching
        }

        if library.watch_folders_enabled() {
            let roots = library.watched_folders.clone();
            for signal in sweep(&roots, inbox_dir.as_path(), &mut seen, &mut baselined) {
                if signals.send(signal).is_err() {
                    return Ok(());
                }
            }
        }

        // Sleep in slices so cancellation is responsive.
        let mut waited = Duration::ZERO;
        while waited < interval {
            if ctx.cancelled() {
                return Ok(());
            }
            let slice = Duration::from_millis(200).min(interval - waited);
            std::thread::sleep(slice);
            waited += slice;
        }
    }
}

/// One sweep over the watch roots: baseline new roots, diff against `seen`,
/// return the fresh files as a signal (empty when nothing new).
fn sweep(
    roots: &[PathBuf],
    _inbox: &std::path::Path,
    seen: &mut HashSet<PathBuf>,
    baselined: &mut HashSet<PathBuf>,
) -> Vec<WatchSignal> {
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
    let fresh: Vec<PathBuf> = files.into_iter().filter(|f| !seen.contains(f)).collect();
    if fresh.is_empty() {
        Vec::new()
    } else {
        vec![WatchSignal::Files(fresh)]
    }
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
        let inbox = temp_dir("inbox");

        // First sweep baselines: existing files are not reported.
        let signals = sweep(
            std::slice::from_ref(&root),
            &inbox,
            &mut seen,
            &mut baselined,
        );
        assert!(signals.is_empty());

        // A new file is reported once; marking seen is the caller's job.
        std::fs::write(root.join("b.jpg"), b"x").unwrap();
        let signals = sweep(
            std::slice::from_ref(&root),
            &inbox,
            &mut seen,
            &mut baselined,
        );
        match &signals[..] {
            [WatchSignal::Files(files)] => assert_eq!(files, &[root.join("b.jpg")]),
            other => panic!("expected one Files signal, got {other:?}"),
        }
        for signal in &signals {
            if let WatchSignal::Files(files) = signal {
                for f in files {
                    seen.insert(f.clone());
                }
            }
        }
        assert!(
            sweep(
                std::slice::from_ref(&root),
                &inbox,
                &mut seen,
                &mut baselined
            )
            .is_empty()
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&inbox).ok();
    }
}
