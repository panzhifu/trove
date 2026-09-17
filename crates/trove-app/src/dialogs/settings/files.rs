//! Files page: where the bytes live.
//!
//! Three kinds of thing, in the order a user meets them: the open library and
//! the folders watched for it, what Trove is using on disk, and the jobs that
//! keep the derived files honest — thumbnails, backups, orphans, integrity.

use super::*;
use gpui_kit::component::chart::PieChart;
use trove_core::config::LibraryConfig;
use trove_core::services::storage::{DirUsage, StorageReport};

// =============================== files page ==================================

/// Files ▸ the libraries, storage and maintenance.
pub(super) fn files_page(
    controller: &Entity<LibraryController>,
    stats: LibraryStats,
    storage: Option<StorageReport>,
) -> SettingPage {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let status = controller.clone();
    let assets = stats.clone();
    SettingPage::new(t("settings.files"))
        .icon(IconName::HardDrive)
        .resettable(false)
        .group(current_library_group(controller))
        .group(watch_folders_group())
        .group(
            SettingGroup::new()
                .title(t("settings.storage"))
                .item(
                    SettingItem::render(move |_, _, cx| app_data_block(storage, cx))
                        .keywords([rust_i18n::t!("settings.storage_usage").to_string()]),
                )
                .item(
                    SettingItem::render(move |_, _, cx| stats_block(&assets, cx))
                        .keywords([rust_i18n::t!("settings.storage_assets").to_string()]),
                ),
        )
        .group(
            SettingGroup::new().title(t("settings.thumbnails")).item(
                SettingItem::new(
                    t("settings.rebuild_thumbs"),
                    SettingField::render({
                        let controller = controller.clone();
                        move |_, _, cx| thumbs_row(&controller, cx)
                    }),
                )
                .description(t("settings.rebuild_thumbs_desc")),
            ),
        )
        .group(backups_group(controller))
        .group(
            SettingGroup::new()
                .title(t("settings.cleanup"))
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
        .group(SettingGroup::new().item(SettingItem::new(
            t("settings.status"),
            SettingField::render(move |_, _, cx| status_row(&status, cx)),
        )))
}

// ============================== storage rings ================================

/// What Trove keeps on disk, as a ring: one slice per directory it owns, and
/// the total in the hole.
///
/// `None` while the background walk is still running — it is the first thing
/// the page shows after opening, and a stale-looking zero would be worse than
/// a sentence.
fn app_data_block(report: Option<StorageReport>, cx: &App) -> Div {
    let title = rust_i18n::t!("settings.storage_usage").to_string();
    let Some(report) = report else {
        return ring_row(title, None, cx);
    };

    // (label, measurement) — one line per directory, in a stable order.
    let rows: [(String, DirUsage); 6] = [
        (
            rust_i18n::t!("settings.storage_config").to_string(),
            report.config,
        ),
        (
            rust_i18n::t!("settings.storage_library").to_string(),
            report.library(),
        ),
        (
            rust_i18n::t!("settings.storage_backups").to_string(),
            report.backups,
        ),
        (
            rust_i18n::t!("settings.storage_cache").to_string(),
            report.cache(),
        ),
        (
            rust_i18n::t!("settings.storage_logs").to_string(),
            report.logs,
        ),
        (
            rust_i18n::t!("settings.storage_incoming").to_string(),
            report.incoming,
        ),
    ];

    let slices = rows
        .into_iter()
        .enumerate()
        .map(|(ix, (label, usage))| Slice {
            label: label.into(),
            detail: format!(
                "{} · {} {}",
                format_bytes(usage.bytes),
                usage.files,
                rust_i18n::t!("settings.storage_files")
            ),
            value: usage.bytes,
            color: slice_color(cx, ix),
        })
        .collect();
    ring_row(title, Some((slices, format_bytes(report.total.bytes))), cx)
}

/// What the library holds, as a ring: one slice per asset kind, by count.
///
/// The kinds are the only breakdown worth a ring here — how many assets are
/// in the trash, how large they are, how many tags exist: those are numbers,
/// not shares of a whole.
fn stats_block(stats: &LibraryStats, cx: &App) -> Div {
    let slices = stats
        .by_kind
        .iter()
        .map(|(kind, count)| Slice {
            label: kind_label(kind).into(),
            detail: count.to_string(),
            value: *count,
            color: slice_color(cx, kind_slot(kind)),
        })
        .collect();
    ring_row(
        rust_i18n::t!("settings.storage_assets").to_string(),
        Some((slices, stats.live.to_string())),
        cx,
    )
}

/// One ring row: the title where a setting's label sits, and the ring where
/// its field would — so the row lines up with the plain ones around it.
///
/// `data` is `None` while a measurement is still running.
fn ring_row(title: String, data: Option<(Vec<Slice>, String)>, cx: &App) -> Div {
    let row = h_flex().w_full().items_center().gap_4().child(
        div()
            .w(px(110.))
            .flex_shrink_0()
            .text_sm()
            .text_color(cx.theme().foreground)
            .child(title),
    );
    match data {
        Some((slices, center)) => row.child(ring(slices, center, cx)),
        None => row.child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("settings.storage_measuring").to_string()),
        ),
    }
}

