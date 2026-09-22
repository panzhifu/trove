//! The AI analysis job: description / tags / rating from a vision model, one
//! API call per asset (the longest-running job the library has), plus its
//! undo. This submodule also holds the status-bar task panel's job-control
//! entry points — [`pause_task_app`], [`resume_task_app`] and [`retry_task_app`]
//! — because retry needs to know how to rebuild an analysis provider/run, which
//! lives here.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::tasks::TaskStatus;
use trove_core::tasks::ai_analysis::{AiAnalysisOutcome, AiAnalysisRunRequest, UndoOutcome};
use trove_core::tasks::{TaskId, TaskKind};

use super::embedding::start_embedding_backfill_app;
use super::import::start_import_job;
use super::{NoticeKey, watch_job};
use crate::library::{AnalysisProbe, LibraryController, Retryable};

/// Marker for the keyed analysis toast.
pub struct AnalysisNotice;

impl NoticeKey for AnalysisNotice {
    const ID: &'static str = "autotag-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(rust_i18n::t!("autotag.running", done = done, total = total).to_string())
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("autotag.failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("autotag.cancelled").to_string())
    }
}

/// What an analysis run is asked to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisTarget {
    /// The assets selected right now. What the grid's menu and toolbar ask
    /// for: an action on a selection should stay on it.
    Selection,
    /// Every live asset. Only the settings page asks for this, and it says
    /// so on the button.
    WholeLibrary,
}

/// Prove the analysis endpoint works (Settings ▸ AI): send one throwaway
/// prompt and show what came back.
///
/// Deliberately not a task-manager job, for the same reasons the embedding
/// probe is not: it writes no rows, must not occupy the analysis slot a real
/// run needs, and its answer is one line of text on the page.
pub fn test_analysis_endpoint_app(controller: &Entity<LibraryController>, cx: &mut App) {
    if controller.read(cx).analysis_probe.is_running() {
        return;
    }
    let Some(config) = trove_core::config::AppConfig::load()
        .ai_analysis
        .filter(trove_core::config::AiAnalysisConfig::is_configured)
    else {
        set_analysis_probe(
            controller,
            analysis_probe_failure(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return;
    };
    let provider = match trove_core::ai::vendor::build_from_config(&config) {
        Ok(provider) => provider,
        Err(error) => {
            set_analysis_probe(controller, analysis_probe_failure(error.to_string()), cx);
            return;
        }
    };

    set_analysis_probe(controller, AnalysisProbe::Running, cx);
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        // The reply is the test result, so it has to come back as a string:
        // that also keeps the awaited payload trivially `Send`.
        let result: Result<String, String> = cx
            .background_executor()
            .spawn(async move {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                provider
                    .analyze(&probe_request(), &cancel)
                    .map(|reply| reply.trim().to_string())
                    .map_err(|error| error.message)
            })
            .await;
        let probe = match result {
            Ok(reply) if !reply.is_empty() => AnalysisProbe::Ok {
                reply: shorten(&reply),
            },
            // A success with nothing in it means the server answered a shape
            // we cannot use; a green line would be a lie.
            Ok(_) => analysis_probe_failure(rust_i18n::t!("settings.ai_probe_empty").to_string()),
            Err(message) => analysis_probe_failure(message),
        };
        controller.update(cx, |ctl, cx| {
            ctl.analysis_probe = probe;
            cx.notify();
        });
    })
    .detach();
}

/// The throwaway request the probe sends: no image, one field, no vocabulary.
fn probe_request() -> trove_core::ai::analysis::AiAnalysisRequest {
    use trove_core::ai::analysis::{
        AiAnalysisFields, AiAnalysisRequest, AiAnalysisSettings, MediaType,
    };
    AiAnalysisRequest {
        asset_id: uuid::Uuid::nil(),
        display_name: "probe".into(),
        file_name: "probe".into(),
        mime: "text/plain".into(),
        media_type: MediaType::Other,
        thumbnail_jpeg: None,
        contact_sheet_jpeg: None,
        language: "en".into(),
        enabled_fields: AiAnalysisFields {
            description: true,
            tags: false,
            rating: false,
        },
        metadata_lines: vec![],
        existing_tag_names: vec![],
        vocabulary: vec![],
        settings: AiAnalysisSettings::default(),
    }
}

