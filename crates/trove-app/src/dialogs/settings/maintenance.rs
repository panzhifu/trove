//! Maintenance page: thumbnail rebuild, FTS rebuild, orphan sweep,
//! integrity check and backups.

use super::*;

// ============================ maintenance page ===============================

/// Maintenance ▸ jobs from `trove-core::maintenance`.
pub(super) fn maintenance_page(controller: &Entity<LibraryController>) -> SettingPage {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let status = controller.clone();
    SettingPage::new(t("settings.maintenance"))
        .icon(IconName::RotateCw)
        .resettable(false)
        .group(
            SettingGroup::new()
                .item(
                    SettingItem::new(
                        t("settings.rebuild_thumbs"),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| thumbs_row(&controller, cx)
                        }),
                    )
                    .description(t("settings.rebuild_thumbs_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.rebuild_index"),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| index_row(&controller, cx)
                        }),
                    )
                    .description(t("settings.rebuild_index_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.clean_orphans"),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| orphans_row(&controller, cx)
                        }),
                    )
                    .description(t("settings.clean_orphans_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.verify_integrity"),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| integrity_row(&controller, cx)
                        }),
                    )
                    .description(t("settings.verify_integrity_desc")),
                ),
        )
        .group(backups_group(controller))
        .group(SettingGroup::new().item(SettingItem::new(
            t("settings.status"),
            SettingField::render(move |_, _, cx| status_row(&status, cx)),
        )))
}

// =============================== backups =====================================

/// Maintenance ▸ Backups: snapshot the database on demand and reveal the
/// snapshot folder. Auto-backups run at library open (daily throttle).
fn backups_group(controller: &Entity<LibraryController>) -> SettingGroup {
    let controller = controller.clone();
    SettingGroup::new()
        .title(rust_i18n::t!("settings.backups").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.backup_now").to_string(),
            SettingField::render(move |_, _, cx| backup_row(&controller, cx)),
        ))
}

/// The backups row: snapshot count, a "back up now" button and a reveal
/// button for the backups folder.
fn backup_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let (busy, count) = {
        let ctl = controller.read(cx);
        (ctl.busy, ctl.library.list_backups().len())
    };
    let backups_dir = controller.read(cx).library.root().join("backups");
    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    rust_i18n::t!(
                        "settings.backup_count",
                        count = count,
                        max = trove_core::services::backup::MAX_BACKUPS
                    )
                    .to_string(),
                ),
        )
        .child(
            Button::new("open-backups-dir")
                .ghost()
                .small()
                .icon(IconName::Folder)
                .tooltip(rust_i18n::t!("settings.open_backups_dir").to_string())
                .on_click(move |_, _, _cx| {
                    crate::panels::common::reveal_path(&backups_dir);
                }),
        )
        .child(
            Button::new("backup-now")
                .outline()
                .small()
                .disabled(busy)
                .label(rust_i18n::t!("settings.backup_now").to_string())
                .on_click({
                    let controller = controller.clone();
                    move |_, _, cx| {
                        let result = {
                            let library = &controller.read(cx).library;
                            library.create_backup()
                        };
                        let message = match result {
                            Ok(path) => rust_i18n::t!(
                                "settings.backup_done",
                                path = path.display().to_string()
                            )
                            .to_string(),
                            Err(e) => rust_i18n::t!("settings.job_failed", error = e.to_string())
                                .to_string(),
                        };
                        finish_job(&controller, message, cx);
                    }
                }),
        )
}

/// Mark the controller busy (unless a job is already running). Returns
/// whether the caller may start.
fn start_job(controller: &Entity<LibraryController>, cx: &mut App) -> bool {
    let mut started = false;
    controller.update(cx, |ctl, cx| {
        if !ctl.busy {
            ctl.busy = true;
            ctl.notice = Some(rust_i18n::t!("settings.maintenance_running").to_string());
            started = true;
        }
        cx.notify();
    });
    started
}

/// Finish a job: clear the busy flag and publish `message`.
fn finish_job(controller: &Entity<LibraryController>, message: String, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.busy = false;
        ctl.notice = Some(message);
        cx.notify();
    });
    cx.refresh_windows();
}

