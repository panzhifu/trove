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

use crate::library::LibraryController;

/// Embed freshly imported images, one per main-thread turn with a short
/// yield in between, so the UI stays responsive. `Store` is thread-confined
/// (`Rc<RefCell<Connection>>`), so this must run on the foreground executor;
/// only the model inference blocks, and only for one image at a time.
/// Per-asset logic lives in `clip::embed_asset` (core).
async fn embed_imported_images(
    controller: Entity<LibraryController>,
    store: trove_core::store::Store,
    root: PathBuf,
    imported_ids: Vec<uuid::Uuid>,
    cx: &mut AsyncApp,
) {
    use trove_core::media::clip;
    if !clip::semantic_ready() || imported_ids.is_empty() {
        return;
    }
    let total = imported_ids.len();
    let mut done = 0usize;
    for id in imported_ids {
        // Persistent per-file failures are counted, not spammed; the user can
        // see them via Settings ▸ Search ▸ embed-all (which reports skipped).
        if matches!(clip::embed_asset(&store, &root, id), Ok(true)) {
            done += 1;
        }
        cx.background_executor()
            .timer(std::time::Duration::from_millis(50))
            .await;
    }
    controller.update(cx, |ctl, cx| {
        ctl.notice =
            Some(rust_i18n::t!("notice.embedded_done", done = done, total = total).to_string());
        cx.notify();
    });
}

/// Marker type for the import progress toast: pushing with the same id
/// replaces the previous toast instead of stacking a new one.
pub struct ImportNotice;

/// Start an import from a set of file paths into the currently browsed
/// collection. See [`import_paths_app_into`].
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
    let into_collection = controller.read(cx).current_collection;
    import_paths_app_into(controller, paths, into_collection, window, cx);
}

