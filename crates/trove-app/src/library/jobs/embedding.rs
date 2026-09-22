//! The embedding side of Settings ▸ AI: the whole-library vector backfill
//! (a `TaskManager` job with a keyed progress toast), the connection probe and
//! the vector-clearing action. The backfill runs on the backend task thread;
//! this submodule only starts it and watches its events.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::tasks::embed::EmbedOutcome;
use trove_core::tasks::{TaskKind, TaskStatus};

use super::{NoticeKey, watch_job};
use crate::library::{AiProbe, LibraryController, Retryable};

/// Marker for the keyed embedding-progress toast.
pub struct EmbeddingNotice;

impl NoticeKey for EmbeddingNotice {
    const ID: &'static str = "embedding-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(
            rust_i18n::t!("settings.ai_running_progress", done = done, total = total).to_string(),
        )
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("settings.ai_failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("settings.ai_cancelled").to_string())
    }
}

/// Start an embedding backfill from the settings page: build the provider
/// from the saved config (an OpenAI-compatible endpoint), start the
/// library's `EmbeddingBackfill` job, and watch it — a keyed toast carries
/// progress, and the outcome replaces it. The job runs on the backend task
/// thread; this only starts it and watches. Returns `false` when the config
/// is missing or incomplete (a toast says so) or a run is already going.
pub fn start_embedding_backfill_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let config = trove_core::config::AppConfig::load().ai_embedding;
    let Some(config) = config.filter(trove_core::config::EmbeddingConfig::is_configured) else {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return false;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&config) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                window.push_notification(Notification::warning(error.to_string()), cx);
                return false;
            }
        };

    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| ctl.library.start_embedding_backfill(provider));
    let Ok((task_id, rx)) = started else {
        return false; // one backfill at a time; the running toast is up
    };
    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Embedding);
        ctl.begin_task(
            task_id,
            TaskKind::EmbeddingBackfill,
            TaskKind::EmbeddingBackfill.name().to_string(),
        );
    });
    window.push_notification(
        EmbeddingNotice::keyed(Notification::info(
            rust_i18n::t!("settings.ai_running").to_string(),
        )),
        cx,
    );
    watch_job::<EmbeddingNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        |outcome: &EmbedOutcome| Some(embedding_outcome_toast(outcome)),
        |_, _| {},
        cx,
    );
    true
}

/// Ask the running backfill to stop at its next cancellation checkpoint (a
/// batch boundary); the outcome toast replaces the progress toast.
pub fn cancel_embedding_backfill_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let manager = controller.read(cx).library.tasks().clone();
    if let Some(task) = manager
        .snapshot()
        .into_iter()
        .find(|t| t.kind == TaskKind::EmbeddingBackfill && t.status == TaskStatus::Running)
    {
        manager.cancel(task.id);
    }
}

/// The line a connection test embeds. The content is irrelevant — only that
/// the endpoint answers with a vector at all, and how wide it is.
const PROBE_TEXT: &str = "trove connection test";

/// Prove the configured endpoint works (Settings ▸ AI): build the provider
/// exactly as the backfill does, embed one throwaway line on the background
/// executor, and record the width the server answered with — or the reason it
/// refused — on [`LibraryController::ai_probe`].
///
/// Deliberately not a task-manager job: it writes no rows, must not occupy
/// the embedding slot a real backfill needs, and its answer is one line of
/// text on the page rather than a progress bar.
pub fn test_embedding_endpoint_app(controller: &Entity<LibraryController>, cx: &mut App) {
    if controller.read(cx).ai_probe.is_running() {
        return;
    }
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_embedding
        .filter(trove_core::config::EmbeddingConfig::is_configured)
    else {
        set_ai_probe(
            controller,
            ai_probe_failure(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&config) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                set_ai_probe(controller, ai_probe_failure(error.to_string()), cx);
                return;
            }
        };

    set_ai_probe(controller, AiProbe::Running, cx);
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        // Flattened to a string on the worker: the page only needs the
        // message, and that keeps the awaited payload trivially `Send`.
        let result: Result<usize, String> = cx
            .background_executor()
            .spawn(async move {
                provider
                    .embed_texts(&[PROBE_TEXT.to_string()])
                    .map(|vectors| vectors.first().map_or(0, Vec::len))
                    .map_err(|error| error.to_string())
            })
            .await;
        let probe = match result {
            Ok(dim) if dim > 0 => AiProbe::Ok { dim },
            // A success with no vector means the server is answering
            // nonsense; report it as a failure rather than a green line.
            Ok(_) => ai_probe_failure(rust_i18n::t!("settings.ai_probe_empty").to_string()),
            Err(message) => ai_probe_failure(message),
        };
        // `notify` is enough to repaint the page (the settings view observes
        // the controller); `refresh_windows` is an `App` method and this runs
        // on a background executor, where only `AsyncApp` is in hand.
        controller.update(cx, |ctl, cx| {
            ctl.ai_probe = probe;
            cx.notify();
        });
    })
    .detach();
}

/// Delete every vector stored under the configured model (Settings ▸ AI).
///
/// A vector is a derivative of title / description / tags, so this costs a
/// re-embed and never user data — the same class of operation as clearing the
/// thumbnail cache, which is why it runs on the click and reports the row
/// count through a toast instead of asking first.
pub fn delete_embeddings_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_embedding
        .filter(trove_core::config::EmbeddingConfig::is_configured)
    else {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return;
    };
    let note = match controller.update(cx, |ctl, _| ctl.library.delete_embeddings(&config.model)) {
        Ok(count) => {
            Notification::success(rust_i18n::t!("settings.ai_deleted", count = count).to_string())
        }
        Err(error) => {
            Notification::warning(rust_i18n::t!("settings.job_failed", error = error).to_string())
        }
    };
    window.push_notification(note, cx);
    // The coverage line and the delete button's enabled state both read the
    // table, and both are rendered per paint.
    cx.refresh_windows();
}

/// A [`AiProbe::Failed`] carrying a localized reason.
fn ai_probe_failure(reason: impl Into<String>) -> AiProbe {
    AiProbe::Failed {
        message: reason.into(),
    }
}

/// Record a probe result and repaint, so the page shows it whether the test
/// finished on the UI thread (bad configuration) or a worker (a real call).
fn set_ai_probe(controller: &Entity<LibraryController>, probe: AiProbe, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.ai_probe = probe;
        cx.notify();
    });
    cx.refresh_windows();
}

/// The completion toast for a backfill that returned a value: success when
/// nothing failed, a warning naming the failures otherwise. The `error`
/// case (a fatal stop) is handled from the terminal event, not here.
fn embedding_outcome_toast(outcome: &EmbedOutcome) -> Notification {
    let text = rust_i18n::t!(
        "settings.ai_done",
        embedded = outcome.embedded,
        skipped = outcome.skipped,
        failed = outcome.failed
    )
    .to_string();
    if outcome.failed > 0 {
        Notification::warning(text)
    } else {
        Notification::success(text)
    }
}
