//! Settings dialog: sidebar pages built on gpui-kit's `Settings` widget
//! (the same scaffold OpenLogi uses for its settings window — page
//! navigation, groups and data-bound fields all come from the component).
//!
//! The dialog carries the [`LibraryController`]: the library-location row
//! hot-swaps the open library (no restart), and the maintenance page runs
//! the `trove-core` maintenance jobs — thumbnail rebuild on the background
//! executor, FTS rebuild and orphan sweep inline (they are database-bound
//! and quick). Outcomes land on [`LibraryController::notice`], a status
//! line both the general and the maintenance page render.
//!
//! Pages are assembled inside the dialog's content closure so a live
//! language switch re-localizes every title on the next refresh.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, Disableable as _, IconName, Sizable, WindowExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::SUPPORTED;
use crate::state::LibraryController;
use trove_core::config::AppConfig;

/// Sentinel value for the "follow the system language" choice, which the
/// config stores as `None`.
const SYSTEM_LANGUAGE: &str = "system";

pub struct SettingsDialog;

impl SettingsDialog {
    /// Open the settings dialog for the given library controller.
    pub fn open(window: &mut Window, cx: &mut App, controller: Entity<LibraryController>) {
        window.open_dialog(cx, move |dialog, _, _| {
            // Pages are rebuilt on every dialog render, so titles follow the
            // active locale after `refresh_windows`.
            let settings = Settings::new("trove-settings")
                .sidebar_width(px(170.))
                .page(general_page(&controller))
                .page(maintenance_page(&controller))
                .page(language_page());

            dialog
                .title(rust_i18n::t!("settings.title").to_string())
                .width(px(760.))
                .child(v_flex().w_full().h(px(520.)).child(settings))
        });
    }
}

// ============================ general page ===================================

/// General ▸ Library: where the library lives, hot-switchable.
fn general_page(controller: &Entity<LibraryController>) -> SettingPage {
    let controller = controller.clone();
    SettingPage::new(rust_i18n::t!("settings.general").to_string())
        .icon(IconName::Settings)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.library").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.library_location").to_string(),
                        SettingField::render(move |_, _, cx| library_location_row(&controller, cx)),
                    )
                    .description(rust_i18n::t!("settings.library_location_desc").to_string()),
                ),
        )
}

/// The library-location row: the resolved path, the last switch/maintenance
/// notice (in the danger color), plus a Browse button that persists the
/// choice AND hot-swaps the open library without a restart.
fn library_location_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let current = AppConfig::load().resolved_library_path();
    let notice = controller.read(cx).notice.clone();
    h_flex()
        .flex_1()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(current.display().to_string()),
        )
        .when_some(notice, |row, notice| {
            row.child(
                div()
                    .max_w(px(240.))
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(notice),
            )
        })
        .child(
            Button::new("browse-library")
                .outline()
                .small()
                .label(rust_i18n::t!("settings.browse").to_string())
                .on_click({
                    let controller = controller.clone();
                    move |_, _, cx| {
                        let rx = cx.prompt_for_paths(PathPromptOptions {
                            files: false,
                            directories: true,
                            multiple: false,
                            prompt: Some(
                                rust_i18n::t!("settings.select_folder").into_owned().into(),
                            ),
                        });
                        cx.spawn({
                            let controller = controller.clone();
                            async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.first()
                                {
                                    let path: PathBuf = path.to_path_buf();
                                    let _ = cx.update(|cx| {
                                        switch_library(&controller, path, cx);
                                        cx.refresh_windows();
                                    });
                                }
                            }
                        })
                        .detach();
                    }
                }),
        )
}

/// Hot-switch the open library to `path`: swap the controller's library,
/// and only persist the choice when the library actually opened. Any error
/// is reported on [`LibraryController::notice`].
fn switch_library(controller: &Entity<LibraryController>, path: PathBuf, cx: &mut App) {
    controller.update(cx, |ctl, cx| {
        let outcome = ctl.swap_library(path.clone()).and_then(|()| {
            let mut config = AppConfig::load();
            config.set_library_path(path.clone())
        });
        ctl.notice = match outcome {
            Ok(()) => None,
            Err(e) => Some(
                rust_i18n::t!("settings.library_switch_failed", error = e.to_string())
                    .to_string(),
            ),
        };
        cx.notify();
    });
}

// ============================ maintenance page ===============================

/// Maintenance ▸ jobs from `trove-core::maintenance`.
fn maintenance_page(controller: &Entity<LibraryController>) -> SettingPage {
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
                ),
        )
        .group(SettingGroup::new().item(SettingItem::new(
            t("settings.status"),
            SettingField::render(move |_, _, cx| status_row(&status, cx)),
        )))
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
/// background executor via the `Send` [`trove_core::maintenance::ThumbPlan`].
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
        match trove_core::maintenance::plan_thumbnail_rebuild(library, force) {
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
        .spawn(async move { trove_core::maintenance::run_thumbnail_plan(&root, plan) });

    cx.spawn({
        let controller = controller.clone();
        async move |cx| {
            let report = task.await;
            let _ = cx.update(|cx| {
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
    h_flex()
        .flex_1()
        .justify_end()
        .child(
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
                            trove_core::maintenance::rebuild_search_index(library)
                        };
                        let message = match result {
                            Ok(count) => rust_i18n::t!(
                                "settings.rebuild_index_done",
                                count = count
                            )
                            .to_string(),
                            Err(e) => rust_i18n::t!(
                                "settings.job_failed",
                                error = e.to_string()
                            )
                            .to_string(),
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
    h_flex()
        .flex_1()
        .justify_end()
        .child(
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
                            trove_core::maintenance::clean_orphans(library)
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
                            Err(e) => rust_i18n::t!(
                                "settings.job_failed",
                                error = e.to_string()
                            )
                            .to_string(),
                        };
                        finish_job(&controller, message, cx);
                    }
                }),
        )
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

// ============================ language page ==================================

/// Language ▸ Interface: a dropdown of supported catalogs plus the
/// "follow system" sentinel. Switching applies the locale immediately.
fn language_page() -> SettingPage {
    let mut options: Vec<(SharedString, SharedString)> = vec![(
        SharedString::from(SYSTEM_LANGUAGE),
        rust_i18n::t!("settings.follow_system").into_owned().into(),
    )];
    options.extend(SUPPORTED.iter().map(|(code, name)| {
        (SharedString::from(*code), SharedString::from(*name))
    }));

    SettingPage::new(rust_i18n::t!("settings.language").to_string())
        .icon(IconName::Globe)
        .resettable(false)
        .group(SettingGroup::new().item(
            SettingItem::new(
                rust_i18n::t!("settings.language").to_string(),
                SettingField::dropdown(
                    options,
                    |_cx| {
                        let lang = AppConfig::load().language;
                        SharedString::from(lang.unwrap_or_else(|| SYSTEM_LANGUAGE.into()))
                    },
                    |value, cx| {
                        let language =
                            (&*value != SYSTEM_LANGUAGE).then(|| value.to_string());
                        crate::i18n::set_language(language);
                        // The locale is a process global: repaint every open
                        // window and rebuild the (already localized) menus.
                        cx.refresh_windows();
                        crate::apply_menus(cx);
                    },
                ),
            )
            .description(rust_i18n::t!("settings.language_desc").to_string()),
        ))
}
