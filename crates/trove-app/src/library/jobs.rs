//! Import bridge: the pipeline itself lives in `trove_core::tasks::import`
//! and runs on a backend thread managed by the library's `TaskManager`. This
//! module is only the translator between those jobs and the UI: it starts
//! jobs, then polls task events to drive the controller's import phase and
//! the progress toasts, and runs the resident folder-watch task whose
//! signals become imports.

use std::path::PathBuf;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::media::import::ImportStorage;
use trove_core::tasks::embed::EmbedOutcome;
use trove_core::tasks::import::{self, ImportOptions, ImportOutcome, ImportSource};
use trove_core::tasks::watch::{self, WatchSignal};
use trove_core::tasks::{TaskEvent, TaskId, TaskKind, TaskManager, TaskStatus};

use crate::library::{AiProbe, LibraryController};

/// Marker type for the import progress toast: pushing with the same id
/// replaces the previous toast instead of stacking a new one.
pub struct ImportNotice;

/// Handle of the running import job, kept on the controller: the cancel
/// button presses it, and a library swap cancels-and-waits on it before
/// swapping the store.
pub struct ImportTaskHandle {
    pub manager: TaskManager,
    pub task_id: TaskId,
}

/// Handle for the resident watch task, kept on the controller: the manager
/// clone belongs to the library the task was started against, so a library
/// swap can cancel the old job before a new one starts.
pub struct WatchTask {
    pub manager: TaskManager,
    pub task_id: TaskId,
}

/// Poll cadence for task events; the backend throttles progress to ~10/s,
/// so this keeps the bar smooth without busy-looping.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

/// Marker for the keyed XMP export toast: pushing with the same id replaces the
/// previous toast instead of stacking one per asset.
pub struct XmpNotice;

/// Write XMP sidecars for the current selection.
///
/// One asset per turn on the UI thread: the store connection is thread-confined,
/// so the export cannot move to a background executor, and a selection of a few
/// thousand assets would otherwise freeze the window for the whole run. A
/// sidecar lands next to the asset's file — the library's blob for a stored
/// asset, the original for a linked one — which is why this is the one edit-side
/// operation that works on a link-only library.
pub fn export_xmp_app(controller: &Entity<LibraryController>, window: &mut Window, cx: &mut App) {
    let ids: Vec<uuid::Uuid> = controller.read(cx).selected_assets.as_ref().clone();
    if ids.is_empty() {
        window.push_notification(
            Notification::warning(rust_i18n::t!("xmp.empty_selection").to_string()),
            cx,
        );
        return;
    }

    let total = ids.len();
    let handle = window.window_handle();
    let controller = controller.clone();
    window.push_notification(
        Notification::info(rust_i18n::t!("xmp.started", count = total).to_string())
            .id1::<XmpNotice>("xmp-progress"),
        cx,
    );

    cx.spawn(async move |cx| {
        let mut written = 0u64;
        let mut skipped = 0u64;
        for (done, id) in ids.iter().enumerate() {
            let outcome = controller.update(cx, |ctl, _cx| ctl.library.export_xmp_sidecars(&[*id]));
            match outcome {
                Ok(report) => {
                    written += report.written;
                    skipped += report.skipped;
                }
                Err(_) => skipped += 1,
            }
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("xmp.running", done = done + 1, total = total).to_string(),
                    )
                    .id1::<XmpNotice>("xmp-progress"),
                    cx,
                );
            });
            // Yield: a spawned foreground future runs to completion once
            // polled, so without this the whole selection would be processed
            // inside one frame — the grid would freeze and this toast would
            // never repaint until the end.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;
        }

        let note = if skipped == 0 {
            Notification::success(rust_i18n::t!("xmp.done", count = written).to_string())
        } else {
            Notification::warning(
                rust_i18n::t!("xmp.done_skipped", written = written, skipped = skipped).to_string(),
            )
        };
        let _ = handle.update(cx, |_view, window, cx| {
            window.push_notification(note, cx);
        });
    })
    .detach();
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

