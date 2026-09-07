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
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, Disableable as _, IconName, Sizable, WindowExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::SUPPORTED;
use crate::state::LibraryController;
use trove_core::config::AppConfig;
use trove_core::keybindings::{self, KeyBindingConfig};

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
                .page(search_page(&controller))
                .page(maintenance_page(&controller))
                .page(language_page())
                .page(shortcuts_page());

            dialog
                .title(rust_i18n::t!("settings.title").to_string())
                .width(px(760.))
                .button_props(
                    gpui_kit::component::dialog::DialogButtonProps::default()
                        .show_cancel(true)
                        .cancel_text(rust_i18n::t!("settings.close").to_string()),
                )
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
    v_flex()
        .gap_1()
        .child(
            div()
                .w_full()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(current.display().to_string()),
        )
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
                                        cx.update(|cx| {
                                            switch_library(&controller, path, cx);
                                            cx.refresh_windows();
                                        });
                                    }
                                }
                            })
                            .detach();
                        }
                    }),
            ),
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
                rust_i18n::t!("settings.library_switch_failed", error = e.to_string()).to_string(),
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
                        trove_core::maintenance::rebuild_search_index(library)
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
                        Err(e) => {
                            rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string()
                        }
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
    options.extend(
        SUPPORTED
            .iter()
            .map(|(code, name)| (SharedString::from(*code), SharedString::from(*name))),
    );

    SettingPage::new(rust_i18n::t!("settings.language").to_string())
        .icon(IconName::Globe)
        .resettable(false)
        .group(
            SettingGroup::new().item(
                SettingItem::new(
                    rust_i18n::t!("settings.language").to_string(),
                    SettingField::dropdown(
                        options,
                        |_cx| {
                            let lang = AppConfig::load().language;
                            SharedString::from(lang.unwrap_or_else(|| SYSTEM_LANGUAGE.into()))
                        },
                        |value, cx| {
                            let language = (&*value != SYSTEM_LANGUAGE).then(|| value.to_string());
                            crate::i18n::set_language(language);
                            // The locale is a process global: repaint every open
                            // window and rebuild the (already localized) menus.
                            cx.refresh_windows();
                            crate::apply_menus(cx);
                        },
                    ),
                )
                .description(rust_i18n::t!("settings.language_desc").to_string()),
            ),
        )
}

// ============================ shortcuts page ================================

/// Shortcuts ▸ Keyboard: view and customize keybindings.
fn shortcuts_page() -> SettingPage {
    let page = SettingPage::new(rust_i18n::t!("settings.shortcuts").to_string())
        .icon(IconName::Settings)
        .resettable(false);

    let items = keybinding_items();
    let mut group = SettingGroup::new();
    for i in 0..items.len() {
        let action = items[i].action;
        let default_key = items[i].key;
        let ctx = items[i].context;
        let label = action_label(action);
        let ctx_label = ctx
            .map(context_label)
            .unwrap_or_else(|| context_label("global"));

        let action_row = action.to_string();
        let key_row = default_key.to_string();
        let ctx_row = ctx_label.clone();
        group = group.item(SettingItem::new(
            label.clone(),
            SettingField::render(move |_, _, cx| {
                keybinding_row(&action_row, &key_row, &ctx_row, cx)
            }),
        ));
    }

    // Add reset button at the bottom.
    group = group.item(
        SettingItem::new(
            rust_i18n::t!("shortcuts.shortcut_reset").to_string(),
            SettingField::render(move |_, _, _cx| {
                h_flex().w_full().justify_end().child(
                    Button::new("reset-keybindings")
                        .outline()
                        .small()
                        .label(rust_i18n::t!("shortcuts.shortcut_reset").to_string())
                        .on_click(move |_, _, cx| {
                            reset_keybindings(cx);
                        }),
                )
            }),
        )
        .description(rust_i18n::t!("shortcuts.shortcut_reset_done").to_string()),
    );

    page.group(group)
}

