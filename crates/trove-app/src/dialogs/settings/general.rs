//! General page: library location, import mode, recents, watched
//! folders, collect service and library statistics.

use super::*;

// ============================ general page ===================================

/// General ▸ Library: where the library lives, hot-switchable.
pub(super) fn general_page(
    controller: &Entity<LibraryController>,
    stats: LibraryStats,
) -> SettingPage {
    let controller = controller.clone();
    let location = controller.clone();
    SettingPage::new(rust_i18n::t!("settings.general").to_string())
        .icon(IconName::Settings)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.library").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.library_location").to_string(),
                        SettingField::render(move |_, _, cx| library_location_row(&location, cx)),
                    )
                    .description(rust_i18n::t!("settings.library_location_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.import_mode").to_string(),
                        SettingField::dropdown(
                            import_mode_options(),
                            |_cx| {
                                let linked = AppConfig::load().import_linked();
                                SharedString::from(if linked { "link" } else { "copy" })
                            },
                            |value, cx| {
                                let mut config = AppConfig::load();
                                config.import_mode = Some(value.to_string());
                                if config.save().is_ok() {
                                    cx.refresh_windows();
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.import_mode_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.font_sample").to_string(),
                        SettingField::input(
                            |_cx| SharedString::from(AppConfig::load().font_sample_text()),
                            |value, cx| {
                                let mut config = AppConfig::load();
                                let value = value.trim().to_string();
                                config.font_sample =
                                    if value.is_empty() { None } else { Some(value) };
                                if config.save().is_ok() {
                                    cx.refresh_windows();
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.font_sample_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.screenshot_command").to_string(),
                        SettingField::input(
                            |_cx| SharedString::from(AppConfig::load().screenshot_command_text()),
                            |value, _cx| {
                                let mut config = AppConfig::load();
                                let value = value.trim().to_string();
                                config.screenshot_command =
                                    if value.is_empty() { None } else { Some(value) };
                                let _ = config.save();
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.screenshot_command_desc").to_string()),
                ),
        )
        .group(recent_libraries_group(&controller))
        .group(watch_folders_group())
        .group(collect_group())
        .group(stats_group(stats))
}

// ========================= recent libraries ==================================

/// The two import modes: copy the file into the library, or link to it in
/// place (values match `AppConfig.import_mode`).
fn import_mode_options() -> Vec<(SharedString, SharedString)> {
    vec![
        (
            SharedString::from("copy"),
            rust_i18n::t!("settings.import_mode_copy")
                .into_owned()
                .into(),
        ),
        (
            SharedString::from("link"),
            rust_i18n::t!("settings.import_mode_link")
                .into_owned()
                .into(),
        ),
    ]
}

/// General ▸ Recent libraries: every previously opened library, one click to
/// hot-switch (excluding the currently open one, which is marked instead).
fn recent_libraries_group(controller: &Entity<LibraryController>) -> SettingGroup {
    let controller = controller.clone();
    let config = AppConfig::load();
    let current = config.resolved_library_path();
    let mut group =
        SettingGroup::new().title(rust_i18n::t!("settings.recent_libraries").to_string());
    let recent = config.recent_libraries.clone();
    if recent.is_empty() {
        group = group.item(SettingItem::new(
            rust_i18n::t!("settings.no_recent").to_string(),
            SettingField::render(|_, _, _| div()),
        ));
        return group;
    }
    for path in recent {
        let is_current = path == current;
        group = group.item(SettingItem::new(
            path.display().to_string(),
            SettingField::render({
                let controller = controller.clone();
                move |_, _, cx| recent_library_row(&controller, path.clone(), is_current, cx)
            }),
        ));
    }
    group
}

/// One recent-library row: switch / remove buttons (the current library
/// cannot be switched to or removed).
fn recent_library_row(
    controller: &Entity<LibraryController>,
    path: PathBuf,
    is_current: bool,
    cx: &mut App,
) -> Div {
    let row_id = format!("lib-{}", path.display());
    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .when(is_current, |row| {
            row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(rust_i18n::t!("settings.current_library").to_string()),
            )
        })
        .child(
            Button::new(format!("{row_id}-open"))
                .outline()
                .small()
                .disabled(is_current)
                .label(rust_i18n::t!("settings.open_library").to_string())
                .on_click({
                    let controller = controller.clone();
                    let path = path.clone();
                    move |_, _, cx| switch_library(&controller, path.clone(), cx)
                }),
        )
        .child(
            Button::new(format!("{row_id}-remove"))
                .ghost()
                .small()
                .icon(IconName::Close)
                .tooltip(rust_i18n::t!("settings.remove_recent").to_string())
                .on_click(move |_, _, cx| {
                    let mut config = AppConfig::load();
                    let _ = config.remove_recent_library(&path);
                    cx.refresh_windows();
                }),
        )
}

// ============================ watched folders ================================

/// General ▸ Watched folders: folders scanned for new files, which import
/// automatically (unfiled). The list re-reads the config on every settings
/// render, so add/remove applies immediately.
fn watch_folders_group() -> SettingGroup {
    let mut group = SettingGroup::new().title(rust_i18n::t!("settings.watch_folders").to_string());
    let config = AppConfig::load();
    let enabled = config.watch_folders_enabled();

    group = group.item(SettingItem::new(
        rust_i18n::t!("settings.watch_enabled").to_string(),
        SettingField::render(move |_, _, cx| watch_toggle_row(enabled, cx)),
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

/// The master-switch row: one button flipping `watch_folders_enabled`.
fn watch_toggle_row(enabled: bool, _cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new("watch-toggle")
            .outline()
            .small()
            .label(if enabled {
                rust_i18n::t!("settings.watch_on").to_string()
            } else {
                rust_i18n::t!("settings.watch_off").to_string()
            })
            .on_click(|_, _, cx| {
                let mut config = AppConfig::load();
                config.watch_folders_enabled = Some(!config.watch_folders_enabled());
                let _ = config.save();
                cx.refresh_windows();
            }),
    )
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
                let mut config = AppConfig::load();
                let _ = config.remove_watched_folder(&path);
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
                            let mut config = AppConfig::load();
                            let _ = config.add_watched_folder(path);
                            cx.refresh_windows();
                        });
                    }
                })
                .detach();
            }),
    )
}

