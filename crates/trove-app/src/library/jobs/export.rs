//! The export job bridge: `trove_core::tasks::export` runs the whole
//! hand-off on a backend thread; this submodule starts the job (from the
//! export dialog's plan), then polls its events through the shared keyed
//! toast — the same shape every other job bridge has.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::tasks::export::{self, ExportItem, ExportOptions, ExportOutcome};
use trove_core::tasks::{TaskKind, TaskPriority};

use super::{NoticeKey, watch_job};
use crate::library::{LibraryController, Retryable};

/// Marker for the keyed export progress toast.
pub struct ExportNotice;

impl NoticeKey for ExportNotice {
    const ID: &'static str = "export-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(rust_i18n::t!("export.running", done = done, total = total).to_string())
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("export.failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("export.cancelled").to_string())
    }
}

/// Start the export job over a plan the dialog froze (items with resolved
/// sources, per-kind targets, destination). Returns `false` when an export
/// is already running — one at a time, like every other kind.
pub fn start_export_job_app(
    controller: &Entity<LibraryController>,
    items: Vec<ExportItem>,
    mut options: ExportOptions,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    if manager.is_active(&TaskKind::Export) {
        return false;
    }
    options.items = items;
    let total = options.items.len();
    let retry_options = options.clone();
    let label = TaskKind::Export.name().to_string();
    let Ok((task_id, rx)) = manager.start_with_priority(
        TaskKind::Export,
        label.clone(),
        TaskPriority::High,
        move |ctx| export::run(&options, ctx),
    ) else {
        return false;
    };

    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Export {
            options: retry_options,
        });
        ctl.begin_task(task_id, TaskKind::Export, label);
    });
    window.push_notification(
        ExportNotice::keyed(
            Notification::info(rust_i18n::t!("export.started", count = total).to_string())
                .action(export_cancel_button(controller.clone(), task_id)),
        ),
        cx,
    );

    watch_job::<ExportNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        // The closing toast reads the run's own report; a cancel arrives as
        // a Cancelled *event* (the manager drops the value then), so the
        // outcome seen here is always a settled run.
        |outcome: &ExportOutcome| {
            if outcome.report.failed.is_empty() {
                Some(ExportNotice::keyed(Notification::success(
                    rust_i18n::t!("export.done", count = outcome.report.written.len()).to_string(),
                )))
            } else {
                Some(ExportNotice::keyed(Notification::warning(
                    rust_i18n::t!(
                        "export.done_failed",
                        written = outcome.report.written.len(),
                        failed = outcome.report.failed.len()
                    )
                    .to_string(),
                )))
            }
        },
        // The task rows carry the whole export state; nothing extra to unwind.
        |_, _| {},
        cx,
    );
    true
}

/// The progress toast's cancel button: asks the job to stop after the
/// current item (a running ffmpeg encode is killed within a poll tick).
fn export_cancel_button(
    controller: Entity<LibraryController>,
    task_id: trove_core::tasks::TaskId,
) -> impl Fn(&mut Notification, &mut Window, &mut gpui_kit::Context<Notification>) -> Button {
    move |_notification, _window, _cx| {
        let controller = controller.clone();
        Button::new("export-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("notice.import_cancel").to_string())
            .on_click(move |_, _, cx| {
                controller.update(cx, |ctl, cx| {
                    ctl.cancel_task(task_id);
                    cx.notify();
                });
            })
    }
}