/// Render a single keybinding row: description + clickable key + context.
fn keybinding_row(action: &str, default_key: &str, context_label: &str, cx: &mut App) -> Div {
    let config = AppConfig::load();
    let display_key = config
        .keybindings
        .get(action)
        .cloned()
        .unwrap_or_else(|| default_key.to_string());
    let label = action_label(action);
    let action_owned = action.to_string();
    let default_owned = default_key.to_string();

    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(label),
        )
        .child(
            Button::new(format!("key-{action}"))
                .ghost()
                .xsmall()
                .label(display_key.to_uppercase())
                .on_click(move |_, window, cx| {
                    prompt_keybinding_change(&action_owned, &default_owned, window, cx);
                }),
        )
        .child(
            div()
                .text_xs()
                .w(px(60.))
                .text_right()
                .text_color(cx.theme().muted_foreground)
                .child(context_label.to_string()),
        )
}

/// Localized label for an action id.
fn action_label(action: &str) -> String {
    match action {
        "MoveLeft" => rust_i18n::t!("shortcuts.actions.MoveLeft").to_string(),
        "MoveRight" => rust_i18n::t!("shortcuts.actions.MoveRight").to_string(),
        "MoveUp" => rust_i18n::t!("shortcuts.actions.MoveUp").to_string(),
        "MoveDown" => rust_i18n::t!("shortcuts.actions.MoveDown").to_string(),
        "OpenPreview" => rust_i18n::t!("shortcuts.actions.OpenPreview").to_string(),
        "TrashSelected" => rust_i18n::t!("shortcuts.actions.TrashSelected").to_string(),
        "SelectAll" => rust_i18n::t!("shortcuts.actions.SelectAll").to_string(),
        "ClearSelection" => rust_i18n::t!("shortcuts.actions.ClearSelection").to_string(),
        "Undo" => rust_i18n::t!("shortcuts.actions.Undo").to_string(),
        "Redo" => rust_i18n::t!("shortcuts.actions.Redo").to_string(),
        "ImportFiles" => rust_i18n::t!("shortcuts.actions.ImportFiles").to_string(),
        "OpenSettings" => rust_i18n::t!("shortcuts.actions.OpenSettings").to_string(),
        "RefreshLibrary" => rust_i18n::t!("shortcuts.actions.RefreshLibrary").to_string(),
        other => other.to_string(),
    }
}

/// Localized context label.
fn context_label(context: &str) -> String {
    match context {
        "Workspace" => rust_i18n::t!("shortcuts.context.Workspace").to_string(),
        _ => rust_i18n::t!("shortcuts.context.global").to_string(),
    }
}

/// Prompt the user for a new keybinding via keyboard capture dialog.
fn prompt_keybinding_change(action_id: &str, default_key: &str, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::dialog::DialogButtonProps;
    use gpui_kit::component::input::{Input, InputState};

    let action_id = action_id.to_string();
    let default = default_key.to_string();
    let action_label_disp = action_label(&action_id);
    window.open_dialog(cx, move |dialog, window, cx| {
        let input_state = cx.new(|cx| InputState::new(window, cx).placeholder(default.clone()));
        let input_clone = input_state.clone();
        let action_ok = action_id.clone();
        dialog
            .title(rust_i18n::t!("shortcuts.shortcut_prompt").to_string())
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div().text_sm().text_color(cx.theme().foreground).child(
                            rust_i18n::t!(
                                "shortcuts.shortcut_prompt_hint",
                                action = action_label_disp.clone(),
                                default = default.clone()
                            )
                            .to_string(),
                        ),
                    )
                    .child(Input::new(&input_clone).small())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("shortcuts.shortcut_prompt_note").to_string()),
                    ),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("settings.change").to_string())
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                let value: String = input_clone.read(cx).value().to_string();
                let trimmed = value.trim().to_lowercase();
                if !trimmed.is_empty() {
                    let mut config = AppConfig::load();
                    config.keybindings.insert(action_ok.clone(), trimmed);
                    let _ = config.save();
                    cx.refresh_windows();
                }
                true
            })
    });
}

