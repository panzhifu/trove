//! The background-removal model's UI bridge: ask, download, continue.
//!
//! Shaped like [`super::local_model`] because it is the same problem: the
//! engine needs 168 MB of weights before anything runs, the ask belongs at
//! the moment something actually needs them, and the action that asked has to
//! resume on its own when the download lands — the user clicked once. The one
//! difference is what "the action" is: here it is the selection to cut out,
//! carried as a plain id list.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Entity, Window};
use uuid::Uuid;

use trove_core::services::local_model::{self, ModelStatus, U2NET};

use crate::library::{LibraryController, ModelDownload};

/// Whether the caller may proceed right now. `true` = the checkpoint is on
/// disk. `false` = the dialog was shown (and the download, if accepted, is
/// already running; `then` re-launches the cutout when it lands).
pub fn ensure_u2net_app(
    controller: &Entity<LibraryController>,
    then: Option<Vec<Uuid>>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if matches!(local_model::status(&U2NET), ModelStatus::Ready { .. }) {
        return true;
    }
    if controller
        .read(cx)
        .matting_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return false;
    }

    let controller = controller.clone();
    let then = then.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let controller = controller.clone();
        let then = then.clone();
        alert
            .title(rust_i18n::t!("matting.model_dialog_title").to_string())
            .description(
                rust_i18n::t!("matting.model_dialog_description", mb = U2NET.download_mb)
                    .to_string(),
            )
            .confirm()
            .ok_text(rust_i18n::t!("settings.local_model_download_now").to_string())
            .cancel_text(rust_i18n::t!("settings.local_model_not_now").to_string())
            .on_ok(move |_, window, cx| {
                start_u2net_download_app(&controller, then.clone(), window, cx);
                true
            })
    });
    false
}

/// Run the download in the background: progress on the controller for the
/// settings page, toasts at the end, and the waiting cutout re-launched on
/// success.
pub fn start_u2net_download_app(
    controller: &Entity<LibraryController>,
    then: Option<Vec<Uuid>>,
    window: &mut Window,
    cx: &mut App,
) {
    if controller
        .read(cx)
        .matting_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return;
    }
    let handle = window.window_handle();
    controller.update(cx, |ctl, cx| {
        ctl.matting_model_download = Some(ModelDownload::Running {
            received: 0,
            total: 0,
        });
        cx.notify();
    });
    window.push_notification(
        Notification::info(rust_i18n::t!("matting.model_download_started").to_string()),
        cx,
    );

    let controller = controller.clone();
    cx.spawn(async move |cx| {
        let progress: Arc<Mutex<(u64, u64)>> = Arc::new(Mutex::new((0, 0)));
        let finished = Arc::new(AtomicBool::new(false));
        let outcome: Arc<Mutex<Option<Result<(), String>>>> = Arc::new(Mutex::new(None));

        {
            let progress = progress.clone();
            let finished = finished.clone();
            let outcome = outcome.clone();
            cx.background_executor()
                .spawn(async move {
                    let result = local_model::download(
                        &U2NET,
                        &AtomicBool::new(false),
                        &|received, total| {
                            *progress.lock().unwrap() = (received, total);
                        },
                    );
                    *outcome.lock().unwrap() =
                        Some(result.map(|_| ()).map_err(|error| error.to_string()));
                    finished.store(true, Ordering::Relaxed);
                })
                .detach();
        }

        // The download blocks its worker; this loop keeps the settings page
        // fed at a toast-friendly cadence until it settles.
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            if finished.load(Ordering::Relaxed) {
                break;
            }
            let (received, total) = *progress.lock().unwrap();
            controller.update(cx, |ctl, cx| {
                ctl.matting_model_download = Some(ModelDownload::Running { received, total });
                cx.notify();
            });
        }

        let result = outcome.lock().unwrap().clone();
        controller.update(cx, |ctl, cx| {
            ctl.matting_model_download = match result.as_ref() {
                Some(Ok(())) => None,
                Some(Err(message)) => Some(ModelDownload::Failed {
                    message: message.clone(),
                }),
                None => ctl.matting_model_download.clone(),
            };
            cx.notify();
        });
        let _ = handle.update(cx, |_view, window, cx| {
            match result.as_ref() {
                Some(Ok(())) => {
                    window.push_notification(
                        Notification::success(
                            rust_i18n::t!("matting.model_download_done").to_string(),
                        ),
                        cx,
                    );
                    // The action that asked for the model proceeds now — one
                    // click total, even though 168 MB had to arrive in between.
                    if let Some(ids) = then {
                        let path = match local_model::status(&U2NET) {
                            ModelStatus::Ready { path } => path,
                            ModelStatus::Missing => return,
                        };
                        super::matting::start_cutout_app(&controller, ids, path, window, cx);
                    }
                }
                Some(Err(message)) => {
                    window.push_notification(
                        Notification::warning(
                            rust_i18n::t!("matting.model_download_failed", error = message.clone())
                                .to_string(),
                        ),
                        cx,
                    );
                }
                None => {}
            }
        });
    })
    .detach();
}

/// Delete the managed checkpoint after a confirm, guarded against a download
/// in flight and a running cutout (the job holds the graph, which it read out
/// of that file, for its whole run).
pub fn delete_u2net_app(controller: &Entity<LibraryController>, window: &mut Window, cx: &mut App) {
    if controller
        .read(cx)
        .matting_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.model_delete_busy_download").to_string()),
            cx,
        );
        return;
    }
    if controller
        .read(cx)
        .library
        .tasks()
        .is_active(&trove_core::tasks::TaskKind::Matting)
    {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.model_delete_busy_run").to_string()),
            cx,
        );
        return;
    }
    let controller = controller.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let controller = controller.clone();
        alert
            .title(rust_i18n::t!("settings.model_delete_title").to_string())
            .description(
                rust_i18n::t!("settings.model_delete_body", mb = U2NET.download_mb).to_string(),
            )
            .confirm()
            .ok_text(rust_i18n::t!("settings.model_delete").to_string())
            .cancel_text(rust_i18n::t!("settings.local_model_not_now").to_string())
            .on_ok(move |_, window, cx| {
                match local_model::delete(&U2NET) {
                    Ok(()) => {
                        window.push_notification(
                            Notification::success(
                                rust_i18n::t!("settings.model_deleted").to_string(),
                            ),
                            cx,
                        );
                    }
                    Err(error) => {
                        window.push_notification(Notification::warning(error.to_string()), cx);
                    }
                }
                controller.update(cx, |_, cx| cx.notify());
                true
            })
    });
}