/// Build the analysis provider from the saved config, toasting and returning
/// `None` when the endpoint is missing or refuses to construct. Shared by the
/// start and retry paths so they agree on exactly how a provider is made.
fn analysis_provider(
    window: &mut Window,
    cx: &mut App,
) -> Option<std::sync::Arc<dyn trove_core::ai::vendor::VendorAdapter>> {
    let analysis = trove_core::config::AppConfig::load()
        .ai_analysis
        .unwrap_or_default();
    if !analysis.is_configured() {
        window.push_notification(
            Notification::warning(rust_i18n::t!("settings.ai_not_configured").to_string()),
            cx,
        );
        return None;
    }
    match trove_core::ai::vendor::build_from_config(&analysis) {
        Ok(provider) => Some(std::sync::Arc::from(provider)),
        Err(error) => {
            window.push_notification(Notification::warning(error.to_string()), cx);
            None
        }
    }
}

/// Start an analysis run from an already-resolved request + provider, record
/// what a retry needs, add its panel row, and detach the watcher. The one
/// place an analysis run actually launches, shared by the grid/settings action
/// and the panel's Retry button.
fn launch_analysis(
    controller: &Entity<LibraryController>,
    request: AiAnalysisRunRequest,
    provider: std::sync::Arc<dyn trove_core::ai::vendor::VendorAdapter>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| {
        ctl.library.start_ai_analysis(provider, request.clone())
    });
    let Ok((task_id, rx)) = started else {
        return false; // one run at a time; the running toast is already up
    };
    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Analysis {
            request: request.clone(),
            undo: false,
        });
        ctl.begin_task(
            task_id,
            TaskKind::AiAnalysis,
            TaskKind::AiAnalysis.name().to_string(),
        );
    });

    let started_text = if request.only.is_empty() {
        rust_i18n::t!("autotag.started_all").to_string()
    } else {
        rust_i18n::t!("autotag.started", count = request.only.len()).to_string()
    };
    window.push_notification(AnalysisNotice::keyed(Notification::info(started_text)), cx);
    watch_job::<AnalysisNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        |outcome: &AiAnalysisOutcome| Some(analysis_outcome_toast(outcome)),
        |_, _| {},
        cx,
    );
    true
}

/// Start an analysis run from the settings page or the grid.
///
/// Returns `false` when the endpoint is not configured, nothing is selected
/// (for [`AnalysisTarget::Selection`]) or another run already holds the slot —
/// in each case a toast says which.
pub fn start_analysis_app(
    controller: &Entity<LibraryController>,
    target: AnalysisTarget,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(provider) = analysis_provider(window, cx) else {
        return false;
    };
    let only = match target {
        AnalysisTarget::Selection => {
            let selection = controller.read(cx).selected_assets.as_ref().clone();
            if selection.is_empty() {
                window.push_notification(
                    Notification::warning(rust_i18n::t!("autotag.empty_selection").to_string()),
                    cx,
                );
                return false;
            }
            selection
        }
        AnalysisTarget::WholeLibrary => Vec::new(),
    };
    let request = AiAnalysisRunRequest {
        only,
        ..AiAnalysisRunRequest::default()
    };
    launch_analysis(controller, request, provider, window, cx)
}

/// Detach every tag the analysis ever added. Needs no endpoint.
pub fn start_analysis_undo_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let manager = controller.read(cx).library.tasks().clone();
    let started = controller.update(cx, |ctl, _| {
        ctl.library
            .start_ai_analysis_undo(AiAnalysisRunRequest::default())
    });
    let Ok((task_id, rx)) = started else {
        return false; // a run or an undo is already going
    };
    controller.update(cx, |ctl, _| {
        ctl.record_retry(Retryable::Analysis {
            request: AiAnalysisRunRequest::default(),
            undo: true,
        });
        ctl.begin_task(
            task_id,
            TaskKind::AiAnalysis,
            TaskKind::AiAnalysis.name().to_string(),
        );
    });
    window.push_notification(
        AnalysisNotice::keyed(Notification::info(
            rust_i18n::t!("autotag.undo_running").to_string(),
        )),
        cx,
    );
    watch_job::<AnalysisNotice, _>(
        controller.clone(),
        manager,
        task_id,
        rx,
        window.window_handle(),
        |outcome: &UndoOutcome| Some(undo_outcome_toast(outcome)),
        |_, _| {},
        cx,
    );
    true
}

