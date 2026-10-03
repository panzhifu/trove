//! The migration job bridge: scan a foreign library (Eagle / Billfish),
//! link-import its files, then write the carried metadata back — one
//! `TaskManager` job with the shared keyed-toast progress and the import
//! phase's status-bar plumbing (a migration *is* an import that carries
//! metadata, so it reuses `begin_import` / `import_progress` / `finish_import`
//! and the same one-writer slot).

use std::path::PathBuf;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use trove_core::tasks::migration::MigrationOptions;
use trove_core::tasks::{RetryPolicy, TaskKind, TaskManager, TaskPriority};

use super::import::{ImportTaskHandle, cap_refused, set_cap_notice};
use super::{JobStep, NoticeKey, watch_job};
use crate::library::LibraryController;

/// Marker for the keyed migration progress toast.
pub struct MigrateNotice;

impl NoticeKey for MigrateNotice {
    const ID: &'static str = "migrate-progress";

    fn running(_controller: &Entity<LibraryController>, done: u64, total: u64) -> Notification {
        Notification::info(rust_i18n::t!("migrate.running", done = done, total = total).to_string())
    }

    fn failed(error: &str) -> Notification {
        Notification::warning(rust_i18n::t!("migrate.failed", error = error).to_string())
    }

    fn cancelled() -> Notification {
        Notification::info(rust_i18n::t!("migrate.cancelled").to_string())
    }
}

/// Start the migration job. `total` is the plan's item count — the dialog
/// scans before offering the button, so it knows. Returns false when another
/// import-shaped job holds the slot or the free-tier cap refuses.
pub fn start_migration_job_app(
    controller: &Entity<LibraryController>,
    source: PathBuf,
    total: usize,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let (manager, options) = {
        let ctl = controller.read(cx);
        if ctl.is_importing() {
            return false;
        }
        if cap_refused(&ctl.library) {
            set_cap_notice(controller, cx);
            return false;
        }
        (
            ctl.library.tasks().clone(),
            MigrationOptions {
                source,
                data_root: ctl.library.root().to_path_buf(),
                cache_root: ctl.library.cache().to_path_buf(),
            },
        )
    };
    // One writer at a time across the import-shaped kinds.
    if manager.is_active(&TaskKind::Import)
        || manager.is_active(&TaskKind::CollectInbox)
        || manager.is_active(&TaskKind::Migration)
    {
        return false;
    }

    let label = TaskKind::Migration.name().to_string();
    let Ok((task_id, rx)) = manager.start_with_retry_and_priority(
        TaskKind::Migration,
        label.clone(),
        RetryPolicy::times(2),
        TaskPriority::High,
        move || {
            let options = options.clone();
            Box::new(move |ctx| trove_core::tasks::migration::run(&options, ctx))
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
        ctl.begin_task(task_id, TaskKind::Migration, label);
    });
    window.push_notification(
        MigrateNotice::keyed(
            Notification::info(rust_i18n::t!("migrate.started", count = total).to_string())
                .action(migrate_cancel_button(controller.clone())),
        ),
        cx,
    );

    watch_job::<MigrateNotice, _>(
        controller.clone(),
        manager.clone(),
        task_id,
        rx,
        window.window_handle(),
        // The closing toast reads the migration's own outcome shape.
        |outcome: &trove_core::tasks::migration::MigrationOutcome| {
            if outcome.cancelled {
                return Some(MigrateNotice::keyed(Notification::info(
                    rust_i18n::t!("migrate.cancelled").to_string(),
                )));
            }
            if let Some(error) = &outcome.error {
                return Some(MigrateNotice::keyed(Notification::warning(
                    rust_i18n::t!("migrate.failed", error = error).to_string(),
                )));
            }
            let report = &outcome.report;
            Some(MigrateNotice::keyed(Notification::success(
                rust_i18n::t!(
                    "migrate.done",
                    imported = report.imported,
                    reused = report.reused,
                    tagged = report.tagged,
                    collections = report.collections_created
                )
                .to_string(),
            )))
        },
        |ctl, step| match step {
            JobStep::Progress { done, total } => {
                ctl.import_progress(*done as usize, *total as usize);
            }
            JobStep::Completed(outcome) => {
                ctl.finish_import(
                    (outcome.report.imported + outcome.report.reused) as usize,
                    outcome.report.skipped as usize,
                );
                ctl.generation += 1;
            }
            JobStep::Aborted => ctl.finish_import(0, 0),
        },
        cx,
    );
    true
}

