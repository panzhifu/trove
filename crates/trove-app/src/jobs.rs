//! Import jobs: heavy file work on the background executor, database commits
//! on the main thread, with progress published to the [`LibraryController`].

use std::path::{Path, PathBuf};

use gpui_kit::*;

use trove_core::media::import::{self, StagedFile};

use crate::state::LibraryController;

type StageResult = std::result::Result<StagedFile, (PathBuf, String)>;

/// Stage every source file (copy + hash + probe) on a background thread.
fn stage_all(library_root: &Path, paths: Vec<PathBuf>) -> Vec<StageResult> {
    paths
        .iter()
        .map(|p| {
            import::stage_source(library_root, p).map_err(|e| (p.clone(), e.to_string()))
        })
        .collect()
}

/// Start an import from a set of file paths.
///
/// Works from any entry point that holds an `App` (button, file drop, ...):
/// the heavy staging is scheduled on the background executor and the database
/// commits run on the foreground executor where entity state lives.
pub fn import_paths_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
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

    let controller = controller.clone();
    let task = cx
        .background_executor()
        .spawn(async move { stage_all(&library_root, paths) });

    cx.spawn(async move |cx| {
        let staged = task.await;

        controller.update(cx, |ctl, cx| {
            let mut imported = 0usize;
            let mut skipped = 0usize;
            for item in staged {
                match item {
                    Ok(file) => {
                        match import::commit_staged(
                            ctl.library.store(),
                            into_collection,
                            Some(import::AutoCollection::SourceFolder),
                            &file,
                        ) {
                            Ok(_) => imported += 1,
                            Err(e) => {
                                skipped += 1;
                                eprintln!("skipped {}: {e}", file.path.display());
                            }
                        }
                    }
                    Err((path, reason)) => {
                        skipped += 1;
                        eprintln!("skipped {}: {reason}", path.display());
                    }
                }
            }
            ctl.import_progress(total);
            ctl.finish_import(imported, skipped);
            cx.notify();
        });
    })
    .detach();
}
