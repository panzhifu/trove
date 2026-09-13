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

/// Start the resident watch task: a backend thread sweeps the collect
/// inbox and the configured watch roots every [`watch::WATCH_INTERVAL`] and
/// reports discoveries; this module's pump turns them into imports. Call
/// once at startup and again after a library swap.
pub fn start_watch_service(
    controller: &Entity<LibraryController>,
    handle: gpui::AnyWindowHandle,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let started = manager.start(TaskKind::WatchScan, "watch", move |ctx| {
        watch::run(
            watch::WATCH_INTERVAL,
            trove_core::services::collect::inbox_dir(),
            tx,
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
    watch_signals(controller.clone(), rx, handle, cx);
    true
}

/// Consume watch signals: inbox discoveries start a collect import; fresh
/// files under watch roots queue for import, retried until accepted (a
/// manual import still running refuses the batch, and the queue keeps the
/// files pending — the same contract the old watcher loop had).
fn watch_signals(
    controller: Entity<LibraryController>,
    rx: std::sync::mpsc::Receiver<WatchSignal>,
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
                let accepted = handle
                    .update(cx, |_view, window, cx| {
                        if controller.read(cx).is_importing() {
                            return false;
                        }
                        import_paths_app_into(&controller, pending.clone(), None, window, cx)
                    })
                    .unwrap_or(false);
                // Mark seen only after the batch was accepted; a refusal
                // retries on the next pump tick.
                if accepted {
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
        // Expand before the count so progress totals cover the folder
        // contents, not just the dropped entries. The walk shares the
        // watcher's rules (dotfiles skipped, depth capped) and stays on the
        // main thread like the watcher's own scan.
        let expanded = import::expand_dirs(paths.clone());
        if expanded.is_empty() {
            return false;
        }
        let total = expanded.len();
        let library_root = ctl.library.root().to_path_buf();
        // The import mode (copy vs link) is read at call time from the config.
        let linked = trove_core::config::AppConfig::load().import_linked();
        let options = ImportOptions {
            db_path: library_root.join("library.db"),
            library_root,
            linked,
            source: ImportSource::Paths {
                paths: expanded,
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
        let library_root = ctl.library.root().to_path_buf();
        // Collect-inbox files are transient copies; they are always stored.
        let options = ImportOptions {
            db_path: library_root.join("library.db"),
            library_root,
            linked: false,
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
    window.push_notification(
        Notification::info(rust_i18n::t!("notice.import_started", count = total).to_string())
            .id1::<ImportNotice>("import-progress"),
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
                    window.push_notification(
                        Notification::info(
                            rust_i18n::t!("notice.import_running", done = done, total = total)
                                .to_string(),
                        )
                        .id1::<ImportNotice>("import-progress"),
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
