//! The local transcription model's UI bridge: ask, download, continue.
//!
//! The engine needs its weights on disk before any run; the download is
//! hundreds of megabytes, so the flow asks before spending them — a dialog
//! at the moment something actually needs the model, not a background fetch
//! the user never sanctioned. Progress lands on the controller
//! ([`ModelDownload`]) for the settings page, and when the ask came
//! from an action (transcribe now), that action re-launches itself when the
//! download lands, so the second click is never needed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Entity, Window};

use trove_core::services::local_model;
use trove_core::tasks::transcription::TranscribeRunRequest;

use super::transcription::start_transcription_request_app;
use crate::library::{LibraryController, ModelDownload};

/// Whether the caller may proceed right now. `true` = a usable model is on
/// disk. `false` = the dialog was shown (and the download, if accepted, is
/// already running; `then` re-launches the action on completion).
pub fn ensure_local_model_app(
    controller: &Entity<LibraryController>,
    then: Option<TranscribeRunRequest>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if matches!(
        local_model::status(),
        local_model::ModelStatus::Ready { .. }
    ) {
        return true;
    }
    // A download already in flight answers the question: when it lands, the
    // action it was asked for re-launches itself.
    if controller
        .read(cx)
        .local_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return false;
    }

    let controller = controller.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        // The dialog's builders are `Fn` — callable for every rebuild —
        // so each layer clones what the next one hands down, and the
        // click handler owns its own copies.
        let controller = controller.clone();
        let then = then.clone();
        alert
            .title(rust_i18n::t!("settings.local_model_dialog_title").to_string())
            .description(
                rust_i18n::t!(
                    "settings.local_model_dialog_description",
                    mb = local_model::MODEL_DOWNLOAD_MB
                )
                .to_string(),
            )
            .confirm()
            .ok_text(rust_i18n::t!("settings.local_model_download_now").to_string())
            .cancel_text(rust_i18n::t!("settings.local_model_not_now").to_string())
            .on_ok(move |_, window, cx| {
                start_model_download_app(&controller, then.clone(), window, cx);
                true
            })
    });
    false
}

/// Delete the local transcription model's files after a confirm. Guarded
/// against a download in flight and a running transcription job (the
/// recogniser holds the weights open for its whole run).
pub fn delete_local_model_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    if controller
        .read(cx)
        .local_model_download
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
        .is_active(&trove_core::tasks::TaskKind::Transcription)
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
                rust_i18n::t!(
                    "settings.model_delete_body",
                    mb = local_model::MODEL_DOWNLOAD_MB
                )
                .to_string(),
            )
            .confirm()
            .ok_text(rust_i18n::t!("settings.model_delete").to_string())
            .cancel_text(rust_i18n::t!("settings.local_model_not_now").to_string())
            .on_ok(move |_, window, cx| {
                match local_model::delete() {
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

/// Run the model download in the background: progress on the controller for
/// the settings page, toasts at the end, and `then` re-launched on success.
pub fn start_model_download_app(
    controller: &Entity<LibraryController>,
    then: Option<TranscribeRunRequest>,
    window: &mut Window,
    cx: &mut App,
) {
    if controller
        .read(cx)
        .local_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return;
    }
    let handle = window.window_handle();
    controller.update(cx, |ctl, cx| {
        ctl.local_model_download = Some(ModelDownload::Running {
            received: 0,
            total: 0,
        });
        cx.notify();
    });
    window.push_notification(
        Notification::info(rust_i18n::t!("settings.local_model_download_started").to_string()),
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
                    let result =
                        local_model::download(&AtomicBool::new(false), &|received, total| {
                            *progress.lock().unwrap() = (received, total);
                        });
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
                ctl.local_model_download = Some(ModelDownload::Running { received, total });
                cx.notify();
            });
        }

        let result = outcome.lock().unwrap().clone();
        controller.update(cx, |ctl, cx| {
            ctl.local_model_download = match result.as_ref() {
                Some(Ok(())) => None,
                Some(Err(message)) => Some(ModelDownload::Failed {
                    message: message.clone(),
                }),
                None => ctl.local_model_download.clone(),
            };
            cx.notify();
        });
        let _ = handle.update(cx, |_view, window, cx| {
            match result.as_ref() {
                Some(Ok(())) => {
                    window.push_notification(
                        Notification::success(
                            rust_i18n::t!("settings.local_model_download_done").to_string(),
                        ),
                        cx,
                    );
                }
                Some(Err(message)) => {
                    window.push_notification(
                        Notification::warning(
                            rust_i18n::t!(
                                "settings.local_model_download_failed",
                                error = message.clone()
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                }
                None => {}
            }
            // The action that asked for the model proceeds now — one click
            // total, even though the model had to arrive in between.
            if result.as_ref().is_some_and(|r| r.is_ok())
                && let Some(request) = then
            {
                start_transcription_request_app(&controller, request, window, cx);
            }
        });
    })
    .detach();
}