/// A donut with its legend: the ring on the left, one line per slice on the
/// right, and the total in the hole — the one number the slices add up to.
///
/// The plot draws from the bounds it is handed, so the box around it is also
/// what decides how big the ring comes out; the radii are given explicitly so
/// the ring keeps its proportions whatever the row is measured at.
fn ring(slices: Vec<Slice>, center: String, cx: &App) -> Div {
    const SIZE: f32 = 128.;
    const THICKNESS: f32 = 28.;

    let outer = SIZE / 2. - 2.;
    let legend = slices.clone();

    h_flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap_4()
        .child(
            div()
                .relative()
                .w(px(SIZE))
                .h(px(SIZE))
                .flex_shrink_0()
                .child(
                    PieChart::new(slices)
                        .value(|slice| slice.value as f32)
                        .color(|slice| slice.color)
                        .inner_radius(outer - THICKNESS)
                        .outer_radius(outer),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(center),
                        ),
                ),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .children(legend.into_iter().map(|slice| legend_row(slice, cx))),
        )
}

/// One legend line: the slice's colour, its name, and its number.
fn legend_row(slice: Slice, cx: &App) -> Div {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(8.))
                .h(px(8.))
                .flex_shrink_0()
                .rounded(px(2.))
                .bg(slice.color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(slice.label),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(slice.detail),
        )
}

/// One slice of a ring: what it is, how much of the whole it holds, and the
/// colour it takes from the palette. The ring and the legend are built from
/// one list, so a slice's colour is never looked up twice.
#[derive(Clone)]
struct Slice {
    label: SharedString,
    /// The number, as the legend spells it out beside the label.
    detail: String,
    value: u64,
    color: Hsla,
}

/// The palette a ring and its legend share. Eight entries: one per asset kind,
/// the last of them the muted tone, so "other" reads as the leftovers.
fn slice_color(cx: &App, ix: usize) -> Hsla {
    let theme = cx.theme();
    match ix % 8 {
        0 => theme.chart_1,
        1 => theme.green,
        2 => theme.yellow,
        3 => theme.magenta,
        4 => theme.cyan,
        5 => theme.red,
        6 => theme.blue,
        _ => theme.muted_foreground,
    }
}

/// The asset kind's slot in the palette. Keyed on the kind rather than on the
/// slice's position, so a library without audio does not repaint its images.
fn kind_slot(kind: &trove_core::model::AssetKind) -> usize {
    use trove_core::model::AssetKind as Kind;
    match kind {
        Kind::Image => 0,
        Kind::Video => 1,
        Kind::Audio => 2,
        Kind::Document => 3,
        Kind::Archive => 4,
        Kind::Font => 5,
        Kind::Model => 6,
        Kind::Other => 7,
    }
}

/// Localized name for an asset kind.
fn kind_label(kind: &trove_core::model::AssetKind) -> String {
    use trove_core::model::AssetKind as Kind;
    let key = match kind {
        Kind::Image => "asset.kind.image",
        Kind::Video => "asset.kind.video",
        Kind::Audio => "asset.kind.audio",
        Kind::Document => "asset.kind.document",
        Kind::Archive => "asset.kind.archive",
        Kind::Font => "asset.kind.font",
        Kind::Model => "asset.kind.model",
        Kind::Other => "asset.kind.other",
    };
    rust_i18n::t!(key).to_string()
}

