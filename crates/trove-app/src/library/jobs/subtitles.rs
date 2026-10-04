//! Save a transcript as an SRT subtitle file next to its source.
//!
//! The recognisers store plain text, so the cue timeline is an estimate —
//! see `trove_core::media::subtitles` for how the cues split and time. The
//! destination is the sidecar players pick up on their own
//! (`show.mp4` → `show.srt`), written atomically; an existing file asks
//! before being replaced, because a sidecar the user hand-tuned is user
//! work the transcript cannot reproduce.
//!
//! The sidecar is also registered as a *linked* library asset, so it shows in
//! the grid like any imported file and can be opened directly. Editing it
//! later refreshes that record's hash instead of adding a second one.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, Entity, Window};

use crate::library::LibraryController;

/// Register a just-written sidecar as a linked library asset so it appears in
/// the grid (and can be opened on its own). A record already linking this path
/// is refreshed rather than duplicated; a failure is logged, never surfaced as
/// a second toast on top of the save that already succeeded.
pub fn ensure_subtitle_asset(
    controller: &Entity<LibraryController>,
    path: &std::path::Path,
    cx: &mut App,
) {
    controller.update(cx, |ctl, cx| {
        match ctl.library.ensure_linked_file(path) {
            Ok(_) => {
                ctl.generation += 1;
                cx.notify();
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "subtitle asset registration failed")
            }
        }
    });
}

/// Auto-export subtitles for a finished transcription run: every asset the
/// run transcribed gets its `.srt` sidecar written as the run settles — this
/// is the save path, there is no separate save action to reach for.
///
/// A sidecar that does not exist yet is written straight away. A sidecar that
/// does exist may be the user's own hand-tuned work, so those are collected
/// and asked about in one dialog before being replaced. This runs from the
/// app view's controller observer, which has the window the ask needs; the
/// transcription watcher only left the ids on the controller.
pub fn auto_save_subtitles_app(
    controller: &Entity<LibraryController>,
    request: crate::library::PendingSubtitleExport,
    window: &mut Window,
    cx: &mut App,
) {
    // (srt path, transcript, duration) — resolved under the controller borrow,
    // which ends before any dialog or notification touches `cx`. A missing
    // sidecar is written for both groups; an *existing* one only asks to be
    // replaced for a freshly transcribed asset. A skipped asset's existing
    // sidecar is left exactly as it is (the user may have edited it, and the
    // recogniser was not asked again).
    let mut fresh: Vec<(std::path::PathBuf, String, Option<u64>)> = Vec::new();
    let mut conflicts: Vec<(std::path::PathBuf, String, Option<u64>)> = Vec::new();
    {
        let ctl = controller.read(cx);
        let transcribed = request.transcribed.iter().map(|id| (*id, true));
        let backfill = request.backfill.iter().map(|id| (*id, false));
        for (id, ask_on_conflict) in transcribed.chain(backfill) {
            let Some(asset) = ctl.library.asset(id).ok().flatten() else {
                continue;
            };
            let Some(transcript) = ctl
                .library
                .transcript(id)
                .ok()
                .flatten()
                .filter(|text| !text.trim().is_empty())
            else {
                continue;
            };
            let Some(disk_path) = ctl.library.asset_file(id) else {
                continue;
            };
            let entry = (disk_path.with_extension("srt"), transcript, asset.duration_ms);
            if entry.0.exists() {
                if ask_on_conflict {
                    conflicts.push(entry);
                }
            } else {
                fresh.push(entry);
            }
        }
    }

    let mut written: Vec<std::path::PathBuf> = Vec::new();
    for (path, transcript, duration) in &fresh {
        match trove_core::media::subtitles::save(path, transcript, *duration) {
            Ok(_) => written.push(path.clone()),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "subtitle auto-save failed");
            }
        }
    }
    for path in &written {
        ensure_subtitle_asset(controller, path, cx);
    }
    if !written.is_empty() {
        window.push_notification(
            Notification::success(
                rust_i18n::t!("subtitle.auto_saved", count = written.len()).to_string(),
            ),
            cx,
        );
    }

    if conflicts.is_empty() {
        return;
    }
    let count = conflicts.len();
    let first = conflicts[0].0.display().to_string();
    let controller = controller.clone();
    window.open_alert_dialog(
        cx,
        move |alert, _, _| {
            let conflicts = conflicts.clone();
            let first = first.clone();
            let controller = controller.clone();
            alert
                .title(rust_i18n::t!("inspector.transcript_overwrite_title").to_string())
                .description(
                    rust_i18n::t!(
                        "subtitle.auto_overwrite_body",
                        count = count,
                        path = first
                    )
                    .to_string(),
                )
                .confirm()
                .ok_text(rust_i18n::t!("inspector.transcript_save").to_string())
                .on_ok(move |_, window, cx| {
                    let mut overwritten = 0usize;
                    for (path, transcript, duration) in &conflicts {
                        if trove_core::media::subtitles::save(path, transcript, *duration).is_ok() {
                            ensure_subtitle_asset(&controller, path, cx);
                            overwritten += 1;
                        }
                    }
                    window.push_notification(
                        Notification::success(
                            rust_i18n::t!(
                                "subtitle.auto_overwritten",
                                count = overwritten
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                    true
                })
        },
    );
}