// ============================ collect service ================================

/// General ▸ Collect service: the local HTTP endpoint a browser extension
/// (or curl) posts files to; they import automatically via the inbox.
fn collect_group() -> SettingGroup {
    let config = AppConfig::load();
    let enabled = config.collect_enabled();
    let port = config.collect_port();
    SettingGroup::new()
        .title(rust_i18n::t!("settings.collect").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.collect_enabled").to_string(),
            SettingField::render(move |_, _, cx| collect_toggle_row(enabled, cx)),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.collect_endpoint").to_string(),
            SettingField::render(move |_, _, cx| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("http://127.0.0.1:{port}"))
            }),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.collect_example").to_string(),
            SettingField::render(|_, _, cx| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("settings.collect_example_cmd").to_string())
            }),
        ))
}

/// The collect master-switch row.
fn collect_toggle_row(enabled: bool, _cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new("collect-toggle")
            .outline()
            .small()
            .label(if enabled {
                rust_i18n::t!("settings.collect_on").to_string()
            } else {
                rust_i18n::t!("settings.collect_off").to_string()
            })
            .on_click(|_, _, cx| {
                let mut config = AppConfig::load();
                config.collect_enabled = Some(!config.collect_enabled());
                let _ = config.save();
                // The server thread and watcher re-read the config each
                // cycle; toggling needs a restart to (un)bind the port.
                cx.refresh_windows();
            }),
    )
}

// ============================== statistics ===================================

/// General ▸ Statistics: the library-size snapshot (see [`StatsSnapshot`]),
/// taken when the window opened / the last job finished — not per render.
fn stats_group(stats: LibraryStats) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.stats").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.stats").to_string(),
            SettingField::render(move |_, _, cx| stats_block(&stats, cx)),
        ))
}

/// The label/value rows for the statistics item.
fn stats_block(stats: &LibraryStats, cx: &mut App) -> Div {
    let kind_label = |kind: &trove_core::model::AssetKind| {
        let key = match kind {
            trove_core::model::AssetKind::Image => "asset.kind.image",
            trove_core::model::AssetKind::Video => "asset.kind.video",
            trove_core::model::AssetKind::Audio => "asset.kind.audio",
            trove_core::model::AssetKind::Document => "asset.kind.document",
            trove_core::model::AssetKind::Archive => "asset.kind.archive",
            trove_core::model::AssetKind::Font => "asset.kind.font",
            trove_core::model::AssetKind::Model => "asset.kind.model",
            trove_core::model::AssetKind::Other => "asset.kind.other",
        };
        rust_i18n::t!(key).to_string()
    };

    let mut rows: Vec<(String, String)> = vec![
        (
            rust_i18n::t!("stats.assets").to_string(),
            stats.live.to_string(),
        ),
        (
            rust_i18n::t!("stats.trashed").to_string(),
            stats.trashed.to_string(),
        ),
    ];
    for (kind, count) in &stats.by_kind {
        rows.push((kind_label(kind), count.to_string()));
    }
    rows.push((
        rust_i18n::t!("stats.total_size").to_string(),
        crate::panels::common::human_bytes(stats.total_bytes),
    ));
    rows.push((
        rust_i18n::t!("stats.collections").to_string(),
        stats.collections.to_string(),
    ));
    rows.push((
        rust_i18n::t!("stats.smart").to_string(),
        stats.smart_collections.to_string(),
    ));
    rows.push((
        rust_i18n::t!("stats.tags").to_string(),
        stats.tags.to_string(),
    ));

    v_flex()
        .gap_1()
        .children(rows.into_iter().map(|(label, value)| {
            h_flex()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(value),
                )
        }))
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
            config.set_library_path(path.clone())?;
            config.push_recent_library(path.clone())
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