/// A byte count in the largest unit that keeps it readable: 1536 → "1.5 KB".
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
pub(super) fn start_job(controller: &Entity<LibraryController>, cx: &mut App) -> bool {
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
pub(super) fn finish_job(controller: &Entity<LibraryController>, message: String, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        ctl.busy = false;
        ctl.notice = Some(message);
        cx.notify();
    });
    cx.refresh_windows();
}

/// Run one maintenance job through the library's `TaskManager`: mutual
/// exclusion (one `Maintenance` job at a time, enforced by the manager),
/// task-list visibility, plus the busy flag the settings buttons read.
/// `plan` runs on the main thread (the library handle is not `Send`) and
/// produces the payload `job` consumes on the task thread; `done` publishes
/// the outcome back into the controller.
fn spawn_maintenance_job<T, P>(
    controller: &Entity<LibraryController>,
    label: &str,
    plan: impl FnOnce(&trove_core::library::Library) -> Result<P, trove_core::Error>,
    job: impl FnOnce(P) -> T + Send + 'static,
    done: impl FnOnce(&mut LibraryController, T) + Send + 'static,
    cx: &mut App,
) where
    T: Send + 'static,
    P: Send + 'static,
{
    // Plan on the main thread first, so a planning error never starts a job.
    let payload = match plan(&controller.read(cx).library) {
        Ok(payload) => payload,
        Err(e) => {
            finish_job(
                controller,
                rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string(),
                cx,
            );
            return;
        }
    };
    let manager = controller.read(cx).library.tasks().clone();
    let started = manager.start(
        trove_core::tasks::TaskKind::Maintenance,
        label,
        move |_ctx| {
            // Job outcomes are plain values: a panicked/failed job closes the
            // channel and surfaces as a failed task, not through this value.
            Ok::<_, String>(job(payload))
        },
    );
    let Ok((_task_id, rx)) = started else {
        return; // a maintenance job is already running
    };
    controller.update(cx, |ctl, cx| {
        ctl.busy = true;
        ctl.notice = Some(rust_i18n::t!("settings.maintenance_running").to_string());
        cx.notify();
    });
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        // The channel is blocking; park the recv on a pool thread.
        let outcome = cx
            .background_executor()
            .spawn(async move { rx.recv() })
            .await;
        controller.update(cx, |ctl, cx| {
            ctl.busy = false;
            match outcome {
                // `start` only delivers a value on success; a `Err` job or a
                // cancelled/failed task closes the channel instead.
                Ok(value) => done(&mut *ctl, value),
                // Failed / cancelled: the task event carries the details;
                // here we surface the failure in the status line.
                _ => {
                    ctl.notice = Some(
                        rust_i18n::t!("settings.job_failed", error = "job failed").to_string(),
                    );
                }
            }
            cx.notify();
            cx.refresh_windows();
        });
    })
    .detach();
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
    spawn_maintenance_job(
        controller,
        "thumbnail rebuild",
        |library| {
            // Thumbnails live in the cache root, not beside the database.
            let cache = library.cache().to_path_buf();
            trove_core::services::maintenance::plan_thumbnail_rebuild(library, force)
                .map(|plan| (cache, plan))
        },
        |(cache, plan)| {
            // Pure filesystem work.
            trove_core::services::maintenance::run_thumbnail_plan(&cache, plan)
        },
        |ctl, report: trove_core::services::maintenance::ThumbRebuildReport| {
            ctl.notice = Some(
                rust_i18n::t!(
                    "settings.rebuild_thumbs_done",
                    count = report.regenerated,
                    missing = report.missing_blobs
                )
                .to_string(),
            );
        },
        cx,
    );
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
    spawn_maintenance_job(
        controller,
        "integrity check",
        trove_core::services::maintenance::plan_integrity,
        trove_core::services::maintenance::run_integrity_plan,
        |ctl, report: trove_core::services::maintenance::IntegrityReport| {
            let issues = report.entries.len();
            ctl.notice = Some(if issues == 0 {
                rust_i18n::t!("settings.verify_done_clean", count = report.checked).to_string()
            } else {
                rust_i18n::t!(
                    "settings.verify_done_issues",
                    count = report.checked,
                    issues = issues
                )
                .to_string()
            });
            ctl.integrity_report = Some(report);
        },
        cx,
    );
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

// ============================== libraries ====================================