/// Reset all keybindings to defaults.
fn reset_keybindings(cx: &mut App) {
    let mut config = AppConfig::load();
    config.keybindings.clear();
    let _ = config.save();
    cx.refresh_windows();
}

/// Get all configurable keybindings.
fn keybinding_items() -> Vec<KeyBindingConfig> {
    keybindings::default_keybindings()
}

// ============================ search page ===================================

/// Search ▸ pick the "search by image" backend and (for the semantic
/// backend) point at the CLIP models + embed the library.
fn search_page(controller: &Entity<LibraryController>) -> SettingPage {
    SettingPage::new(rust_i18n::t!("settings.search").to_string())
        .icon(IconName::Search)
        .resettable(false)
        .group(
            SettingGroup::new()
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.search_mode").to_string(),
                        SettingField::dropdown(
                            search_mode_options(),
                            |_cx| {
                                let mode = AppConfig::load().search_mode();
                                SharedString::from(mode)
                            },
                            |value, cx| {
                                let mut config = AppConfig::load();
                                config.search_mode = Some(value.to_string());
                                if config.save().is_ok() {
                                    cx.refresh_windows();
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.search_mode_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.clip_model_file").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| clip_model_file_row(&controller, cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.clip_model_file_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.semantic_status").to_string(),
                        SettingField::render(move |_, _, cx| semantic_status_row(cx)),
                    )
                    .description(rust_i18n::t!("settings.semantic_status_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.embed_coverage").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| embed_coverage_row(&controller, cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.embed_coverage_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.embed_all").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| embed_all_row(controller.clone(), cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.embed_all_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.model_files_hint").to_string(),
                        SettingField::render(move |_, _, cx| {
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("settings.model_files_hint").to_string())
                        }),
                    )
                    .description(rust_i18n::t!("settings.model_files_hint_desc").to_string()),
                ),
        )
}

fn search_mode_options() -> Vec<(SharedString, SharedString)> {
    vec![
        (
            SharedString::from("visual"),
            rust_i18n::t!("settings.search_mode_visual")
                .into_owned()
                .into(),
        ),
        (
            SharedString::from("semantic"),
            rust_i18n::t!("settings.search_mode_semantic")
                .into_owned()
                .into(),
        ),
    ]
}

/// Model-file row: shows the resolved path, a Browse button (file picker)
/// and an Open-Directory button (reveals the folder in the file manager so
/// the user can drop the downloaded model + ONNX Runtime lib there).
fn clip_model_file_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let config = AppConfig::load();
    let path = config
        .clip_model_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ctl = controller.clone();
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(path),
        )
        .child(
            Button::new("open-model-dir")
                .ghost()
                .xsmall()
                .icon(IconName::Folder)
                .tooltip(rust_i18n::t!("settings.open_model_dir").to_string())
                .on_click(move |_, _, _cx| {
                    let dir = config
                        .clip_model_dir()
                        .unwrap_or_else(|| std::env::temp_dir());
                    reveal_in_file_manager(&dir);
                }),
        )
        .child(
            Button::new("browse-model-file")
                .outline()
                .small()
                .label(rust_i18n::t!("settings.browse").to_string())
                .on_click(move |_, _, cx| {
                    let ctl = ctl.clone();
                    prompt_model_file(&ctl, cx);
                }),
        )
}

/// Pick the single CLIP ONNX model file via the system dialog and persist it.
/// Reveal a directory in the system file manager (cross-platform).
fn reveal_in_file_manager(path: &std::path::Path) {
    let path = path.to_string_lossy().to_string();
    let _ = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(&path).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&path).spawn()
    } else {
        // Linux: try xdg-open, then fall back to common file managers.
        std::process::Command::new("xdg-open").arg(&path).spawn()
    };
}