/// Ask the running analysis job to stop at its next cancellation checkpoint (a
/// batch boundary); the outcome toast replaces the progress toast.
pub fn cancel_analysis_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let manager = controller.read(cx).library.tasks().clone();
    if let Some(task) = manager
        .snapshot()
        .into_iter()
        .find(|task| task.kind == TaskKind::AiAnalysis && task.status == TaskStatus::Running)
    {
        manager.cancel(task.id);
    }
}

/// Hold a running job at its next checkpoint (the panel's Pause button).
pub fn pause_task_app(controller: &Entity<LibraryController>, task_id: TaskId, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.pause_task(task_id);
        cx.notify();
    });
}

/// Release a paused job (the panel's Resume button).
pub fn resume_task_app(controller: &Entity<LibraryController>, task_id: TaskId, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.resume_task(task_id);
        cx.notify();
    });
}

/// Re-run a job the panel lists as failed or cancelled, from the inputs saved
/// when it first started (the panel's Retry button). Returns `false` when there
/// is nothing recorded for `kind` or the relaunch was refused.
pub fn retry_task_app(
    controller: &Entity<LibraryController>,
    kind: TaskKind,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(retry) = controller.read(cx).retry_inputs(kind).cloned() else {
        return false;
    };
    match retry {
        Retryable::Import {
            kind,
            options,
            total,
        } => {
            let manager = controller.read(cx).library.tasks().clone();
            start_import_job(controller, &manager, kind, options, total, window, cx)
        }
        Retryable::Embedding => start_embedding_backfill_app(controller, window, cx),
        Retryable::Analysis { request, undo } => {
            if undo {
                start_analysis_undo_app(controller, window, cx)
            } else if let Some(provider) = analysis_provider(window, cx) {
                launch_analysis(controller, request, provider, window, cx)
            } else {
                false
            }
        }
    }
}

fn analysis_outcome_toast(outcome: &AiAnalysisOutcome) -> Notification {
    let mut text = rust_i18n::t!(
        "autotag.done",
        tagged = outcome.analysed,
        unchanged = outcome.unchanged,
        skipped = outcome.skipped,
        failed = outcome.failed
    )
    .to_string();
    if outcome.images_rejected {
        // Worth saying rather than hiding: the endpoint could not take the
        // thumbnails, so the result came from text alone.
        text.push(' ');
        text.push_str(rust_i18n::t!("autotag.text_only").as_ref());
    }
    if outcome.failed > 0 {
        Notification::warning(text)
    } else {
        Notification::success(text)
    }
}

fn undo_outcome_toast(outcome: &UndoOutcome) -> Notification {
    Notification::info(
        rust_i18n::t!(
            "autotag.undo_done",
            detached = outcome.detached,
            assets = outcome.assets
        )
        .to_string(),
    )
}

/// An [`AnalysisProbe::Failed`] carrying a localized reason.
fn analysis_probe_failure(reason: impl Into<String>) -> AnalysisProbe {
    AnalysisProbe::Failed {
        message: reason.into(),
    }
}

/// Record an analysis-probe result and repaint, so the page shows it whether
/// the test finished on the UI thread (bad configuration) or a worker (a
/// call).
fn set_analysis_probe(controller: &Entity<LibraryController>, probe: AnalysisProbe, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.analysis_probe = probe;
        cx.notify();
    });
    cx.refresh_windows();
}

/// Squeeze a model's reply onto one settings row.
fn shorten(reply: &str) -> String {
    let collapsed = reply.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 80 {
        collapsed
    } else {
        let mut short: String = collapsed.chars().take(80).collect();
        short.push('…');
        short
    }
}