/// The progress toast's cancel button — the same controller method the
/// import toast uses, pointed at the migration's task handle.
fn migrate_cancel_button(
    controller: Entity<LibraryController>,
) -> impl Fn(&mut Notification, &mut Window, &mut gpui_kit::Context<Notification>) -> Button {
    move |_notification, _window, _cx| {
        let controller = controller.clone();
        Button::new("migrate-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("notice.import_cancel").to_string())
            .on_click(move |_, _, cx| {
                controller.update(cx, |ctl, _| ctl.cancel_import());
            })
    }
}

/// Start a migration that lands in a **new** library named after the source —
/// the entry the library-manager window offers, where no library is open to
/// receive one. The job runs on its own manager with a keyed toast in this
/// window; when it settles the sidebar already lists the fresh library and a
/// double-click enters it.
///
/// Returns false when the new library could not be registered.
pub fn start_migration_standalone(
    source: PathBuf,
    library_name: &str,
    total: usize,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let library_name = library_name.trim().to_string();
    use trove_core::config::AppConfig;
    use trove_core::tasks::TaskEvent;

    let entry = match AppConfig::load().add_library(&library_name) {
        Ok(entry) => entry,
        Err(error) => {
            tracing::error!(%error, "could not register the migration target library");
            window.push_notification(
                Notification::warning(rust_i18n::t!("migrate.failed", error = error).to_string()),
                cx,
            );
            return false;
        }
    };

    let options = MigrationOptions {
        source,
        data_root: entry.dir(),
        cache_root: entry.cache_dir(),
    };
    let manager = TaskManager::new();
    let label = TaskKind::Migration.name().to_string();
    let Ok((task_id, rx)) = manager.start(TaskKind::Migration, label, move |ctx| {
        trove_core::tasks::migration::run(&options, ctx)
    }) else {
        return false;
    };

    window.push_notification(
        MigrateNotice::keyed(
            Notification::info(rust_i18n::t!("migrate.started", count = total).to_string())
                .action(migrate_cancel_button_standalone(manager.clone(), task_id)),
        ),
        cx,
    );

    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let mut last: Option<(u64, u64)> = None;
        loop {
            cx.background_executor().timer(super::POLL_INTERVAL).await;
            let mut settled: Option<Notification> = None;
            for event in manager.poll_events_for(task_id) {
                if let TaskEvent::Progress { done, total, .. } = event {
                    last = Some((done, total));
                }
            }
            if let Some((done, total)) = last {
                let _ = handle.update(cx, |_, window, cx| {
                    window.push_notification(
                        MigrateNotice::keyed(Notification::info(
                            rust_i18n::t!("migrate.running", done = done, total = total)
                                .to_string(),
                        )),
                        cx,
                    );
                });
            }
            match rx.try_recv() {
                Ok(outcome) => {
                    settled = if outcome.cancelled {
                        Some(MigrateNotice::keyed(Notification::info(
                            rust_i18n::t!("migrate.cancelled").to_string(),
                        )))
                    } else if let Some(error) = &outcome.error {
                        Some(MigrateNotice::keyed(Notification::warning(
                            rust_i18n::t!("migrate.failed", error = error).to_string(),
                        )))
                    } else {
                        let report = &outcome.report;
                        Some(MigrateNotice::keyed(Notification::success(
                            rust_i18n::t!(
                                "migrate.standalone_done",
                                name = library_name,
                                imported = report.imported,
                                reused = report.reused
                            )
                            .to_string(),
                        )))
                    };
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    settled = Some(MigrateNotice::keyed(Notification::warning(
                        rust_i18n::t!("migrate.failed", error = "job lost").to_string(),
                    )));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if !manager.is_active(&TaskKind::Migration) {
                        settled = Some(MigrateNotice::keyed(Notification::warning(
                            rust_i18n::t!("migrate.failed", error = "job ended").to_string(),
                        )));
                    }
                }
            }
            if let Some(note) = settled {
                let _ = handle.update(cx, |_, window, cx| {
                    window.push_notification(note, cx);
                    cx.refresh_windows();
                });
                return;
            }
        }
    })
    .detach();
    true
}

fn migrate_cancel_button_standalone(
    manager: TaskManager,
    task_id: trove_core::tasks::TaskId,
) -> impl Fn(&mut Notification, &mut Window, &mut gpui_kit::Context<Notification>) -> Button {
    move |_notification, _window, _cx| {
        let manager = manager.clone();
        Button::new("migrate-cancel-standalone")
            .outline()
            .small()
            .label(rust_i18n::t!("notice.import_cancel").to_string())
            .on_click(move |_, _, _cx| {
                manager.cancel(task_id);
            })
    }
}
