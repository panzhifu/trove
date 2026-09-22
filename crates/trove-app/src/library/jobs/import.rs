//! The import bridge: `trove_core::tasks::import` runs the whole pipeline on a
//! backend thread; this submodule is only the translator — it starts a job
//! (from a path drop, a clipboard paste, or the collect inbox), then polls that
//! job's events to drive the controller's import phase and the keyed progress
//! toast with its cancel button.

use std::path::PathBuf;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::media::import::ImportStorage;
use trove_core::tasks::import::{self, ImportOptions, ImportOutcome, ImportSource};
use trove_core::tasks::{TaskId, TaskKind, TaskManager};

use super::{JobStep, NoticeKey, watch_job};
use crate::library::{LibraryController, Retryable};

/// Marker for the import progress toast.
pub struct ImportNotice;

impl NoticeKey for ImportNotice {
    const ID: &'static str = "import-progress";

    fn running(controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        // No total yet: the job is still walking the folders it was handed, so
        // the bar has nothing to be a fraction of.
        let text = if total == 0 {
            rust_i18n::t!("notice.import_scanning").to_string()
        } else {
            rust_i18n::t!("notice.import_running", done = done, total = total).to_string()
        };
        Notification::info(text).action(cancel_button(controller.clone()))
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("workspace.trash_failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("notice.import_cancelled").to_string())
    }
}

/// Handle of the running import job, kept on the controller: the cancel
/// button presses it, and a library swap cancels-and-waits on it before
/// swapping the store.
pub struct ImportTaskHandle {
    pub manager: TaskManager,
    pub task_id: TaskId,
}

/// Start an import from a set of file paths into the currently browsed
/// collection. See [`import_paths_app_into`].
///
/// Works from any entry point that holds an `App` (button, file drop, ...):
/// the whole pipeline — staging *and* database commits — runs on the backend
/// task thread; this function only starts it and watches the events.
pub fn import_paths_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let into_collection = controller.read(cx).current_collection;
    import_paths_app_into(controller, paths, into_collection, window, cx);
}

/// Import files the library itself just produced and left in a temporary
/// spot — clipboard pastes. They are *copied* into the store rather than
/// linked: a link would point at `/tmp`, which the system is free to empty
/// at any moment, leaving an asset with no reachable original.
pub fn import_copied_app(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let into_collection = controller.read(cx).current_collection;
    start_paths_import(
        controller,
        paths,
        into_collection,
        ImportStorage::Copy,
        window,
        cx,
    );
}

/// Start an import into an explicit collection (`None` = unfiled).
/// Directories in `paths` are expanded into their contained files, so a
/// dropped folder imports everything inside it. Returns `false` when the
/// batch was refused (already importing, empty list, nothing importable or a
/// dead target collection) so callers can retry later — the folder watcher
/// relies on this to keep new files pending.
pub fn import_paths_app_into(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    into_collection: Option<uuid::Uuid>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // A user import links: the file stays where the user keeps it.
    start_paths_import(
        controller,
        paths,
        into_collection,
        ImportStorage::Link,
        window,
        cx,
    )
}

/// The shared importer entry: snapshot everything the job needs from the
/// controller, then hand off.
fn start_paths_import(
    controller: &Entity<LibraryController>,
    paths: Vec<PathBuf>,
    into_collection: Option<uuid::Uuid>,
    storage: ImportStorage,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if paths.is_empty() {
        return false;
    }

    // Snapshot everything the job needs from the controller, then hand off.
    let (manager, options, total) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return false;
        }
        // Fail fast when the target collection does not exist.
        if let Some(cid) = into_collection {
            let conn = ctl.library.store().conn();
            if trove_core::store::collections::get(conn, cid)
                .ok()
                .flatten()
                .is_none()
            {
                return false;
            }
        }
        // Count without walking. Progress totals must cover the folder
        // contents, not just the dropped entries, but the walk that counts them
        // belongs to the job (`tasks::import`): on a big folder it takes
        // seconds, and this thread used to pay for it — once here to learn the
        // total, and then again inside the job, for the same list. Zero means
        // "not known yet", which the UI renders as scanning.
        let total = if paths.iter().any(|p| p.is_dir()) {
            0
        } else {
            paths.len()
        };
        let options = ImportOptions {
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            storage,
            source: ImportSource::Paths {
                paths,
                into_collection,
            },
        };
        (ctl.library.tasks().clone(), options, total)
    };

    start_import_job(
        controller,
        &manager,
        TaskKind::Import,
        options,
        total,
        window,
        cx,
    )
}

