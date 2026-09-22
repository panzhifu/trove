//! The status-bar task center: a compact live summary (the newest running or
//! paused job and its real numbers) that expands into a `Popover` listing every
//! active and recently-settled job, each carrying pause / resume / cancel /
//! retry as its state allows.
//!
//! Split out of `app::root` because the whole surface is a pure function of the
//! [`LibraryController`]'s task center — it reads `tasks` / `retry_inputs` /
//! `task_panel_open` and nothing else about the root view.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;

// Re-export gpui's styled-building names (div, ElementId, AnyElement, Entity,
// App, SharedString, px, …) plus gpui-kit's extensions.
use gpui_kit::*;

use crate::library::{LibraryController, TaskCard, jobs};
use trove_core::tasks::{TaskId, TaskKind, TaskStatus};

/// A panel row bundled with whether its (settled) job can be retried.
struct TaskRow {
    card: TaskCard,
    can_retry: bool,
}

/// Build the task-center popover for the status bar. `controller` supplies the
/// live rows; the toggle button writes back to `task_panel_open`.
pub fn task_panel(controller: &Entity<LibraryController>, cx: &App) -> AnyElement {
    let ctl = controller.read(cx);
    let open = ctl.task_panel_open;
    let rows: Vec<TaskRow> = ctl
        .tasks
        .iter()
        .map(|card| TaskRow {
            can_retry: matches!(card.status, TaskStatus::Failed | TaskStatus::Cancelled)
                && ctl.retry_inputs(card.kind).is_some(),
            card: card.clone(),
        })
        .collect();
    let summary = task_summary(&rows);
    let body = task_panel_body(controller.clone(), &rows, cx);

    Popover::new("statusbar-tasks")
        .anchor(Anchor::BottomLeft)
        .open(open)
        .w_96()
        .on_open_change({
            let controller = controller.clone();
            move |open: &bool, _window, cx| {
                controller.update(cx, |ctl, cx| {
                    if ctl.task_panel_open != *open {
                        ctl.task_panel_open = *open;
                        cx.notify();
                    }
                });
            }
        })
        .trigger(
            Button::new("statusbar-tasks-trigger")
                .ghost()
                .xsmall()
                .label(summary),
        )
        .child(body)
        .into_any_element()
}

/// Localized label for a job kind, shown on each panel row.
fn task_kind_label(kind: TaskKind) -> SharedString {
    let key = match kind {
        TaskKind::Import | TaskKind::CollectInbox => "task.kind_import",
        TaskKind::EmbeddingBackfill | TaskKind::VisualBackfill => "task.kind_embedding",
        TaskKind::AiAnalysis | TaskKind::AutoTag => "task.kind_analysis",
        TaskKind::Maintenance => "task.kind_maintenance",
        TaskKind::WatchScan => "task.kind_watch",
        TaskKind::ModelPreview => "task.kind_model",
        TaskKind::VideoDecode | TaskKind::BatchConvert => "task.kind_convert",
    };
    rust_i18n::t!(key).to_string().into()
}

/// Localized state word for a job, colored by the caller.
fn task_status_label(status: TaskStatus) -> SharedString {
    let key = match status {
        TaskStatus::Running => "task.status_running",
        TaskStatus::Paused => "task.status_paused",
        TaskStatus::Completed => "task.status_completed",
        TaskStatus::Failed => "task.status_failed",
        TaskStatus::Cancelled => "task.status_cancelled",
    };
    rust_i18n::t!(key).to_string().into()
}

/// The compact summary drawn on the status bar when the panel is closed: the
/// newest live job and its numbers, else an attention hint, else idle.
fn task_summary(rows: &[TaskRow]) -> String {
    if let Some(row) = rows.iter().rev().find(|row| !row.card.finished()) {
        let label = task_kind_label(row.card.kind).to_string();
        if row.card.total > 0 {
            format!("{label} {}/{}", row.card.done, row.card.total)
        } else {
            format!("{label} {}", rust_i18n::t!("task.scanning"))
        }
    } else if rows
        .iter()
        .any(|row| matches!(row.card.status, TaskStatus::Failed | TaskStatus::Cancelled))
    {
        rust_i18n::t!("task.needs_attention").to_string()
    } else {
        rust_i18n::t!("task.idle").to_string()
    }
}

/// A unique element id for one of a row's action buttons (so two jobs' Pause
/// buttons never share an id and thus a hover/pressed state).
fn task_btn_id(action: &str, id: TaskId) -> ElementId {
    ElementId::Name(SharedString::from(format!("statusbar-task-{action}-{id}")))
}

