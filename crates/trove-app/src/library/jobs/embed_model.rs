//! The local embedding model's UI bridge: ask, download, report.
//!
//! The engine needs its weights on disk before anything embeds; the
//! download is ~100 MB, so the flow asks before spending them — a dialog at
//! the moment something actually needs the model (a backfill, a connection
//! test), not a background fetch the user never sanctioned. Progress lands
//! on the controller ([`ModelDownload`]) for the settings page. Unlike the
//! transcription download there is no chained relaunch: the embedding
//! controls live on the settings page only, where the finished model turns
//! the row green and the next click is the action itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Entity, Window};

use trove_core::services::embed_model;

use crate::library::{LibraryController, ModelDownload};

/// Whether the caller may proceed right now. `true` = a usable model is on
/// disk. `false` = the dialog was shown (and the download, if accepted, is
/// already running); the caller simply stays idle until the user repeats the
/// action.
pub fn ensure_embed_model_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if matches!(
        embed_model::status(),
        embed_model::ModelStatus::Ready { .. }
    ) {
        return true;
    }
    // A download already in flight answers the question.
    if controller
        .read(cx)
        .embed_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return false;
    }

    let controller = controller.clone();
    window.open_alert_dialog(
        cx,
        move |alert, _, _| {
            let controller = controller.clone();
            alert
                .title(rust_i18n::t!("settings.embed_model_dialog_title").to_string())
                .description(
                    rust_i18n::t!(
                        "settings.embed_model_dialog_description",
                        mb = embed_model::MODEL_DOWNLOAD_MB
                    )
                    .to_string(),
                )
                .confirm()
                .ok_text(rust_i18n::t!("settings.embed_model_download_now").to_string())
                .cancel_text(rust_i18n::t!("settings.embed_model_not_now").to_string())
                .on_ok(move |_, window, cx| {
                    start_embed_model_download_app(&controller, window, cx);
                    true
                })
        },
    );
    false
}

/// Run the model download in the background: progress on the controller for
/// the settings page, toasts at the end.
pub fn start_embed_model_download_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    if controller
        .read(cx)
        .embed_model_download
        .as_ref()
        .is_some_and(ModelDownload::is_running)
    {
        return;
    }
    let handle = window.window_handle();
    controller.update(cx, |ctl, cx| {
        ctl.embed_model_download = Some(ModelDownload::Running {
            received: 0,
            total: 0,
        });
        cx.notify();
    });
    window.push_notification(
        Notification::info(rust_i18n::t!("settings.embed_model_download_started").to_string()),
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
                    let result = embed_model::download(&AtomicBool::new(false), &|received,
                                                                                    total| {
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
                ctl.embed_model_download = Some(ModelDownload::Running { received, total });
                cx.notify();
            });
        }

        let result = outcome.lock().unwrap().clone();
        controller.update(cx, |ctl, cx| {
            ctl.embed_model_download = match result.as_ref() {
                Some(Ok(())) => None,
                Some(Err(message)) => Some(ModelDownload::Failed {
                    message: message.clone(),
                }),
                None => ctl.embed_model_download.clone(),
            };
            cx.notify();
        });
        let _ = handle.update(cx, |_view, window, cx| {
            match result.as_ref() {
                Some(Ok(())) => {
                    window.push_notification(
                        Notification::success(
                            rust_i18n::t!("settings.embed_model_download_done").to_string(),
                        ),
                        cx,
                    );
                }
                Some(Err(message)) => {
                    window.push_notification(
                        Notification::warning(
                            rust_i18n::t!(
                                "settings.embed_model_download_failed",
                                error = message.clone()
                            )
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