/// Start an import from a set of file paths into the currently browsed
/// collection. See [`import_paths_app_into`].
///
/// Works from any entry point that holds an `App` (button, file drop, ...):
/// the whole pipeline — staging *and* database commits — runs on the backend
/// task thread; this function only starts it and watches the events.
pub fn import_paths_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let into_collection = controller.read(cx).current_collection;
    import_paths_app_into(controller, paths, into_collection, window, cx);
}

/// Import files the library itself just produced and left in a temporary
/// spot — clipboard pastes. They are *copied* into the store rather than
/// linked: a link would point at `/tmp`, which the system is free to empty
/// at any moment, leaving an asset with no reachable original.
pub fn import_copied_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let into_collection = controller.read(cx).current_collection;
    start_paths_import(
        controller,
        paths,
        into_collection,
        ImportStorage::Copy,
        window,
        cx,
    );
}

/// Start an import into an explicit collection (`None` = unfiled).
/// Directories in `paths` are expanded into their contained files, so a
/// dropped folder imports everything inside it. Returns `false` when the
/// batch was refused (already importing, empty list, nothing importable or a
/// dead target collection) so callers can retry later — the folder watcher
/// relies on this to keep new files pending.
pub fn import_paths_app_into(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    into_collection: Option<uuid::Uuid>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // A user import links: the file stays where the user keeps it.
    start_paths_import(
        controller,
        paths,
        into_collection,
        ImportStorage::Link,
        window,
        cx,
    )
}

/// The shared importer entry: snapshot everything the job needs from the
/// controller, then hand off.
fn start_paths_import(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    into_collection: Option<uuid::Uuid>,
    storage: ImportStorage,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if paths.is_empty() {
        return false;
    }

    // Snapshot everything the job needs from the controller, then hand off.
    let (manager, options, total) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return false;
        }
        // Fail fast when the target collection does not exist.
        if let Some(cid) = into_collection {
            let conn = ctl.library.store().conn();
            if trove_core::store::collections::get(conn, cid)
                .ok()
                .flatten()
                .is_none()
            {
                return false;
            }
        }
        // Count without walking. Progress totals must cover the folder
        // contents, not just the dropped entries, but the walk that counts them
        // belongs to the job (`tasks::import`): on a big folder it takes
        // seconds, and this thread used to pay for it — once here to learn the
        // total, and then again inside the job, for the same list. Zero means
        // "not known yet", which the UI renders as scanning.
        let total = if paths.iter().any(|p| p.is_dir()) {
            0
        } else {
            paths.len()
        };
        let options = ImportOptions {
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            storage,
            source: ImportSource::Paths {
                paths,
                into_collection,
            },
        };
        (ctl.library.tasks().clone(), options, total)
    };

    start_import_job(
        controller,
        &manager,
        TaskKind::Import,
        options,
        total,
        window,
        cx,
    )
}

/// What [`collect_inbox_app`] did with the waiting files.
///
/// The caller needs the difference: a refusal is the embedder's "try again
/// later" (an inbox signal that carried a file must not be dropped because an
/// unrelated import happened to be running), while `Idle` is a settled answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxDrain {
    /// A job is running over the waiting files.
    Started,
    /// Another import is running: nothing was touched, ask again later.
    Refused,
    /// Nothing was waiting, or the library already holds every waiting file.
    Idle,
}

