//! The resident background services that outlive any single job: the folder
//! watch task (kernel events + a periodic sweep, whose discoveries become
//! imports) and the index-drain loop (flushes the search outbox into the text
//! index on a timer). Both are started once per session and re-started against
//! a new library after a swap.

use std::path::PathBuf;

use gpui_kit::*;

use trove_core::tasks::watch::{self, WatchSignal};
use trove_core::tasks::{TaskKind, TaskManager};

use super::POLL_INTERVAL;
use super::import::{InboxDrain, collect_inbox_app, import_paths_app_into};
use crate::library::LibraryController;

/// Handle for the resident watch task, kept on the controller: the manager
/// clone belongs to the library the task was started against, so a library
/// swap can cancel the old job before a new one starts.
pub struct WatchTask {
    pub manager: TaskManager,
    pub task_id: trove_core::tasks::TaskId,
}

/// Start the resident watch task: a backend thread watches the collect inbox
/// and the configured watch roots — kernel events, falling back to a full
/// sweep at least every [`watch::WATCH_INTERVAL`] — and reports discoveries;
/// this module's pump turns them into imports. Call once at startup and again
/// after a library swap.
pub fn start_watch_service(
    controller: &Entity<LibraryController>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    let library_dir = controller.read(cx).library.root().to_path_buf();
    // The pump guards every tick against this same root: the watch thread
    // gets its own copy, the pump another.
    let pump_root = library_dir.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    // The other half of the watcher's retry contract: whatever the pump accepts
    // is reported back, so the job stops offering it. Without this the sweep
    // re-offered every file that arrived after the baseline, and offering one
    // costs a hash — for as long as the process lived.
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let started = manager.start(TaskKind::WatchScan, "watch", move |ctx| {
        watch::run(
            watch::WATCH_INTERVAL,
            library_dir.clone(),
            trove_core::services::collect::inbox_dir(),
            tx,
            accepted_rx,
            ctx,
        )
    });
    let Ok((task_id, _signals)) = started else {
        return false; // already watching
    };
    controller.update(cx, |ctl, _| {
        ctl.watch_task = Some(WatchTask {
            manager: manager.clone(),
            task_id,
        });
        ctl.watch_handle = Some(handle);
    });
    watch_signals(controller.clone(), rx, accepted_tx, pump_root, handle, cx);
    true
}

/// Consume watch signals: inbox discoveries start a collect import; fresh
/// files under watch roots queue for import, retried until accepted (a
/// manual import still running refuses the batch, and the queue keeps the
/// files pending — the same contract the old watcher loop had).
///
/// `library_root` is the library this watch service was started against. A
/// swap cancels the old watcher cooperatively — its thread checks the flag
/// between ticks — so for a window the old pump is still alive while the
/// controller already points at the new library. Signals from that window
/// must not act: a pending file from the old library's watch roots would be
/// imported into the new one. The pump drops everything until the channel
/// closes.
fn watch_signals(
    controller: Entity<LibraryController>,
    rx: std::sync::mpsc::Receiver<WatchSignal>,
    accepted: std::sync::mpsc::Sender<Vec<PathBuf>>,
    library_root: PathBuf,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let mut pending: Vec<PathBuf> = Vec::new();
        // An inbox signal the drain refused (an import of the other kind was
        // already running) is not lost: it stays pending and is retried on
        // later ticks. The retry is gated on that import having finished,
        // because the gate is a flag while the drain lists the inbox and asks
        // the database — checking every tick while busy would be the cost of
        // the very loop this replaced.
        let mut inbox_pending = false;
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;

            // Checked every tick, not only when a signal arrives: a swap
            // between signals must still stop this pump's queued batch.
            let same_library = controller.update(cx, |ctl, _| ctl.library.root() == library_root);
            if !same_library {
                // The library moved under this pump: its watcher is being (or
                // has been) cancelled, its signals describe the old library's
                // roots, and its pending files must not land in the new one.
                // Drain and ignore until the channel closes, then stop.
                pending.clear();
                let mut channel_open = true;
                loop {
                    match rx.try_recv() {
                        Ok(_) => {}
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            channel_open = false;
                            break;
                        }
                    }
                }
                if !channel_open {
                    break;
                }
                continue;
            }

            let mut channel_open = true;
            loop {
                match rx.try_recv() {
                    Ok(WatchSignal::Inbox) => {
                        let outcome = handle.update(cx, |_view, window, cx| {
                            collect_inbox_app(&controller, window, cx)
                        });
                        inbox_pending = matches!(outcome, Ok(InboxDrain::Refused));
                    }
                    Ok(WatchSignal::Files(files)) => pending.extend(files),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        channel_open = false;
                        break;
                    }
                }
            }

            // The retry half of the inbox contract: a refused drain leaves its
            // signal pending until a tick finds the import slot free. The gate
            // is a flag; only a free slot pays for the listing.
            if inbox_pending {
                let outcome = handle.update(cx, |_view, window, cx| {
                    if controller.read(cx).is_importing() {
                        return InboxDrain::Refused;
                    }
                    collect_inbox_app(&controller, window, cx)
                });
                if !matches!(outcome, Ok(InboxDrain::Refused)) {
                    inbox_pending = false;
                }
            }

            if !pending.is_empty() {
                let accepted_flag = handle
                    .update(cx, |_view, window, cx| {
                        if controller.read(cx).is_importing() {
                            return false;
                        }
                        import_paths_app_into(&controller, pending.clone(), None, window, cx)
                    })
                    .unwrap_or(false);
                // Mark seen only after the batch was accepted; a refusal
                // retries on the next pump tick. The watcher is told the same
                // thing, so it stops offering files it has already handed
                // over. A dying channel has no watcher left to retry — the
                // queue would just leak, so it goes.
                if accepted_flag || !channel_open {
                    if accepted_flag {
                        let _ = accepted.send(pending.clone());
                    }
                    pending.clear();
                }
            }

            if !channel_open {
                break;
            }
        }
    })
    .detach();
}

/// Cadence for the resident index-drain loop. The read paths drain too —
/// every search and browse flushes the outbox first — so this loop is not
/// what makes the index converge during normal use; it is the safety net:
/// it retries a drain that failed on a busy write lock, and keeps the index
/// current even when nothing reads.
const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Start the resident index-drain loop: every [`DRAIN_INTERVAL`], flush the
/// `search_queue` outbox into the text index. The store connection is
/// confined to the UI thread, so — like the watch pump — this runs as a
/// foreground task, and a tick costs one small SELECT while the queue is
/// empty.
///
/// Started once per session, next to the watch service. A library swap needs
/// no restart — the loop follows whatever library the controller holds — and
/// the loop ends when its window does.
pub fn start_index_drain_service(
    controller: &Entity<LibraryController>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(DRAIN_INTERVAL).await;
            // A dead window ends the loop. A running import is skipped, not
            // forced: the import's own connection takes the write lock in
            // bursts, and the drain's batch delete would otherwise wait out
            // the store's 5 s busy timeout on this thread — the outbox rows
            // are still there for the next tick.
            let ran = handle.update(cx, |_, _, cx| {
                if controller.read(cx).is_importing() {
                    return;
                }
                if let Err(error) = controller.update(cx, |ctl, _| ctl.library.drain_search_queue())
                {
                    tracing::warn!(
                        %error,
                        "periodic search outbox drain failed; the next tick retries"
                    );
                }
            });
            if ran.is_err() {
                break;
            }
        }
    })
    .detach();
}