/// The open library: what it is called and where Trove keeps it. The path is
/// read-only — it is not the user's to pick, so the row offers the folder but
/// never spells it out.
fn current_library_group(controller: &Entity<LibraryController>) -> SettingGroup {
    let controller = controller.clone();
    SettingGroup::new()
        .title(rust_i18n::t!("settings.library").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.library_name").to_string(),
            SettingField::input(
                |_cx| SharedString::from(AppConfig::load().active_entry().name),
                |value, cx| {
                    let mut config = AppConfig::load();
                    let slug = config.active_slug();
                    if config.rename_library(&slug, &value).is_ok() {
                        cx.refresh_windows();
                    }
                },
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.library_current").to_string(),
            SettingField::render(move |_, _, cx| library_current_row(&controller, cx)),
        ))
}

// ============================ watched folders ================================

/// Add ▸ Watched folders: folders scanned for new files, which import
/// automatically (unfiled). The list re-reads the config on every settings
/// render, so add/remove applies immediately.
fn watch_folders_group() -> SettingGroup {
    let mut group = SettingGroup::new().title(rust_i18n::t!("settings.watch_folders").to_string());
    // The watch list belongs to the open library, not to the application: two
    // libraries can watch different folders.
    let config = LibraryConfig::load(&library_dir());

    group = group.item(SettingItem::new(
        rust_i18n::t!("settings.watch_enabled").to_string(),
        SettingField::switch(
            |_cx| LibraryConfig::load(&library_dir()).watch_folders_enabled(),
            |enabled, cx| {
                let dir = library_dir();
                let mut config = LibraryConfig::load(&dir);
                config.watch_folders_enabled = Some(enabled);
                let _ = config.save(&dir);
                cx.refresh_windows();
            },
        ),
    ));

    for path in config.watched_folders.clone() {
        group = group.item(SettingItem::new(
            path.display().to_string(),
            SettingField::render(move |_, _, cx| watch_folder_row(path.clone(), cx)),
        ));
    }
    group.item(SettingItem::new(
        rust_i18n::t!("settings.add_watch_folder").to_string(),
        SettingField::render(|_, _, cx| add_watch_folder_row(cx)),
    ))
}

/// The open library's data directory — where its `library.json` lives.
fn library_dir() -> PathBuf {
    AppConfig::load().active_entry().dir()
}

/// One watched-folder row: stop watching.
fn watch_folder_row(path: PathBuf, _cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new(format!("watch-remove-{}", path.display()))
            .ghost()
            .small()
            .icon(IconName::Close)
            .tooltip(rust_i18n::t!("settings.remove_watch_folder").to_string())
            .on_click(move |_, _, cx| {
                let dir = library_dir();
                let mut config = LibraryConfig::load(&dir);
                let _ = config.remove_watched_folder(&dir, &path);
                cx.refresh_windows();
            }),
    )
}

/// The add-row: pick a folder to watch.
fn add_watch_folder_row(_cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new("watch-add")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.add_watch_folder").to_string())
            .on_click(|_, _, cx| {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some(
                        rust_i18n::t!("settings.select_watch_folder")
                            .into_owned()
                            .into(),
                    ),
                });
                cx.spawn(async move |cx| {
                    if let Ok(Ok(Some(paths))) = rx.await
                        && let Some(path) = paths.first()
                    {
                        let path = path.to_path_buf();
                        cx.update(|cx| {
                            let dir = library_dir();
                            let mut config = LibraryConfig::load(&dir);
                            let _ = config.add_watched_folder(&dir, path);
                            cx.refresh_windows();
                        });
                    }
                })
                .detach();
            }),
    )
}

/// The open library's folder: one button that opens it, plus the last
/// switch / maintenance notice when there is one.
///
/// The path is not the user's to pick, so it is not spelled out here either —
/// the button is the whole row.
fn library_current_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let dir = AppConfig::load().active_entry().dir();
    let notice = controller.read(cx).notice.clone();
    v_flex()
        .gap_1()
        .when_some(notice, |col, notice| {
            col.child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(notice),
            )
        })
        .child(
            div().w_full().flex().justify_end().child(
                Button::new("open-library-dir")
                    .outline()
                    .small()
                    .icon(IconName::Folder)
                    .label(rust_i18n::t!("settings.open_data_dir").to_string())
                    .on_click({
                        let dir = dir.clone();
                        move |_, _, _| {
                            let _ = trove_core::services::open_external::open(
                                &dir,
                                trove_core::services::open_external::OpenTarget::Default,
                            );
                        }
                    }),
            ),
        )
}