/// Drain the collect-service inbox: import every waiting file (unfiled,
/// `source_url` stamped from the sidecar, files kept and linked).
///
/// The waiting-list comes from `collect::inbox_items` — the one definition
/// of what is waiting — rather than a hand-rolled enumeration: a private
/// copy of the skip rules is exactly how `.part` files (still being written)
/// and sidecars ended up being imported as assets in their own right. Files
/// the library already holds are dropped before the job is even started; see
/// [`waiting_files`].
pub fn collect_inbox_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> InboxDrain {
    let items = trove_core::services::collect::inbox_items();
    if items.is_empty() {
        return InboxDrain::Idle;
    }

    let (manager, options, total) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return InboxDrain::Refused;
        }
        let items = waiting_files(ctl, items);
        if items.is_empty() {
            tracing::debug!("collect inbox: nothing waiting that the library lacks");
            return InboxDrain::Idle;
        }
        let total = items.len();
        let options = ImportOptions {
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            // A collected file lives in the incoming directory, which is not a
            // scratch area — the import leaves it there and links it.
            storage: trove_core::media::import::ImportStorage::Link,
            source: ImportSource::CollectInbox { items },
        };
        (ctl.library.tasks().clone(), options, total)
    };

    if start_import_job(
        controller,
        &manager,
        TaskKind::CollectInbox,
        options,
        total,
        window,
        cx,
    ) {
        InboxDrain::Started
    } else {
        // Both import kinds share one slot: the other one got there first.
        InboxDrain::Refused
    }
}

/// Drop the waiting files the library already holds.
///
/// The inbox is where imports *stay* — a collected page and a screenshot are
/// linked, not copied — so a wake-up over that directory is normally a batch
/// of files the library has had for days. Asking first (by the same loose
/// file-name-plus-size key the import itself skips on) keeps that from costing
/// a job, a scan and a progress notice that then has nothing to report.
fn waiting_files(
    ctl: &LibraryController,
    items: Vec<(PathBuf, Option<PathBuf>)>,
) -> Vec<(PathBuf, Option<PathBuf>)> {
    let paths: Vec<PathBuf> = items.iter().map(|(path, _)| path.clone()).collect();
    let unimported: std::collections::HashSet<PathBuf> =
        ctl.library.unimported_paths(&paths).into_iter().collect();
    items
        .into_iter()
        .filter(|(path, _)| unimported.contains(path))
        .collect()
}

/// Start the backend import job and detach the event watcher. Returns
/// `false` when a job of either import kind is already running.
fn start_import_job(
    controller: &Entity<LibraryController>,
    manager: &TaskManager,
    kind: TaskKind,
    options: ImportOptions,
    total: usize,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // One import at a time across both kinds: the controller's phase is
    // shared, and two writer threads would race the progress reporting.
    if manager.is_running(TaskKind::Import) || manager.is_running(TaskKind::CollectInbox) {
        return false;
    }
    let Ok((task_id, rx)) = manager.start(kind, kind.name(), move |ctx| import::run(&options, ctx))
    else {
        return false;
    };

    controller.update(cx, |ctl, _| {
        ctl.begin_import(total);
        ctl.import_task = Some(ImportTaskHandle {
            manager: manager.clone(),
            task_id,
        });
    });
    // `total == 0` is the job's "still counting the folder" state: the scan
    // runs on the backend thread and the real total arrives as a progress
    // event, so the first toast must not claim "0 files".
    let started = if total == 0 {
        rust_i18n::t!("notice.import_scanning").to_string()
    } else {
        rust_i18n::t!("notice.import_started", count = total).to_string()
    };
    window.push_notification(
        Notification::info(started)
            .id1::<ImportNotice>("import-progress")
            .action(cancel_button(controller.clone())),
        cx,
    );

    let handle = window.window_handle();
    watch_import(controller.clone(), manager.clone(), task_id, rx, handle, cx);
    true
}