/// What [`collect_inbox_app`] did with the waiting files.
///
/// The caller needs the difference: a refusal is the embedder's "try again
/// later" (an inbox signal that carried a file must not be dropped because an
/// unrelated import happened to be running), while `Idle` is a settled answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxDrain {
    /// A job is running over the waiting files.
    Started,
    /// Another import is running: nothing was touched, ask again later.
    Refused,
    /// Nothing was waiting, or the library already holds every waiting file.
    Idle,
}

/// Drain the collect-service inbox: import every waiting file (unfiled,
/// `source_url` stamped from the sidecar, files kept and linked).
///
/// The waiting-list comes from `collect::inbox_items` — the one definition
/// of what is waiting — rather than a hand-rolled enumeration: a private
/// copy of the skip rules is exactly how `.part` files (still being written)
/// and sidecars ended up being imported as assets in their own right. Files
/// the library already holds are dropped before the job is even started; see
/// [`waiting_files`].
pub fn collect_inbox_app(
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) -> InboxDrain {
    let items = trove_core::services::collect::inbox_items();
    if items.is_empty() {
        return InboxDrain::Idle;
    }

    let (manager, options, total) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return InboxDrain::Refused;
        }
        let items = waiting_files(ctl, items);
        if items.is_empty() {
            tracing::debug!("collect inbox: nothing waiting that the library lacks");
            return InboxDrain::Idle;
        }
        let total = items.len();
        let options = ImportOptions {
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            // A collected file lives in the incoming directory, which is not a
            // scratch area — the import leaves it there and links it.
            storage: trove_core::media::import::ImportStorage::Link,
            source: ImportSource::CollectInbox { items },
        };
        (ctl.library.tasks().clone(), options, total)
    };

    if start_import_job(
        controller,
        &manager,
        TaskKind::CollectInbox,
        options,
        total,
        window,
        cx,
    ) {
        InboxDrain::Started
    } else {
        // Both import kinds share one slot: the other one got there first.
        InboxDrain::Refused
    }
}

/// Drop the waiting files the library already holds.
///
/// The inbox is where imports *stay* — a collected page and a screenshot are
/// linked, not copied — so a wake-up over that directory is normally a batch
/// of files the library has had for days. Asking first (by the same loose
/// file-name-plus-size key the import itself skips on) keeps that from costing
/// a job, a scan and a progress notice that then has nothing to report.
fn waiting_files(
    ctl: &LibraryController,
    items: Vec<(PathBuf, Option<PathBuf>)>,
) -> Vec<(PathBuf, Option<PathBuf>)> {
    let paths: Vec<PathBuf> = items.iter().map(|(path, _)| path.clone()).collect();
    let unimported: std::collections::HashSet<PathBuf> =
        ctl.library.unimported_paths(&paths).into_iter().collect();
    items
        .into_iter()
        .filter(|(path, _)| unimported.contains(path))
        .collect()
}

