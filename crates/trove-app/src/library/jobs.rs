//! Import bridge: the pipeline itself lives in `trove_core::tasks::import`
//! and runs on a backend thread managed by the library's `TaskManager`. This
//! module is only the translator between those jobs and the UI: it starts
//! jobs, then polls task events to drive the controller's import phase and
//! the progress toasts, and runs the resident folder-watch task whose
//! signals become imports.

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::tasks::import::{self, ImportOptions, ImportOutcome, ImportSource};
use trove_core::tasks::watch::{self, WatchSignal};
use trove_core::tasks::{TaskEvent, TaskId, TaskKind, TaskManager};

use crate::library::LibraryController;

/// Marker type for the import progress toast: pushing with the same id
/// replaces the previous toast instead of stacking a new one.
pub struct ImportNotice;

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
    let (tx, rx) = std::sync::mpsc::channel();
    // The other half of the watcher's retry contract: whatever the pump accepts
    // is reported back, so the job stops offering it. Without this the sweep
    // re-offered every file that arrived after the baseline, and offering one
    // costs a hash — for as long as the process lived.
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let started = manager.start(TaskKind::WatchScan, "watch", move |ctx| {
        watch::run(
            watch::WATCH_INTERVAL,
            library_dir,
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
    watch_signals(controller.clone(), rx, accepted_tx, handle, cx);
    true
}

/// Consume watch signals: inbox discoveries start a collect import; fresh
/// files under watch roots queue for import, retried until accepted (a
/// manual import still running refuses the batch, and the queue keeps the
/// files pending — the same contract the old watcher loop had).
fn watch_signals(
    controller: Entity<LibraryController>,
    rx: std::sync::mpsc::Receiver<WatchSignal>,
    accepted: std::sync::mpsc::Sender<Vec<PathBuf>>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let mut pending: Vec<PathBuf> = Vec::new();
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let mut channel_open = true;
            loop {
                match rx.try_recv() {
                    Ok(WatchSignal::Inbox) => {
                        let _ = handle.update(cx, |_view, window, cx| {
                            collect_inbox_app(&controller, window, cx);
                        });
                    }
                    Ok(WatchSignal::Files(files)) => pending.extend(files),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        channel_open = false;
                        break;
                    }
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
                // thing, so it stops offering files it has already handed over.
                if accepted_flag {
                    let _ = accepted.send(pending.clone());
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
            // A user import links: the file stays where the user keeps it.
            storage: trove_core::media::import::ImportStorage::Link,
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

/// Drain the collect-service inbox: import every waiting file (unfiled,
/// `source_url` stamped from the sidecar, files deleted afterwards).
/// Returns `false` when the inbox was empty or an import is already running
/// (retry on the next watcher cycle).
pub fn collect_inbox_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let inbox = trove_core::services::collect::inbox_dir();
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return false;
    };
    // Files with an optional sidecar; sidecars themselves are not imports.
    let mut items: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".meta.json"))
                .unwrap_or(true)
        {
            continue;
        }
        let sidecar = {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            inbox.join(format!("{name}.meta.json"))
        };
        items.push((path, sidecar.is_file().then_some(sidecar)));
    }
    if items.is_empty() {
        return false;
    }

    let total = items.len();
    let (manager, options) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return false;
        }
        let options = ImportOptions {
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            // A collected file lives in the incoming directory, which is not a
            // scratch area — the import leaves it there and links it.
            storage: trove_core::media::import::ImportStorage::Link,
            source: ImportSource::CollectInbox { items },
        };
        (ctl.library.tasks().clone(), options)
    };

    start_import_job(
        controller,
        &manager,
        TaskKind::CollectInbox,
        options,
        total,
        window,
        cx,
    )
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

    controller.update(cx, |ctl, _| ctl.begin_import(total));
    // `total == 0` is the job's "still counting the folder" state: the scan
    // runs on the backend thread and the real total arrives as a progress
    // event, so the first toast must not claim "0 files".
    let started = if total == 0 {
        rust_i18n::t!("notice.import_scanning").to_string()
    } else {
        rust_i18n::t!("notice.import_started", count = total).to_string()
    };
    window.push_notification(
        Notification::info(started).id1::<ImportNotice>("import-progress"),
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
            for event in manager.poll_events() {
                let TaskEvent::Progress { done, total, id } = &event else {
                    if event_task_id(&event) != Some(task_id) {
                        continue;
                    }
                    match event {
                        TaskEvent::Failed { error, .. } => {
                            settled = Some(Notification::warning(
                                rust_i18n::t!("workspace.trash_failed", error = error).to_string(),
                            ));
                            controller.update(cx, |ctl, cx| {
                                ctl.finish_import(0, 0);
                                cx.notify();
                            });
                        }
                        TaskEvent::Cancelled { .. } => {
                            settled = Some(Notification::info(
                                rust_i18n::t!("notice.import_cancelled").to_string(),
                            ));
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
                        Notification::info(text).id1::<ImportNotice>("import-progress"),
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
                    settled = Some(outcome_toast(&outcome));
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
                }
            }

            if let Some(note) = settled {
                let _ = handle.update(cx, |_view, window, cx| {
                    window.push_notification(note, cx);
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
/// skips otherwise, a plain info when the run was cancelled.
fn outcome_toast(outcome: &ImportOutcome) -> Notification {
    if outcome.cancelled {
        Notification::info(rust_i18n::t!("notice.import_cancelled").to_string())
    } else if outcome.report.skipped.is_empty() {
        Notification::success(
            rust_i18n::t!(
                "notice.import_done",
                imported = outcome.report.imported_count()
            )
            .to_string(),
        )
    } else {
        Notification::warning(
            rust_i18n::t!(
                "notice.import_done_skipped",
                imported = outcome.report.imported_count(),
                skipped = outcome.report.skipped_count()
            )
            .to_string(),
        )
    }
}
