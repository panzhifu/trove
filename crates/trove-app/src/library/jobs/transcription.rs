//! The transcription bridge: start a speech-to-text run over selected (or all)
//! audio/video assets, then poll that job's events through the shared
//! [`watch_job`] loop — the same shape every other job speaks. This submodule
//! also holds the endpoint probe the AI settings page uses, because the probe
//! needs the same provider construction the start and retry paths share.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::model::AssetKind;
use trove_core::tasks::transcription::{TranscribeOutcome, TranscribeRunRequest};
use trove_core::tasks::{TaskKind, TaskStatus};

use super::{NoticeKey, watch_job};
use crate::library::{LibraryController, Retryable, TranscriptionProbe};

/// Marker for the keyed transcription toast.
pub struct TranscribeNotice;

impl NoticeKey for TranscribeNotice {
    const ID: &'static str = "transcribe-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(
            rust_i18n::t!("transcribe.running", done = done, total = total).to_string(),
        )
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("transcribe.failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("transcribe.cancelled").to_string())
    }
}

/// What a transcription run is asked to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeTarget {
    /// The assets selected right now, narrowed to the kinds a recogniser can
    /// hear (audio, and video with an audio track).
    Selection,
    /// Every live audio/video asset. Only the settings page asks for this,
    /// and it says so on the button.
    WholeLibrary,
}

/// Prove the transcription endpoint works (Settings ▸ AI): upload one second
/// of synthesized silence and show what came back — often nothing, which is
/// itself the proof the upload was parsed as audio.
///
/// Deliberately not a task-manager job, like the analysis probe: it writes no
/// rows and must not occupy the transcription slot a real run needs.
pub fn test_transcription_endpoint_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    if controller.read(cx).transcription_probe.is_running() {
        return;
    }
    let Some(config) = trove_core::config::AppConfig::load()
        .resolved_transcription()
        .filter(trove_core::config::TranscriptionConfig::is_configured)
    else {
        set_transcription_probe(
            controller,
            TranscriptionProbe::Failed {
                message: rust_i18n::t!("settings.transcription_not_configured").to_string(),
            },
            cx,
        );
        return;
    };
    // The local engine's probe needs its model on disk; the ask is the same
    // dialog the run path shows, with nothing chained after it.
    if config.engine == trove_core::config::TranscriptionEngine::Local
        && !super::local_model::ensure_local_model_app(controller, None, window, cx)
    {
        return;
    }
    let provider = match trove_core::ai::transcribe::build_from_config(&config) {
        Ok(provider) => provider,
        Err(error) => {
            set_transcription_probe(
                controller,
                TranscriptionProbe::Failed {
                    message: error.to_string(),
                },
                cx,
            );
            return;
        }
    };

    set_transcription_probe(controller, TranscriptionProbe::Running, cx);
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        // The reply is the test result, so it has to come back as a string:
        // that also keeps the awaited payload trivially `Send`.
        let result: Result<String, String> = cx
            .background_executor()
            .spawn(async move {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                provider
                    .transcribe(
                        &trove_core::media::audio_prep::probe_wav(),
                        "probe.wav",
                        "audio/wav",
                        config.language.as_deref(),
                        config.prompt.as_deref(),
                        &cancel,
                    )
                    .map(|text| text.trim().to_string())
                    .map_err(|error| error.message)
            })
            .await;
        let probe = match result {
            Ok(reply) => TranscriptionProbe::Ok { reply },
            Err(message) => TranscriptionProbe::Failed { message },
        };
        controller.update(cx, |ctl, cx| {
            ctl.transcription_probe = probe;
            cx.notify();
        });
    })
    .detach();
}

/// Start a transcription run from the workspace context menu or the settings
/// page.
///
/// Returns `false` when the endpoint is not configured, nothing selected can
/// carry a transcript (for [`TranscribeTarget::Selection`]), or another run
/// already holds the slot — in each case a toast says which.
pub fn start_transcription_app(
    controller: &Entity<LibraryController>,
    target: TranscribeTarget,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let only = match target {
        TranscribeTarget::Selection => {
            let selection = controller.read(cx).selected_assets.as_ref().clone();
            let transcribable: Vec<uuid::Uuid> = controller
                .read(cx)
                .library
                .assets_by_ids(&selection)
                .unwrap_or_default()
                .into_iter()
                .filter(|asset| matches!(asset.kind, AssetKind::Audio | AssetKind::Video))
                .map(|asset| asset.id)
                .collect();
            if transcribable.is_empty() {
                window.push_notification(
                    Notification::warning(rust_i18n::t!("transcribe.empty_selection").to_string()),
                    cx,
                );
                return false;
            }
            transcribable
        }
        TranscribeTarget::WholeLibrary => Vec::new(),
    };
    start_transcription_request_app(
        controller,
        TranscribeRunRequest {
            only,
            ..TranscribeRunRequest::default()
        },
        window,
        cx,
    )
}