/// The popover body: a header (title + a dismiss-settled button when any have
/// settled) over the list of rows, newest first.
fn task_panel_body(
    controller: Entity<LibraryController>,
    rows: &[TaskRow],
    cx: &App,
) -> AnyElement {
    let header = h_flex()
        .px_3()
        .pt_2()
        .pb_1()
        .justify_between()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_sm()
                .child(rust_i18n::t!("task.title").to_string()),
        )
        .when(rows.iter().any(|row| row.card.finished()), |head| {
            head.child(
                Button::new("statusbar-tasks-dismiss")
                    .ghost()
                    .xsmall()
                    .label(rust_i18n::t!("task.dismiss").to_string())
                    .on_click({
                        let controller = controller.clone();
                        move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                ctl.tasks.retain(|card| !card.finished());
                                cx.notify();
                            });
                        }
                    }),
            )
        });

    let body = v_flex().w_full().child(header);
    if rows.is_empty() {
        return body
            .child(
                div()
                    .px_3()
                    .py_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("task.empty").to_string()),
            )
            .into_any_element();
    }
    body.child(
        v_flex()
            .max_h(px(280.))
            .overflow_y_scrollbar()
            .children(rows.iter().rev().map(|row| task_row(&controller, row, cx))),
    )
    .into_any_element()
}

/// One job row: kind + numbers + state on the left, its action buttons on the
/// right (Pause/Cancel while running, Resume/Cancel while paused, Retry once
/// failed or cancelled and re-runnable).
fn task_row(controller: &Entity<LibraryController>, row: &TaskRow, cx: &App) -> AnyElement {
    let card = &row.card;
    let id = card.id;
    let kind = card.kind;
    let numbers = if card.total > 0 {
        format!("{}/{}", card.done, card.total)
    } else if card.finished() {
        String::new()
    } else {
        rust_i18n::t!("task.scanning").to_string()
    };
    let status_color = match card.status {
        TaskStatus::Failed => cx.theme().danger,
        TaskStatus::Cancelled => cx.theme().warning,
        TaskStatus::Completed => cx.theme().success,
        TaskStatus::Paused | TaskStatus::Running => cx.theme().muted_foreground,
    };
    let buttons: Vec<Button> = match card.status {
        TaskStatus::Running => vec![
            pause_button(controller.clone(), id),
            cancel_task_button(controller.clone(), id),
        ],
        TaskStatus::Paused => vec![
            resume_button(controller.clone(), id),
            cancel_task_button(controller.clone(), id),
        ],
        TaskStatus::Failed | TaskStatus::Cancelled if row.can_retry => {
            vec![retry_button(controller.clone(), id, kind)]
        }
        TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Completed => Vec::new(),
    };

    h_flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1p5()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().truncate().text_sm().child(task_kind_label(kind)))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(status_color)
                        .child(task_status_label(card.status))
                        .when(!numbers.is_empty(), |line| line.child(div().child(numbers))),
                ),
        )
        .children(buttons)
        .into_any_element()
}

fn pause_button(controller: Entity<LibraryController>, id: TaskId) -> Button {
    Button::new(task_btn_id("pause", id))
        .ghost()
        .xsmall()
        .icon(IconName::Pause)
        .label(rust_i18n::t!("task.pause").to_string())
        .on_click(move |_, _, cx| jobs::pause_task_app(&controller, id, cx))
}

fn resume_button(controller: Entity<LibraryController>, id: TaskId) -> Button {
    Button::new(task_btn_id("resume", id))
        .ghost()
        .xsmall()
        .icon(IconName::Play)
        .label(rust_i18n::t!("task.resume").to_string())
        .on_click(move |_, _, cx| jobs::resume_task_app(&controller, id, cx))
}

fn cancel_task_button(controller: Entity<LibraryController>, id: TaskId) -> Button {
    Button::new(task_btn_id("cancel", id))
        .ghost()
        .xsmall()
        .icon(IconName::Close)
        .label(rust_i18n::t!("task.cancel").to_string())
        .on_click(move |_, _, cx| {
            controller.update(cx, |ctl, cx| {
                ctl.cancel_task(id);
                cx.notify();
            });
        })
}

fn retry_button(controller: Entity<LibraryController>, id: TaskId, kind: TaskKind) -> Button {
    Button::new(task_btn_id("retry", id))
        .ghost()
        .xsmall()
        .icon(IconName::RotateCw)
        .label(rust_i18n::t!("task.retry").to_string())
        .on_click(move |_, window, cx| {
            jobs::retry_task_app(&controller, kind, window, cx);
        })
}
