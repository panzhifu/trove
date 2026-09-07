//! Import jobs: heavy file work on the background executor, database commits
//! on the main thread, with progress reported through the notification layer.
//!
//! The pipeline itself lives in `trove-core::media::import` — this module is
//! only the executor orchestration: stage in the background, commit where the
//! [`LibraryController`] lives, then surface the outcome as a toast.

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::media::import;

use crate::state::LibraryController;

/// Marker type for the import progress toast: pushing with the same id
/// replaces the previous toast instead of stacking a new one.
pub struct ImportNotice;

/// Start an import from a set of file paths.
///
/// Works from any entry point that holds an `App` (button, file drop, ...):
/// the heavy staging is scheduled on the background executor and the database
/// commits run on the foreground executor where entity state lives.
pub fn import_paths_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    if paths.is_empty() || controller.read(cx).is_importing() {
        return;
    }

    // Fail fast when the target collection does not exist.
    let into_collection = controller.read(cx).current_collection;
    if let Some(cid) = into_collection {
        let conn = controller.read(cx).library.store().conn();
        if trove_core::store::collections::get(conn, cid)
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
    }

    let total = paths.len();
    let library_root = controller.read(cx).library.root().to_path_buf();
    controller.update(cx, |ctl, _| ctl.begin_import(total));

    // One keyed toast that tracks the batch: replaced by the completion
    // notification when the import finishes.
    window.push_notification(
        Notification::info(rust_i18n::t!("notice.import_started", count = total).to_string())
            .id1::<ImportNotice>("import-progress"),
        cx,
    );

    let controller = controller.clone();
    let handle = window.window_handle();
    let task = cx
        .background_executor()
        .spawn(async move { import::stage_all(&library_root, &paths) });

    cx.spawn(async move |cx| {
        let staged = task.await;
        let mut report = import::ImportReport::default();

        // Commit one file per main-thread turn, yielding in between so the
        // UI (status bar + progress toast) repaints with live per-file
        // progress instead of jumping from 0 to done.
        for (done, item) in staged.into_iter().enumerate() {
            controller.update(cx, |ctl, cx| {
                match item {
                    Ok(file) => {
                        match import::commit_staged(
                            ctl.library.store(),
                            into_collection,
                            Some(import::AutoCollection::SourceFolder),
                            &file,
                        ) {
                            Ok(imported) => report.imported.push(imported),
                            Err(e) => report.skipped.push(import::ImportSkip {
                                path: file.path,
                                reason: e.to_string(),
                            }),
                        }
                    }
                    Err(skip) => report.skipped.push(skip),
                }
                ctl.import_progress(done + 1);
                cx.notify();
            });

            // Keep the keyed progress toast current.
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("notice.import_running", done = done + 1, total = total)
                            .to_string(),
                    )
                    .id1::<ImportNotice>("import-progress"),
                    cx,
                );
            });

            // Yield so the frame with the updated progress actually draws.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;
        }

        controller.update(cx, |ctl, cx| {
            ctl.finish_import(report.imported_count(), report.skipped_count());
            cx.notify();
        });

        let note = if report.skipped.is_empty() {
            Notification::success(
                rust_i18n::t!("notice.import_done", imported = report.imported_count()).to_string(),
            )
        } else {
            Notification::warning(
                rust_i18n::t!(
                    "notice.import_done_skipped",
                    imported = report.imported_count(),
                    skipped = report.skipped_count()
                )
                .to_string(),
            )
        };
        let _ = handle.update(cx, |_view, window, cx| {
            window.push_notification(note, cx);
        });
    })
    .detach();
}