/// Poll the task manager until the import settles, translating events into
/// controller state and toasts. Runs on the foreground executor; each turn
/// sleeps [`POLL_INTERVAL`] so the UI never blocks.
fn watch_import(
    controller: Entity<LibraryController>,
    manager: TaskManager,
    task_id: TaskId,
    rx: std::sync::mpsc::Receiver<ImportOutcome>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        loop {
            let mut settled: Option<Notification> = None;
            // Whether the job is over, independently of whether there is a
            // toast to show for it: the watcher must stop either way.
            let mut finished = false;
            for event in manager.poll_events() {
                let TaskEvent::Progress { done, total, id } = &event else {
                    if event_task_id(&event) != Some(task_id) {
                        continue;
                    }
                    match event {
                        TaskEvent::Failed { error, .. } => {
                            settled = Some(
                                Notification::warning(
                                    rust_i18n::t!("workspace.trash_failed", error = error)
                                        .to_string(),
                                )
                                .id1::<ImportNotice>("import-progress"),
                            );
                            controller.update(cx, |ctl, cx| {
                                ctl.finish_import(0, 0);
                                cx.notify();
                            });
                        }
                        TaskEvent::Cancelled { .. } => {
                            settled = Some(
                                Notification::info(
                                    rust_i18n::t!("notice.import_cancelled").to_string(),
                                )
                                .id1::<ImportNotice>("import-progress"),
                            );
                            controller.update(cx, |ctl, cx| {
                                ctl.finish_import(0, 0);
                                cx.notify();
                            });
                        }
                        _ => {}
                    }
                    continue;
                };
                if *id != task_id {
                    continue;
                }
                controller.update(cx, |ctl, cx| {
                    ctl.import_progress(*done as usize);
                    cx.notify();
                });
                let _ = handle.update(cx, |_view, window, cx| {
                    // No total yet: the job is still walking the folders it was
                    // handed, so the bar has nothing to be a fraction of.
                    let text = if *total == 0 {
                        rust_i18n::t!("notice.import_scanning").to_string()
                    } else {
                        rust_i18n::t!("notice.import_running", done = done, total = total)
                            .to_string()
                    };
                    window.push_notification(
                        Notification::info(text)
                            .id1::<ImportNotice>("import-progress")
                            .action(cancel_button(controller.clone())),
                        cx,
                    );
                });
            }

            match rx.try_recv() {
                Ok(outcome) => {
                    controller.update(cx, |ctl, cx| {
                        ctl.finish_import(
                            outcome.report.imported_count(),
                            outcome.report.skipped_count(),
                        );
                        cx.notify();
                    });
                    settled = outcome_toast(&outcome);
                    finished = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // No value: failed or cancelled — `settled` was set from
                    // the terminal event above. If it somehow was not, stop
                    // watching anyway so the poller never spins forever.
                    if settled.is_none() {
                        controller.update(cx, |ctl, cx| {
                            ctl.finish_import(0, 0);
                            cx.notify();
                        });
                    }
                    finished = true;
                }
            }

            if finished {
                let _ = handle.update(cx, |_view, window, cx| match settled {
                    Some(note) => window.push_notification(note, cx),
                    // Nothing worth saying — the whole batch was already
                    // imported. The progress toast carries the cancel button,
                    // so it never ages out on its own; leaving it behind is
                    // what turned a no-op inbox drain into a permanent
                    // "importing…" in the corner.
                    None => window.remove_notification1::<ImportNotice>("import-progress", cx),
                });
                break;
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
    })
    .detach();
}

fn event_task_id(event: &TaskEvent) -> Option<TaskId> {
    match event {
        TaskEvent::Started { id, .. }
        | TaskEvent::Progress { id, .. }
        | TaskEvent::Completed { id, .. }
        | TaskEvent::Failed { id, .. }
        | TaskEvent::Cancelled { id, .. } => Some(*id),
    }
}

