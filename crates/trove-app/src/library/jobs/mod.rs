//! Background job bridge: the translator between the backend
//! [`trove_core::tasks::TaskManager`] jobs and the UI. Every kind shares one
//! shape — start a job, then poll that job's events (via
//! [`TaskManager::poll_events_for`]) to drive the controller's state and a
//! keyed progress toast. That loop is [`watch_job`]; what a kind brings to it
//! is its [`NoticeKey`] impl (the toast's key and the wording of each state)
//! and, at the call site, what its completion says and any bookkeeping the
//! shared task rows do not cover.
//!
//! Each kind lives in its own submodule; the public entry points are
//! re-exported from here, so callers keep using `crate::library::jobs::…`.

mod analysis;
mod embedding;
mod import;
mod search;
mod watch;
mod xmp;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AnyWindowHandle, App, Entity};

use crate::library::LibraryController;
use trove_core::tasks::{TaskEvent, TaskId, TaskManager, TaskStatus};

/// Poll cadence for task events; the backend throttles progress to ~10/s, so
/// this keeps the bar smooth without busy-looping.
///
/// Deliberately a cadence and not [`TaskManager::wait_events_for`]: that parks
/// its thread, and a watcher runs on gpui's background pool, which is a fixed
/// set of shared workers — one sleeper is one worker no other background task
/// can run on, and there can be several watchers live at once. An idle poll is
/// now a hash lookup plus taking one job's bucket, so the cadence buys the
/// safety without the O(queue) scan it used to cost.
pub(super) const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

/// The part of a watcher that depends only on which job it is watching: the
/// id its toast replaces itself under, and what each of the states the task
/// manager can report says. The completion wording is *not* here, because one
/// kind can settle with more than one outcome shape — an analysis run and its
/// undo share a key yet return different values — so it is a closure at the
/// call site instead.
pub(super) trait NoticeKey: Sized + 'static {
    /// The notification id every notice of this job carries, so a new one
    /// replaces the previous instead of stacking a toast per progress event.
    const ID: &'static str;

    /// The running toast. `controller` is there for the kinds whose toast
    /// carries a button (import's cancel); the others ignore it.
    fn running(controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification;

    /// The toast for a job the task manager stopped with an error.
    fn failed(error: &str) -> Notification;

    /// The toast for a job stopped by the user.
    fn cancelled() -> Notification;

    /// Attach this kind's key, so pushing replaces whatever is on screen.
    fn keyed(note: Notification) -> Notification {
        note.id1::<Self>(Self::ID)
    }
}

/// A transition [`watch_job`] reacted to, handed to its `on_event` hook so a
/// kind can update the state the shared task rows do not cover.
pub(super) enum JobStep<'a, T> {
    Progress {
        done: u64,
        total: u64,
    },
    /// The job returned `outcome`.
    Completed(&'a T),
    /// The job ended with no outcome: it failed, was cancelled, or its worker
    /// thread went away.
    Aborted,
}