/// Thumbnails row: incremental rebuild plus a full "rewrite everything"
/// pass. The plan (which files need work) is collected on the main thread
/// because the library handle is not `Send`; the file work runs on the
/// background executor via the `Send` [`trove_core::services::maintenance::ThumbPlan`].
fn thumbs_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let (busy, controller2) = (controller.read(cx).busy, controller.clone());
    h_flex()
        .flex_1()
        .justify_end()
        .gap_2()
        .child(
            Button::new("rebuild-thumbs")
                .outline()
                .small()
                .disabled(busy)
                .label(rust_i18n::t!("settings.rebuild_thumbs").to_string())
                .on_click({
                    let controller = controller.clone();
                    move |_, _, cx| rebuild_thumbs(&controller, false, cx)
                }),
        )
        .child(
            Button::new("rebuild-thumbs-force")
                .outline()
                .small()
                .disabled(busy)
                .label(rust_i18n::t!("settings.rebuild_thumbs_force").to_string())
                .on_click(move |_, _, cx| rebuild_thumbs(&controller2, true, cx)),
        )
}

fn rebuild_thumbs(controller: &Entity<LibraryController>, force: bool, cx: &mut App) {
    if !start_job(controller, cx) {
        return;
    }
    // Plan on the main thread (needs the library), run on a worker thread
    // (pure filesystem work).
    let plan = {
        let library = &controller.read(cx).library;
        match trove_core::services::maintenance::plan_thumbnail_rebuild(library, force) {
            Ok(plan) => plan,
            Err(e) => {
                finish_job(
                    controller,
                    rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string(),
                    cx,
                );
                return;
            }
        }
    };
    let root = controller.read(cx).library.root().to_path_buf();

    let task = cx
        .background_executor()
        .spawn(async move { trove_core::services::maintenance::run_thumbnail_plan(&root, plan) });

    cx.spawn({
        let controller = controller.clone();
        async move |cx| {
            let report = task.await;
            cx.update(|cx| {
                finish_job(
                    &controller,
                    rust_i18n::t!(
                        "settings.rebuild_thumbs_done",
                        count = report.regenerated,
                        missing = report.missing_blobs
                    )
                    .to_string(),
                    cx,
                );
            });
        }
    })
    .detach();
}

/// Search-index row: a synchronous rebuild (database-bound, quick).
fn index_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().flex_1().justify_end().child(
        Button::new("rebuild-index")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.rebuild_index").to_string())
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    if !start_job(&controller, cx) {
                        return;
                    }
                    let result = {
                        let library = &controller.read(cx).library;
                        trove_core::services::maintenance::rebuild_search_index(library)
                    };
                    let message = match result {
                        Ok(count) => {
                            rust_i18n::t!("settings.rebuild_index_done", count = count).to_string()
                        }
                        Err(e) => {
                            rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string()
                        }
                    };
                    finish_job(&controller, message, cx);
                }
            }),
    )
}

/// Orphan-sweep row: synchronous (one filesystem walk over `media/` +
/// `thumbs/`).
fn orphans_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().flex_1().justify_end().child(
        Button::new("clean-orphans")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.clean_orphans").to_string())
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    if !start_job(&controller, cx) {
                        return;
                    }
                    let result = {
                        let library = &controller.read(cx).library;
                        trove_core::services::maintenance::clean_orphans(library)
                    };
                    let message = match result {
                        Ok(report) => rust_i18n::t!(
                            "settings.clean_orphans_done",
                            blobs = report.blobs_removed,
                            thumbs = report.thumbs_removed,
                            trashed = report.files_trashed,
                            dirs = report.empty_dirs_removed,
                        )
                        .to_string(),
                        Err(e) => {
                            rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string()
                        }
                    };
                    finish_job(&controller, message, cx);
                }
            }),
    )
}