fn prompt_model_file(controller: &Entity<LibraryController>, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(
            rust_i18n::t!("settings.select_model_file")
                .into_owned()
                .into(),
        ),
    });
    cx.spawn({
        let controller = controller.clone();
        async move |cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.first()
            {
                let path = path.to_path_buf();
                let _ = cx.update(|cx| {
                    let mut config = AppConfig::load();
                    config.clip_model_path = Some(path.clone());
                    let res = config.save();
                    if res.is_ok() {
                        // (Re)initialise the engine with the new model file.
                        let _ = trove_core::media::clip::configure(&path);
                        cx.refresh_windows();
                    }
                    let _ = controller.update(cx, |_, cx| cx.notify());
                });
            }
        }
    })
    .detach();
}

/// Shows the live engine status string with the concrete failure reason
/// when initialization failed, so the user knows what to fix.
fn semantic_status_row(cx: &mut App) -> Div {
    let status = trove_core::media::clip::semantic_status();
    let (label, color) = if status == "ready" {
        (
            rust_i18n::t!("settings.status_ready").to_string(),
            cx.theme().success,
        )
    } else if let Some(rest) = status.strip_prefix("failed:") {
        // Show the actual error (missing lib, missing file, load failure).
        (rest.to_string(), cx.theme().danger)
    } else {
        // Never configured: hint at the two prerequisites.
        let hint = format!(
            "{} (model.onnx + libonnxruntime.so)",
            rust_i18n::t!("settings.status_unconfigured").to_string(),
        );
        (hint, cx.theme().muted_foreground)
    };
    div().text_sm().text_color(color).child(label)
}

/// Embedding coverage: how many live images carry a CLIP vector already.
/// Re-computed on every settings render — two COUNT queries, negligible.
fn embed_coverage_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let (embedded, total) = controller
        .read(cx)
        .library
        .embedding_status()
        .unwrap_or((0, 0));
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(
            rust_i18n::t!(
                "settings.embed_coverage_value",
                embedded = embedded,
                total = total
            )
            .to_string(),
        )
}

/// Embed-all button. The Store is thread-confined (`Rc<RefCell>`), so the
/// batch runs as one asset per main-thread turn with a short yield between —
/// the UI repaints continuously instead of freezing for the whole batch.
/// Per-asset work lives in `Library::embed_one` (core).
fn embed_all_row(controller: Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().w_full().justify_end().child(
        Button::new("embed-all")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.embed_all").to_string())
            .on_click(move |_, _, cx| {
                if !trove_core::media::clip::semantic_ready() {
                    return;
                }
                let ctl = controller.clone();
                ctl.update(cx, |ctl, cx| {
                    ctl.busy = true;
                    ctl.notice = Some(rust_i18n::t!("settings.embedding").to_string());
                    cx.notify();
                });
                cx.spawn(async move |cx| {
                    // Plan on the main thread (Library is not Send).
                    let missing: Vec<uuid::Uuid> = ctl.update(cx, |ctl, _| {
                        trove_core::store::assets::images_missing_embedding(
                            ctl.library.store().conn(),
                        )
                        .map(|v| v.into_iter().map(|(id, _)| id).collect())
                        .unwrap_or_default()
                    });
                    let mut done = 0u64;
                    let mut skipped = 0u64;
                    for id in missing {
                        match ctl.update(cx, |ctl, _| ctl.library.embed_one(id)) {
                            Ok(true) => done += 1,
                            _ => skipped += 1,
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(20))
                            .await;
                    }
                    let _ = ctl.update(cx, |ctl, cx| {
                        ctl.busy = false;
                        ctl.notice = Some(
                            rust_i18n::t!(
                                "settings.embed_done",
                                done = done,
                                skipped = skipped
                            )
                            .to_string(),
                        );
                        cx.notify();
                    });
                })
                .detach();
            }),
    )
}