/// Poll a job until it settles, translating its events into the keyed toast
/// [`NoticeKey`] describes and the controller's task rows. `settle` turns the
/// value the job returned into the closing notice; `None` there means nothing
/// is worth saying, and the progress toast is dismissed instead. `on_event`
/// runs at each transition, before the shared bookkeeping.
///
/// Each turn sleeps [`POLL_INTERVAL`] on the background executor, so the UI
/// never blocks on the job.
#[allow(clippy::too_many_arguments)]
pub(super) fn watch_job<N: NoticeKey, T: Send + 'static>(
    controller: Entity<LibraryController>,
    manager: TaskManager,
    task_id: TaskId,
    rx: std::sync::mpsc::Receiver<T>,
    handle: AnyWindowHandle,
    settle: impl Fn(&T) -> Option<Notification> + Send + 'static,
    on_event: impl Fn(&mut LibraryController, &JobStep<'_, T>) + Send + 'static,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let mut settled: Option<Notification> = None;
            // Whether the job is over, independently of whether there is a
            // toast to show for it: the watcher must stop either way.
            let mut finished = false;
            // Take only *this* job's events — each job has its own bucket, and
            // the global drain would steal (and discard) the progress events a
            // concurrent import / embedding / analysis run is waiting on for
            // its own watcher, which is exactly why toast numbers used to
            // freeze.
            for event in manager.poll_events_for(task_id) {
                match event {
                    TaskEvent::Progress { done, total, .. } => {
                        controller.update(cx, |ctl, cx| {
                            on_event(ctl, &JobStep::Progress { done, total });
                            ctl.update_task(task_id, done, total);
                            cx.notify();
                        });
                        let _ = handle.update(cx, |_view, window, cx| {
                            window.push_notification(
                                N::keyed(N::running(&controller, done, total)),
                                cx,
                            );
                        });
                    }
                    TaskEvent::Paused { .. } => {
                        controller.update(cx, |ctl, cx| {
                            ctl.set_task_status(task_id, TaskStatus::Paused);
                            cx.notify();
                        });
                    }
                    TaskEvent::Resumed { .. } => {
                        controller.update(cx, |ctl, cx| {
                            ctl.set_task_status(task_id, TaskStatus::Running);
                            cx.notify();
                        });
                    }
                    TaskEvent::Failed { error, .. } => {
                        controller.update(cx, |ctl, cx| {
                            on_event(ctl, &JobStep::Aborted);
                            ctl.set_task_status(task_id, TaskStatus::Failed);
                            cx.notify();
                        });
                        settled = Some(N::keyed(N::failed(&error)));
                    }
                    TaskEvent::Cancelled { .. } => {
                        controller.update(cx, |ctl, cx| {
                            on_event(ctl, &JobStep::Aborted);
                            ctl.set_task_status(task_id, TaskStatus::Cancelled);
                            cx.notify();
                        });
                        settled = Some(N::keyed(N::cancelled()));
                    }
                    // `Started` and `Completed` are not acted on: the value the
                    // job hands back below is what says *what* completed.
                    _ => {}
                }
            }

            match rx.try_recv() {
                Ok(outcome) => {
                    controller.update(cx, |ctl, cx| {
                        on_event(ctl, &JobStep::Completed(&outcome));
                        ctl.set_task_status(task_id, TaskStatus::Completed);
                        cx.notify();
                    });
                    settled = settle(&outcome).map(N::keyed);
                    finished = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                // No value: a terminal event above said whatever there is to
                // say — unless there was none, which means the worker died
                // without reporting. Either way stop watching; a closed channel
                // will not reopen.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if settled.is_none() {
                        controller.update(cx, |ctl, cx| {
                            on_event(ctl, &JobStep::Aborted);
                            ctl.set_task_status(task_id, TaskStatus::Failed);
                            cx.notify();
                        });
                    }
                    finished = true;
                }
            }

            if finished || settled.is_some() {
                let _ = handle.update(cx, |_view, window, cx| match settled {
                    Some(note) => window.push_notification(note, cx),
                    // Nothing worth saying: the progress toast is the one with
                    // a cancel button, which never ages out on its own, so
                    // leaving it behind is what turned a no-op inbox drain into
                    // a permanent "importing…" in the corner.
                    None => window.remove_notification1::<N>(N::ID, cx),
                });
                // Views that read the job's running state per paint — the
                // settings page's buttons, the coverage line, the panel's
                // rows — need the repaint, and so does anywhere the job just
                // moved data (labels, the grid).
                controller.update(cx, |_, cx| cx.refresh_windows());
                break;
            }
        }
    })
    .detach();
}

pub use self::{
    analysis::{
        AnalysisTarget, cancel_analysis_app, pause_task_app, resume_task_app, retry_task_app,
        start_analysis_app, start_analysis_undo_app, test_analysis_endpoint_app,
    },
    embedding::{
        cancel_embedding_backfill_app, delete_embeddings_app, start_embedding_backfill_app,
        test_embedding_endpoint_app,
    },
    import::{
        ImportTaskHandle, InboxDrain, collect_inbox_app, import_copied_app, import_paths_app,
    },
    search::{request_ai_plan_app, request_query_embedding_app},
    watch::{WatchTask, start_index_drain_service, start_watch_service},
    xmp::export_xmp_app,
};