/// Start an import into an explicit collection (`None` = unfiled). Returns
/// `false` when the batch was refused (already importing, empty list or a
/// dead target collection) so callers can retry later — the folder watcher
/// relies on this to keep new files pending.
pub fn import_paths_app_into(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    into_collection: Option<uuid::Uuid>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if paths.is_empty() || controller.read(cx).is_importing() {
        return false;
    }

    // Fail fast when the target collection does not exist.
    if let Some(cid) = into_collection {
        let conn = controller.read(cx).library.store().conn();
        if trove_core::store::collections::get(conn, cid)
            .ok()
            .flatten()
            .is_none()
        {
            return false;
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
    let task = cx.background_executor().spawn({
        let library_root = library_root.clone();
        async move { import::stage_all(&library_root, &paths) }
    });

    cx.spawn(async move |cx| {
        let staged = task.await;
        let mut report = import::ImportReport::default();
        let mut imported_ids: Vec<uuid::Uuid> = Vec::new();

        // Commit one file per main-thread turn, yielding in between so the
        // UI (status bar + progress toast) repaints with live per-file
        // progress instead of jumping from 0 to done.
        for (done, item) in staged.into_iter().enumerate() {
            controller.update(cx, |ctl, cx| {
                match item {
                    Ok(file) => {
                        match import::commit_staged(ctl.library.store(), into_collection, &file) {
                            Ok(imported) => {
                                imported_ids.push(imported.asset_id);
                                report.imported.push(imported);
                            }
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

        // Embed newly imported images in the background (one per frame).
        if !imported_ids.is_empty() {
            let store = controller.update(cx, |ctl, _| ctl.library.store().clone());
            embed_imported_images(controller.clone(), store, library_root, imported_ids, cx).await;
        }

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

    true
}

/// Drain the collect-service inbox: import every waiting file (unfiled),
/// write its sidecar `source_url` into the asset, then delete the file and
/// sidecar. Returns `false` when the inbox was empty or an import is
/// already running (retry on the next watcher cycle).
pub fn collect_inbox_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let inbox = trove_core::collect::inbox_dir();
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return false;
    };
    // Files with an optional sidecar; sidecars themselves are not imports.
    let mut items: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".meta.json"))
                .unwrap_or(true)
        {
            continue;
        }
        let sidecar = {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            inbox.join(format!("{name}.meta.json"))
        };
        items.push((path, sidecar.is_file().then_some(sidecar)));
    }
    if items.is_empty() {
        return false;
    }
    if controller.read(cx).is_importing() {
        return false;
    }

    let total = items.len();
    let library_root = controller.read(cx).library.root().to_path_buf();
    controller.update(cx, |ctl, _| ctl.begin_import(total));
    window.push_notification(
        Notification::info(rust_i18n::t!("notice.collect_started", count = total).to_string())
            .id1::<ImportNotice>("collect-progress"),
        cx,
    );

    let controller = controller.clone();
    let handle = window.window_handle();
    let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
    let cleanup = cleanup_names(&paths);
    let stage_root = library_root.clone();
    let stage_paths = paths.clone();
    let task = cx
        .background_executor()
        .spawn(async move { import::stage_all(&stage_root, &stage_paths) });

    cx.spawn(async move |cx| {
        let staged = task.await;
        let mut report = import::ImportReport::default();
        let mut imported_ids: Vec<uuid::Uuid> = Vec::new();

        for (done, (item, (_path, sidecar))) in staged.into_iter().zip(items).enumerate() {
            controller.update(cx, |ctl, cx| {
                match item {
                    Ok(file) => match import::commit_staged(ctl.library.store(), None, &file) {
                        Ok(imported) => {
                            imported_ids.push(imported.asset_id);
                            report.imported.push(imported);
                        }
                        Err(e) => report.skipped.push(import::ImportSkip {
                            path: file.path,
                            reason: e.to_string(),
                        }),
                    },
                    Err(skip) => report.skipped.push(skip),
                }
                ctl.import_progress(done + 1);
                cx.notify();
            });
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("notice.import_running", done = done + 1, total = total)
                            .to_string(),
                    )
                    .id1::<ImportNotice>("collect-progress"),
                    cx,
                );
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;

            // Record the source URL and clean up the inbox entry.
            if let Some(sidecar) = sidecar
                && let Ok(meta) = std::fs::read_to_string(&sidecar)
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta)
                && let Some(url) = meta.get("source_url").and_then(|v| v.as_str())
            {
                controller.update(cx, |ctl, cx| {
                    if let Some(asset_id) = imported_ids.last().copied() {
                        let patch = trove_core::model::AssetPatch {
                            source_url: Some(Some(url.to_string())),
                            ..Default::default()
                        };
                        let _ = ctl.library.patch_asset(asset_id, &patch);
                        cx.notify();
                    }
                });
            }
        }

        controller.update(cx, |ctl, cx| {
            ctl.finish_import(report.imported_count(), report.skipped_count());
            cx.notify();
        });

        if !imported_ids.is_empty() {
            let store = controller.update(cx, |ctl, _| ctl.library.store().clone());
            embed_imported_images(controller.clone(), store, library_root, imported_ids, cx).await;
        }

        // Remove the processed inbox files (blobs were copied by staging).
        let inbox_dir = trove_core::collect::inbox_dir();
        for (name, sidecar) in cleanup {
            let _ = std::fs::remove_file(inbox_dir.join(&name));
            if let Some(sidecar) = sidecar {
                let _ = std::fs::remove_file(inbox_dir.join(sidecar));
            }
        }

        let note = if report.skipped.is_empty() {
            Notification::success(
                rust_i18n::t!("notice.collect_done", imported = report.imported_count())
                    .to_string(),
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
    true
}

/// File names (plus sidecar names when present) to delete after import.
fn cleanup_names(paths: &[PathBuf]) -> Vec<(String, Option<String>)> {
    let inbox = trove_core::collect::inbox_dir();
    paths
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let sidecar = inbox.join(format!("{name}.meta.json"));
            let sidecar_name = sidecar.is_file().then(|| format!("{name}.meta.json"));
            (name, sidecar_name)
        })
        .collect()
}