/// The completion toast: success when everything landed, a warning listing
/// skips otherwise, a plain info when the run was cancelled, an error naming
/// the database problem when batches failed. `None` for the run that did
/// nothing at all — the resident inbox sweep re-runs over its whole history
/// and ends "everything already imported"; toasting that every time would
/// train the user to dismiss import notices unread.
fn outcome_toast(outcome: &ImportOutcome) -> Option<Notification> {
    // Every variant carries the progress toast's id: with the cancel button
    // attached the progress toast never auto-hides, so the outcome must
    // *replace* it, not stack beside it.
    let keyed = |note: Notification| note.id1::<ImportNotice>("import-progress");
    if outcome.cancelled {
        Some(keyed(Notification::info(
            rust_i18n::t!("notice.import_cancelled").to_string(),
        )))
    } else if let Some(error) = &outcome.error {
        Some(keyed(Notification::warning(
            rust_i18n::t!(
                "notice.import_error",
                error = error,
                imported = outcome.report.imported_count(),
                skipped = outcome.report.skipped_count()
            )
            .to_string(),
        )))
    } else if outcome.report.imported.is_empty()
        && outcome.report.skipped.is_empty()
        && outcome.report.already_imported > 0
    {
        None
    } else if outcome.report.skipped.is_empty() {
        Some(keyed(Notification::success(
            rust_i18n::t!(
                "notice.import_done",
                imported = outcome.report.imported_count()
            )
            .to_string(),
        )))
    } else {
        Some(keyed(Notification::warning(
            rust_i18n::t!(
                "notice.import_done_skipped",
                imported = outcome.report.imported_count(),
                skipped = outcome.report.skipped_count()
            )
            .to_string(),
        )))
    }
}

/// The progress toast's cancel button: asks the job to stop at its next
/// checkpoint; the outcome toast replaces this one when the job settles.
fn cancel_button(
    controller: Entity<LibraryController>,
) -> impl Fn(&mut Notification, &mut Window, &mut gpui_kit::Context<Notification>) -> Button {
    move |_notification, _window, _cx| {
        let controller = controller.clone();
        Button::new("import-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("notice.import_cancel").to_string())
            .on_click(move |_, _, cx| {
                controller.update(cx, |ctl, _| ctl.cancel_import());
            })
    }
}

// ============================ embedding backfill =============================

/// Marker for the keyed embedding-progress toast: pushing with the same id
/// replaces the previous toast instead of stacking one per progress event.
pub struct EmbeddingNotice;

/// Start an embedding backfill from the settings page: build the provider
/// from the saved config (an OpenAI-compatible endpoint), start the
/// library's `EmbeddingBackfill` job, and watch it — a keyed toast carries
/// progress, and the outcome replaces it. The job runs on the backend task
/// thread; this only starts it and watches. Returns `false` when the config
/// is missing or incomplete (a toast says so) or a run is already going.
pub fn start_embedding_backfill_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let config = trove_core::config::AppConfig::load().ai_embedding;
    let Some(config) = config.filter(trove_core::config::EmbeddingConfig::is_configured) else {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return false;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&config) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                window.push_notification(Notification::warning(error.to_string()), cx);
                return false;
            }
        };

    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| ctl.library.start_embedding_backfill(provider));
    let Ok((task_id, rx)) = started else {
        return false; // one backfill at a time; the running toast is up
    };
    window.push_notification(
        Notification::info(rust_i18n::t!("settings.ai_running").to_string())
            .id1::<EmbeddingNotice>("embedding-progress"),
        cx,
    );
    watch_embedding(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        cx,
    );
    true
}

/// Ask the running backfill to stop at its next cancellation checkpoint (a
/// batch boundary); the outcome toast replaces the progress toast.
pub fn cancel_embedding_backfill_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let manager = controller.read(cx).library.tasks().clone();
    if let Some(task) = manager
        .snapshot()
        .into_iter()
        .find(|t| t.kind == TaskKind::EmbeddingBackfill && t.status == TaskStatus::Running)
    {
        manager.cancel(task.id);
    }
}

/// The line a connection test embeds. The content is irrelevant — only that
/// the endpoint answers with a vector at all, and how wide it is.
const PROBE_TEXT: &str = "trove connection test";

