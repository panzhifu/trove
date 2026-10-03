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
use trove_core::tasks::{RetryPolicy, TaskId, TaskKind, TaskManager, TaskPriority};

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

/// The free tier's import gate: `true` when this install is not licensed and
/// the open library already holds [`crate::license::FREE_ASSET_CAP`] live
/// assets. The caller decides how loudly to say so.
pub(super) fn cap_refused(library: &trove_core::library::Library) -> bool {
    !crate::license::LicenseGate::for_current().permits_import(library.asset_count())
}

/// Put the refusal on the controller's notice — the same status line every
/// import outcome speaks through. Whoever dropped the files is watching, and
/// pointing at the decision is the conversion moment. The write is guarded on
/// equality, so a background pump ticking over a full library does not repaint
/// the UI every interval; existing assets are never touched by the gate.
pub(super) fn set_cap_notice(controller: &Entity<LibraryController>, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        let message = rust_i18n::t!(
            "workspace.import_cap_reached",
            cap = crate::license::FREE_ASSET_CAP
        )
        .to_string();
        if ctl.notice.as_deref() != Some(message.as_str()) {
            ctl.notice = Some(message);
            cx.notify();
        }
    });
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

    // The free tier's import gate. One shared check covers every door that
    // funnels through here — buttons, file drops, clipboard pastes, capture,
    // conversion — so a library past its cap simply refuses new imports
    // while everything already inside stays fully usable.
    let refused_by_cap = {
        let ctl = controller.read(cx);
        cap_refused(&ctl.library)
    };
    if refused_by_cap {
        set_cap_notice(controller, cx);
        return false;
    }

    // Snapshot everything the job needs from the controller, then hand off.
    let (manager, options, total) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return false;
        }
        // Fail fast when the target collection does not exist.
        if let Some(cid) = into_collection
            && ctl.library.collection(cid).ok().flatten().is_none()
        {
            return false;
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
            pre_gate: true,
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
    /// The free tier's cap: the waiting files stay in the inbox until the
    /// license is activated or space frees up. The pump treats this like
    /// `Refused` — retry on a later tick — without shouting every time.
    CapReached,
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

    // The gate runs only when files are actually waiting: an idle tick over
    // a full library must not shout. `CapReached` (not `Idle`) keeps the
    // pump's retry loop alive, so the batch lands on its own once the
    // license is activated or space frees up.
    let cap_reached;
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
        cap_reached = cap_refused(&ctl.library);
        let total = items.len();
        let options = ImportOptions {
            pre_gate: true,
            data_root: ctl.library.root().to_path_buf(),
            cache_root: ctl.library.cache().to_path_buf(),
            // A collected file lives in the incoming directory, which is not a
            // scratch area — the import leaves it there and links it.
            storage: trove_core::media::import::ImportStorage::Link,
            source: ImportSource::CollectInbox { items },
        };
        (ctl.library.tasks().clone(), options, total)
    };
    if cap_reached {
        set_cap_notice(controller, cx);
        return InboxDrain::CapReached;
    }

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
    if manager.is_active(&TaskKind::Import) || manager.is_active(&TaskKind::CollectInbox) {
        return false;
    }
    // Keep a copy of the inputs so a failed/cancelled run can be retried from
    // the status-bar panel; the job closure consumes the other half.
    let retry_options = options.clone();
    let label = kind.name().to_string();
    // High priority and retried twice. High because the user just asked for
    // this — the panel should not bury it under a backfill that is outranking
    // nothing. Retried because every way this job fails *as a whole* is an
    // opening one (open the database, set the pragmas, begin a batch), and
    // those are transient when another process holds the same library. A re-run
    // is safe: the dedup pre-check drops what the library already holds and the
    // commit path dedups by content hash, so files the first attempt committed
    // are skipped rather than imported twice.
    let Ok((task_id, rx)) = manager.start_with_retry_and_priority(
        kind.clone(),
        label.clone(),
        RetryPolicy::times(2),
        TaskPriority::High,
        move || {
            let options = options.clone();
            Box::new(move |ctx| import::run(&options, ctx))
        },
    ) else {
        return false;
    };

    controller.update(cx, |ctl, _| {
        ctl.begin_import(total);
        ctl.import_task = Some(ImportTaskHandle {
            manager: manager.clone(),
            task_id,
        });
        ctl.record_retry(Retryable::Import {
            kind: kind.clone(),
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
            JobStep::Completed(outcome) => {
                ctl.finish_import(
                    outcome.report.imported_count(),
                    outcome.report.skipped_count(),
                );
                let groups = detect_sequences_after_import(ctl, &outcome.report);
                if groups > 0 {
                    ctl.generation += 1;
                    ctl.notice = Some(
                        rust_i18n::t!("notice.sequence_detected", groups = groups).to_string(),
                    );
                }
            }
            JobStep::Aborted => ctl.finish_import(0, 0),
        },
        cx,
    );
    true
}

/// Frame runs among what this import just brought in, created as sequences.
///
/// Runs are detected over the newly imported files only — the same directory
/// and prefix rules [`trove_core::media::sequence::detect`] applies — and a
/// frame that already belongs to a run stays where it is, so re-importing a
/// folder over a watch root never splits an existing sequence into fragments.
/// This is the auto side of the "manual first" sequencing decision: grouping
/// is non-destructive and dissolvable in one click, the notice names what was
/// made, and the common case (a folder of frames dropped in whole) needs no
/// second dialog.
fn detect_sequences_after_import(
    ctl: &mut LibraryController,
    report: &trove_core::media::import::ImportReport,
) -> usize {
    use std::collections::HashMap;

    let imported: Vec<(uuid::Uuid, PathBuf)> = report
        .imported
        .iter()
        .filter(|item| item.kind == trove_core::model::AssetKind::Image)
        .filter_map(|item| {
            let path = ctl.library.asset_file(item.asset_id)?;
            Some((item.asset_id, path))
        })
        .collect();
    if imported.len() < trove_core::media::sequence::MIN_FRAMES {
        return 0;
    }
    let groups =
        trove_core::media::sequence::detect(imported.iter().map(|(_, path)| path.as_path()));
    let by_path: HashMap<&PathBuf, uuid::Uuid> =
        imported.iter().map(|(id, path)| (path, *id)).collect();
    let mut created = 0;
    for group in groups {
        let ids: Vec<uuid::Uuid> = group
            .frames
            .iter()
            .filter_map(|frame| by_path.get(&frame.path).copied())
            .collect();
        if ids.len() < trove_core::media::sequence::MIN_FRAMES {
            continue;
        }
        let fresh: Vec<uuid::Uuid> = ids
            .into_iter()
            .filter(|id| ctl.library.sequence_of(*id).ok().flatten().is_none())
            .collect();
        if fresh.len() < trove_core::media::sequence::MIN_FRAMES {
            continue;
        }
        if ctl
            .library
            .create_sequence(&fresh, trove_core::media::sequence::DEFAULT_FPS)
            .is_ok()
        {
            created += 1;
        }
    }
    created
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