/// Integrity-check row: a "verify" button; once a report exists, a summary
/// line and one row per problem asset with a trash action. Like thumbnails,
/// the plan is collected on the main thread (the library is not `Send`) and
/// the hashing runs on the background executor.
fn integrity_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    let report = controller.read(cx).integrity_report.clone();

    let mut bar = h_flex().w_full().justify_end().gap_2();
    if let Some(report) = &report {
        let clean = report.entries.is_empty();
        bar = bar.child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(if clean {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().danger
                })
                .child(if clean {
                    rust_i18n::t!("settings.verify_done_clean", count = report.checked).to_string()
                } else {
                    rust_i18n::t!(
                        "settings.verify_done_issues",
                        count = report.checked,
                        issues = report.entries.len()
                    )
                    .to_string()
                }),
        );
    }
    bar = bar.child(
        Button::new("verify-integrity")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.verify_start").to_string())
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| run_integrity_check(&controller, cx)
            }),
    );

    let mut col = v_flex().flex_1().gap_1().child(bar);
    if let Some(report) = report.filter(|r| !r.entries.is_empty()) {
        let mut list = v_flex().w_full().gap_1();
        for entry in &report.entries {
            list = list.child(integrity_entry_row(controller.clone(), entry.clone(), cx));
        }
        col = col.child(
            div()
                .w_full()
                .max_h(px(220.))
                .overflow_y_scrollbar()
                .child(list),
        );
    }
    col
}

/// One problem asset: file name, issue tag, and a move-to-trash action that
/// drops the entry from the report when it succeeds.
fn integrity_entry_row(
    controller: Entity<LibraryController>,
    entry: trove_core::services::maintenance::IntegrityEntry,
    cx: &App,
) -> Div {
    use trove_core::services::maintenance::IntegrityIssue as Issue;
    let (tag, tag_color) = match entry.issue {
        Issue::MissingBlob => (
            rust_i18n::t!("settings.verify_missing").to_string(),
            cx.theme().danger,
        ),
        Issue::HashMismatch => (
            rust_i18n::t!("settings.verify_corrupted").to_string(),
            cx.theme().warning,
        ),
    };
    let asset_id = entry.asset_id;
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .min_w_0()
                .flex_1()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(entry.file_name),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(tag_color)
                        .child(tag),
                ),
        )
        .child(
            Button::new(format!("integrity-trash-{asset_id}"))
                .outline()
                .xsmall()
                .label(rust_i18n::t!("app.move_to_trash").to_string())
                .on_click(move |_, _, cx| {
                    controller.update(cx, |ctl, cx| {
                        let ids = [asset_id];
                        if let Err(e) = ctl.library.trash_assets(&ids) {
                            ctl.notice = Some(
                                rust_i18n::t!("workspace.trash_failed", error = e.to_string())
                                    .to_string(),
                            );
                        } else if let Some(report) = ctl.integrity_report.as_mut() {
                            report.entries.retain(|e| e.asset_id != asset_id);
                        }
                        ctl.deselect(&ids);
                        ctl.generation += 1;
                        cx.notify();
                    });
                }),
        )
}

/// Run the integrity check: plan on the main thread, hash blobs on the
/// background executor, publish the report into the controller.
fn run_integrity_check(controller: &Entity<LibraryController>, cx: &mut App) {
    if !start_job(controller, cx) {
        return;
    }
    let plan = {
        let library = &controller.read(cx).library;
        match trove_core::services::maintenance::plan_integrity(library) {
            Ok(plan) => plan,
            Err(e) => {
                finish_job(
                    controller,
                    rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string(),
                    cx,
                );
                return;
            }
        }
    };

    let task = cx
        .background_executor()
        .spawn(async move { trove_core::services::maintenance::run_integrity_plan(plan) });

    cx.spawn({
        let controller = controller.clone();
        async move |cx| {
            let report = task.await;
            cx.update(|cx| {
                let issues = report.entries.len();
                let message = if issues == 0 {
                    rust_i18n::t!("settings.verify_done_clean", count = report.checked).to_string()
                } else {
                    rust_i18n::t!(
                        "settings.verify_done_issues",
                        count = report.checked,
                        issues = issues
                    )
                    .to_string()
                };
                controller.update(cx, |ctl, cx| {
                    ctl.busy = false;
                    ctl.notice = Some(message);
                    ctl.integrity_report = Some(report);
                    cx.notify();
                });
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

/// The shared status line: the notice of the last finished job, danger
/// colored (failures surface here too).
fn status_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let (notice, busy) = {
        let ctl = controller.read(cx);
        (ctl.notice.clone(), ctl.busy)
    };
    h_flex()
        .flex_1()
        .min_w_0()
        .justify_end()
        .child(
            div()
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(notice.unwrap_or_else(|| "—".into())),
        )
        .when(busy, |row| {
            row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("settings.maintenance_running").to_string()),
            )
        })
}
