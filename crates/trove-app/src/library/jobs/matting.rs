//! Background removal: the run, and what a finished run leaves behind.
//!
//! One action, one shape whether it is asked for one image or five hundred: a
//! task. The inference is about two seconds per image on a desktop CPU, so a
//! single asset is a job that finishes quickly rather than a job worth its own
//! code path — and the task panel gets to say what happened, with a cancel
//! that works and a retry that re-asks the same selection.
//!
//! The run writes its PNGs to the incoming directory and stops there; the
//! import that files them belongs to the app, and it happens through a
//! controller slot the root observer picks up — the watcher has no window to
//! import with. The model's own gate is in [`super::matting_model`].

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use uuid::Uuid;

use trove_core::services::local_model::{self, ModelStatus, U2NET};
use trove_core::tasks::TaskKind;
use trove_core::tasks::matting::CutoutOutcome;

use super::{JobStep, NoticeKey, watch_job};
use crate::library::{LibraryController, Retryable};

/// Marker for the keyed cutout toast.
pub struct CutoutNotice;

impl NoticeKey for CutoutNotice {
    const ID: &'static str = "matting-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(rust_i18n::t!("matting.running", done = done, total = total).to_string())
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("matting.failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("matting.cancelled").to_string())
    }
}

/// Cut the given images out, asking for the model first when it is not on
/// disk. Returns whether the run started.
pub fn cutout_assets_app(
    controller: &Entity<LibraryController>,
    ids: Vec<Uuid>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if ids.is_empty() {
        return false;
    }
    match local_model::status(&U2NET) {
        ModelStatus::Ready { path } => start_cutout_app(controller, ids, path, window, cx),
        // The ask happens here — the one launch point, shared by the menu and
        // the retry button — and when the download lands this same selection
        // re-launches itself. One click either way.
        ModelStatus::Missing => {
            super::matting_model::ensure_u2net_app(controller, Some(ids), window, cx);
            false
        }
    }
}

/// The run itself, with the checkpoint's location already settled.
pub fn start_cutout_app(
    controller: &Entity<LibraryController>,
    ids: Vec<Uuid>,
    model_path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| {
        let options = ctl.library.cutout_options(&ids, model_path);
        ctl.library.start_cutout(options)
    });
    let Ok((task_id, rx)) = started else {
        // One run at a time: the kind is the mutual-exclusion key.
        window.push_notification(
            Notification::warning(rust_i18n::t!("matting.busy").to_string()),
            cx,
        );
        return false;
    };
    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Cutout { ids: ids.clone() });
        ctl.begin_task(
            task_id,
            TaskKind::Matting,
            TaskKind::Matting.name().to_string(),
        );
    });
    window.push_notification(
        CutoutNotice::keyed(Notification::info(
            rust_i18n::t!("matting.started", count = ids.len()).to_string(),
        )),
        cx,
    );

    let handle = window.window_handle();
    watch_job::<CutoutNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        handle,
        |outcome: &CutoutOutcome| Some(outcome_toast(outcome)),
        |ctl, step| {
            if let JobStep::Completed(outcome) = step
                && !outcome.written.is_empty()
            {
                ctl.pending_cutout_import = Some(outcome.written.clone());
            }
        },
        cx,
    );
    true
}

/// The completion toast: how many came out, and how many did not. A run that
/// skipped an unreadable file has to say so, or it reads exactly like a run
/// that processed it.
fn outcome_toast(outcome: &CutoutOutcome) -> Notification {
    if outcome.failed == 0 {
        Notification::success(rust_i18n::t!("matting.done", cut = outcome.cut).to_string())
    } else {
        Notification::warning(
            rust_i18n::t!(
                "matting.done_failed",
                cut = outcome.cut,
                failed = outcome.failed
            )
            .to_string(),
        )
    }
}