/// Start the backend import job and detach the event watcher. Returns
/// `false` when a job of either import kind is already running.
///
/// `pub(super)` so the task panel's Retry entry point (in `analysis`) can
/// re-launch a failed/cancelled import from its saved inputs.
pub(super) fn start_import_job(
    controller: &Entity<LibraryController>,
    manager: &TaskManager,
    kind: TaskKind,
    options: ImportOptions,
    total: usize,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    // One import at a time across both kinds: the controller's phase is
    // shared, and two writer threads would race the progress reporting.
    // `is_active` (not `is_running`) so a *paused* import still holds the slot.
    if manager.is_active(TaskKind::Import) || manager.is_active(TaskKind::CollectInbox) {
        return false;
    }
    // Keep a copy of the inputs so a failed/cancelled run can be retried from
    // the status-bar panel; the job closure consumes the other half.
    let retry_options = options.clone();
    let label = kind.name().to_string();
    let Ok((task_id, rx)) = manager.start(kind, kind.name(), move |ctx| import::run(&options, ctx))
    else {
        return false;
    };

    controller.update(cx, |ctl, _| {
        ctl.begin_import(total);
        ctl.import_task = Some(ImportTaskHandle {
            manager: manager.clone(),
            task_id,
        });
        ctl.record_retry(Retryable::Import {
            kind,
            options: retry_options,
            total,
        });
        ctl.begin_task(task_id, kind, label);
    });
    // `total == 0` is the job's "still counting the folder" state: the scan
    // runs on the backend thread and the real total arrives as a progress
    // event, so the first toast must not claim "0 files".
    let started = if total == 0 {
        rust_i18n::t!("notice.import_scanning").to_string()
    } else {
        rust_i18n::t!("notice.import_started", count = total).to_string()
    };
    window.push_notification(
        ImportNotice::keyed(Notification::info(started).action(cancel_button(controller.clone()))),
        cx,
    );

    watch_job::<ImportNotice, _>(
        controller.clone(),
        manager.clone(),
        task_id,
        rx,
        window.window_handle(),
        outcome_toast,
        // The import phase is the one thing the shared task rows do not
        // carry: the grid's overlay and the explorer's disabled state read it.
        |ctl, step| match step {
            JobStep::Progress { done, total } => {
                ctl.import_progress(*done as usize, *total as usize);
            }
            JobStep::Completed(outcome) => ctl.finish_import(
                outcome.report.imported_count(),
                outcome.report.skipped_count(),
            ),
            JobStep::Aborted => ctl.finish_import(0, 0),
        },
        cx,
    );
    true
}

/// The completion toast: success when everything landed, a warning listing
/// skips otherwise, a plain info when the run was cancelled, an error naming
/// the database problem when batches failed. `None` for the run that did
/// nothing at all — the resident inbox sweep re-runs over its whole history
/// and ends "everything already imported"; toasting that every time would
/// train the user to dismiss import notices unread.
fn outcome_toast(outcome: &ImportOutcome) -> Option<Notification> {
    if outcome.cancelled {
        Some(Notification::info(
            rust_i18n::t!("notice.import_cancelled").to_string(),
        ))
    } else if let Some(error) = &outcome.error {
        Some(Notification::warning(
            rust_i18n::t!(
                "notice.import_error",
                error = error,
                imported = outcome.report.imported_count(),
                skipped = outcome.report.skipped_count()
            )
            .to_string(),
        ))
    } else if outcome.report.imported.is_empty()
        && outcome.report.skipped.is_empty()
        && outcome.report.already_imported > 0
    {
        None
    } else if outcome.report.skipped.is_empty() {
        Some(Notification::success(
            rust_i18n::t!(
                "notice.import_done",
                imported = outcome.report.imported_count()
            )
            .to_string(),
        ))
    } else {
        Some(Notification::warning(
            rust_i18n::t!(
                "notice.import_done_skipped",
                imported = outcome.report.imported_count(),
                skipped = outcome.report.skipped_count()
            )
            .to_string(),
        ))
    }
}

/// The progress toast's cancel button: asks the job to stop at its next
/// checkpoint; the outcome toast replaces this one when the job settles.
fn cancel_button(
    controller: Entity<LibraryController>,
) -> impl Fn(&mut Notification, &mut Window, &mut gpui_kit::Context<Notification>) -> Button {
    move |_notification, _window, _cx| {
        let controller = controller.clone();
        Button::new("import-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("notice.import_cancel").to_string())
            .on_click(move |_, _, cx| {
                controller.update(cx, |ctl, _| ctl.cancel_import());
            })
    }
}