/// Prove the configured endpoint works (Settings ▸ AI): build the provider
/// exactly as the backfill does, embed one throwaway line on the background
/// executor, and record the width the server answered with — or the reason it
/// refused — on [`LibraryController::ai_probe`].
///
/// Deliberately not a task-manager job: it writes no rows, must not occupy
/// the embedding slot a real backfill needs, and its answer is one line of
/// text on the page rather than a progress bar.
pub fn test_embedding_endpoint_app(controller: &Entity<LibraryController>, cx: &mut App) {
    if controller.read(cx).ai_probe.is_running() {
        return;
    }
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_embedding
        .filter(trove_core::config::EmbeddingConfig::is_configured)
    else {
        set_ai_probe(
            controller,
            ai_probe_failure(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&config) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                set_ai_probe(controller, ai_probe_failure(error.to_string()), cx);
                return;
            }
        };

    set_ai_probe(controller, AiProbe::Running, cx);
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        // Flattened to a string on the worker: the page only needs the
        // message, and that keeps the awaited payload trivially `Send`.
        let result: Result<usize, String> = cx
            .background_executor()
            .spawn(async move {
                provider
                    .embed_texts(&[PROBE_TEXT.to_string()])
                    .map(|vectors| vectors.first().map_or(0, Vec::len))
                    .map_err(|error| error.to_string())
            })
            .await;
        let probe = match result {
            Ok(dim) if dim > 0 => AiProbe::Ok { dim },
            // A success with no vector means the server is answering
            // nonsense; report it as a failure rather than a green line.
            Ok(_) => ai_probe_failure(rust_i18n::t!("settings.ai_probe_empty").to_string()),
            Err(message) => ai_probe_failure(message),
        };
        // `notify` is enough to repaint the page (the settings view observes
        // the controller); `refresh_windows` is an `App` method and this runs
        // on a background executor, where only `AsyncApp` is in hand.
        controller.update(cx, |ctl, cx| {
            ctl.ai_probe = probe;
            cx.notify();
        });
    })
    .detach();
}

/// Delete every vector stored under the configured model (Settings ▸ AI).
///
/// A vector is a derivative of title / description / tags, so this costs a
/// re-embed and never user data — the same class of operation as clearing the
/// thumbnail cache, which is why it runs on the click and reports the row
/// count through a toast instead of asking first.
pub fn delete_embeddings_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_embedding
        .filter(trove_core::config::EmbeddingConfig::is_configured)
    else {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return;
    };
    let note = match controller.update(cx, |ctl, _| ctl.library.delete_embeddings(&config.model)) {
        Ok(count) => {
            Notification::success(rust_i18n::t!("settings.ai_deleted", count = count).to_string())
        }
        Err(error) => {
            Notification::warning(rust_i18n::t!("settings.job_failed", error = error).to_string())
        }
    };
    window.push_notification(note, cx);
    // The coverage line and the delete button's enabled state both read the
    // table, and both are rendered per paint.
    cx.refresh_windows();
}

/// A [`AiProbe::Failed`] carrying a localized reason.
fn ai_probe_failure(reason: impl Into<String>) -> AiProbe {
    AiProbe::Failed {
        message: reason.into(),
    }
}

/// Record a probe result and repaint, so the page shows it whether the test
/// finished on the UI thread (bad configuration) or a worker (a real call).
fn set_ai_probe(controller: &Entity<LibraryController>, probe: AiProbe, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.ai_probe = probe;
        cx.notify();
    });
    cx.refresh_windows();
}

// ============================ query embedding ================================