/// Start a transcription run from an already-resolved request, record what a
/// retry needs, add its panel row, and detach the watcher. The one place a
/// run actually launches, shared by the menu action and the panel's Retry
/// button.
pub fn start_transcription_request_app(
    controller: &Entity<LibraryController>,
    request: TranscribeRunRequest,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // The local engine needs its weights on disk before anything runs. The
    // ask happens here — the one launch point, shared by the menu, the
    // settings page and the retry button — and when the download lands, the
    // same request re-launches itself through this very call.
    let engine = trove_core::config::AppConfig::load()
        .ai_transcription
        .unwrap_or_default()
        .engine;
    if engine == trove_core::config::TranscriptionEngine::Local
        && !super::local_model::ensure_local_model_app(
            controller,
            Some(request.clone()),
            window,
            cx,
        )
    {
        return false;
    }
    let Some(provider) = transcription_provider(window, cx) else {
        return false;
    };
    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| {
        ctl.library.start_transcription(provider, request.clone())
    });
    let Ok((task_id, rx)) = started else {
        return false; // one run at a time; the running toast is already up
    };
    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Transcription {
            request: request.clone(),
        });
        ctl.begin_task(
            task_id,
            TaskKind::Transcription,
            TaskKind::Transcription.name().to_string(),
        );
    });

    let started_text = if request.only.is_empty() {
        rust_i18n::t!("transcribe.started_all").to_string()
    } else {
        rust_i18n::t!("transcribe.started", count = request.only.len()).to_string()
    };
    window.push_notification(
        TranscribeNotice::keyed(Notification::info(started_text)),
        cx,
    );
    watch_job::<TranscribeNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        |outcome: &TranscribeOutcome| Some(transcribe_outcome_toast(outcome)),
        |_, _| {},
        cx,
    );
    true
}

/// Build the transcription provider from the saved config, toasting and
/// returning `None` when the endpoint is missing or refuses to construct.
/// Shared by the start and retry paths so they agree on exactly how a
/// provider is made.
fn transcription_provider(
    window: &mut Window,
    cx: &mut App,
) -> Option<std::sync::Arc<dyn trove_core::ai::transcribe::TranscribeProvider>> {
    let transcription = trove_core::config::AppConfig::load()
        .resolved_transcription()
        .unwrap_or_default();
    if !transcription.is_configured() {
        window.push_notification(
            Notification::warning(
                rust_i18n::t!("settings.transcription_not_configured").to_string(),
            ),
            cx,
        );
        return None;
    }
    match trove_core::ai::transcribe::build_from_config(&transcription) {
        Ok(provider) => Some(provider),
        Err(error) => {
            window.push_notification(Notification::warning(error.to_string()), cx);
            None
        }
    }
}

/// The completion toast: success when everything landed, a warning listing
/// the parts that did not. `no_audio` is named because a silent-video run
/// otherwise reads exactly like a run that did nothing.
fn transcribe_outcome_toast(outcome: &TranscribeOutcome) -> Notification {
    let text = rust_i18n::t!(
        "transcribe.done",
        transcribed = outcome.transcribed,
        skipped = outcome.skipped,
        no_audio = outcome.no_audio,
        failed = outcome.failed
    )
    .to_string();
    if outcome.failed > 0 {
        Notification::warning(text)
    } else {
        Notification::success(text)
    }
}

/// Record a transcription-probe result and repaint, so the page shows it
/// whether the test finished on the UI thread (bad configuration) or a
/// worker (a call).
fn set_transcription_probe(
    controller: &Entity<LibraryController>,
    probe: TranscriptionProbe,
    cx: &mut App,
) {
    controller.update(cx, |ctl, cx| {
        ctl.transcription_probe = probe;
        cx.notify();
    });
    cx.refresh_windows();
}

/// Ask the running transcription job to stop at its next checkpoint (the
/// settings page's cancel button; the task panel's per-row cancel reaches the
/// same manager directly).
pub fn cancel_transcription_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let manager = controller.read(cx).library.tasks().clone();
    if let Some(task) = manager
        .snapshot()
        .into_iter()
        .find(|task| task.kind == TaskKind::Transcription && task.status == TaskStatus::Running)
    {
        manager.cancel(task.id);
    }
}