/// Fetch the embedding for the just-committed search term (the search box
/// submits on Enter, so there is nothing to debounce), letting the grid's
/// next data pass fuse a vector leg into the text ranking.
///
/// Silent by design: no endpoint configured, an unreachable server, or a bad
/// model name all leave the search exactly as it was before hybrid ranking —
/// which is not worth a toast on every keystroke-submitted search. The
/// failure is logged instead.
///
/// The response is dropped unless the search term is still the committed one,
/// so a slow call can never paint an answer to a question the user has moved
/// past.
pub fn request_query_embedding_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let text = controller.read(cx).search_text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let already_have_it = controller
        .read(cx)
        .query_vector
        .as_ref()
        .is_some_and(|query| query.text == text);
    if already_have_it {
        return;
    }
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_embedding
        .filter(trove_core::config::EmbeddingConfig::is_configured)
    else {
        return;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&config) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                tracing::warn!(%error, "query embedding skipped: endpoint is misconfigured");
                return;
            }
        };
    let model = config.model.trim().to_string();
    let space = provider.asset_space();
    let controller = controller.clone();

    cx.spawn(async move |cx| {
        let asked = text.clone();
        let result: Result<Vec<f32>, String> = cx
            .background_executor()
            .spawn(async move {
                provider
                    .embed_texts(std::slice::from_ref(&asked))
                    .map(|mut vectors| vectors.pop().unwrap_or_default())
                    .map_err(|error| error.to_string())
            })
            .await;
        let vector = match result {
            Ok(vector) if !vector.is_empty() => vector,
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(%error, "query embedding failed; searching with text only");
                return;
            }
        };
        controller.update(cx, |ctl, cx| {
            if ctl.search_text.trim() != text {
                return; // the user moved on while the request was in flight
            }
            ctl.query_vector = Some(trove_core::search::vector::QueryVector {
                text,
                model,
                space,
                vector,
            });
            // Re-run the query so the fused ranking replaces the text-only
            // one that is on screen right now.
            ctl.generation += 1;
            cx.notify();
        });
    })
    .detach();
}

/// Poll the backfill until it settles, translating events into toasts — the
/// same shape as [`watch_import`]. A settle also refreshes the windows so
/// the settings page's coverage line and generate/cancel button reflect the
/// new state.
fn watch_embedding(
    controller: Entity<LibraryController>,
    manager: TaskManager,
    task_id: TaskId,
    rx: std::sync::mpsc::Receiver<EmbedOutcome>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let mut settled: Option<Notification> = None;
            for event in manager.poll_events() {
                if event_task_id(&event) != Some(task_id) {
                    continue;
                }
                match event {
                    TaskEvent::Progress { done, total, .. } => {
                        let _ = handle.update(cx, |_view, window, cx| {
                            window.push_notification(
                                Notification::info(
                                    rust_i18n::t!(
                                        "settings.ai_running_progress",
                                        done = done,
                                        total = total
                                    )
                                    .to_string(),
                                )
                                .id1::<EmbeddingNotice>("embedding-progress"),
                                cx,
                            );
                        });
                    }
                    TaskEvent::Failed { error, .. } => {
                        settled = Some(keyed_embedding(Notification::warning(
                            rust_i18n::t!("settings.ai_failed", error = error).to_string(),
                        )));
                    }
                    TaskEvent::Cancelled { .. } => {
                        settled = Some(keyed_embedding(Notification::info(
                            rust_i18n::t!("settings.ai_cancelled").to_string(),
                        )));
                    }
                    _ => {}
                }
            }

            match rx.try_recv() {
                Ok(outcome) => {
                    settled = Some(embedding_outcome_toast(&outcome));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // No value: the terminal event above already set the
                    // toast. Nothing to add — just stop watching.
                }
            }

            if let Some(note) = settled {
                let _ = handle.update(cx, |_view, window, cx| {
                    window.push_notification(note, cx);
                });
                controller.update(cx, |_, cx| cx.refresh_windows());
                break;
            }
        }
    })
    .detach();
}

/// Attach the progress toast's key so a settle replaces it, never stacks.
fn keyed_embedding(note: Notification) -> Notification {
    note.id1::<EmbeddingNotice>("embedding-progress")
}

/// The completion toast for a backfill that returned a value: success when
/// nothing failed, a warning naming the failures otherwise. The `error`
/// case (a fatal stop) is handled from the terminal event, not here.
fn embedding_outcome_toast(outcome: &EmbedOutcome) -> Notification {
    if outcome.failed > 0 {
        keyed_embedding(Notification::warning(
            rust_i18n::t!(
                "settings.ai_done",
                embedded = outcome.embedded,
                skipped = outcome.skipped,
                failed = outcome.failed
            )
            .to_string(),
        ))
    } else {
        keyed_embedding(Notification::success(
            rust_i18n::t!(
                "settings.ai_done",
                embedded = outcome.embedded,
                skipped = outcome.skipped,
                failed = outcome.failed
            )
            .to_string(),
        ))
    }
}
