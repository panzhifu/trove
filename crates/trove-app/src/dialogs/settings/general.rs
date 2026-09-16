//! General page: libraries, watched folders, collect service, preview zoom
//! limits and library statistics. The release check and the interface
//! language live on the About page.

use super::*;
use gpui_kit::component::setting::NumberFieldOptions;
use trove_core::config::LibraryConfig;

// ============================ general page ===================================

/// General ▸ Libraries: which one is open, how to make another, where they
/// live. The path is not the user's to choose — Trove keeps every library in
/// the platform's data directory.
pub(super) fn general_page(
    controller: &Entity<LibraryController>,
    stats: LibraryStats,
) -> SettingPage {
    let controller = controller.clone();
    SettingPage::new(rust_i18n::t!("settings.general").to_string())
        .icon(IconName::Settings)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.library").to_string())
                .item(
                    SettingItem::new(
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
                    )
                    .description(rust_i18n::t!("settings.library_name_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.library_current").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| library_current_row(&controller, cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.library_current_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.point_enhance").to_string(),
                        SettingField::render(|_, _, cx| point_enhance_row(cx)),
                    )
                    .description(rust_i18n::t!("settings.point_enhance_desc").to_string()),
                ),
        )
        .group(libraries_group(&controller))
        .group(watch_folders_group())
        .group(collect_group())
        .group(stats_group(stats))
        .group(zoom_group())
}

// ================================ libraries ==================================

/// General ▸ Libraries: every registered library, one click to switch. The
/// directories are Trove's business — a library is a name, not a path the
/// user has to think about.
fn libraries_group(controller: &Entity<LibraryController>) -> SettingGroup {
    let config = AppConfig::load();
    let current = config.active_slug();
    let mut group = SettingGroup::new().title(rust_i18n::t!("settings.libraries").to_string());
    let others: Vec<trove_core::config::LibraryEntry> = config
        .libraries
        .iter()
        .filter(|l| l.slug != current)
        .cloned()
        .collect();
    if others.is_empty() {
        group = group.item(SettingItem::new(
            rust_i18n::t!("settings.no_other_libraries").to_string(),
            SettingField::render(|_, _, _| div()),
        ));
    }
    for entry in others {
        group = group.item(SettingItem::new(
            entry.name.clone(),
            SettingField::render({
                let controller = controller.clone();
                move |_, _, cx| library_row(&controller, entry.clone(), cx)
            }),
        ));
    }
    group.item(
        SettingItem::new(
            rust_i18n::t!("settings.new_library").to_string(),
            SettingField::render({
                let controller = controller.clone();
                move |_, _, cx| new_library_row(&controller, cx)
            }),
        )
        .description(rust_i18n::t!("settings.new_library_desc").to_string()),
    )
}

/// One library row: switch to it, or drop it from the registry. Deleting a
/// library deletes its database; not one user file, because every asset is a
/// link to something that lives outside.
fn library_row(
    controller: &Entity<LibraryController>,
    entry: trove_core::config::LibraryEntry,
    _cx: &mut App,
) -> Div {
    let row_id = format!("lib-{}", entry.slug);
    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .child(
            Button::new(format!("{row_id}-open"))
                .outline()
                .small()
                .label(rust_i18n::t!("settings.open_library").to_string())
                .on_click({
                    let controller = controller.clone();
                    let entry = entry.clone();
                    move |_, _, cx| switch_library(&controller, entry.clone(), cx)
                }),
        )
        .child(
            Button::new(format!("{row_id}-remove"))
                .ghost()
                .small()
                .icon(IconName::Close)
                .tooltip(rust_i18n::t!("settings.remove_library").to_string())
                .on_click({
                    let entry = entry.clone();
                    move |_, _, cx| remove_library(&entry, cx)
                }),
        )
}

/// The "new library" button. The name is generated — the row above (the
/// current library's name field) is where it gets renamed.
fn new_library_row(controller: &Entity<LibraryController>, _cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new("new-library")
            .outline()
            .small()
            .icon(IconName::Plus)
            .label(rust_i18n::t!("settings.new_library").to_string())
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    let mut config = AppConfig::load();
                    let count = config.libraries.len() + 1;
                    if let Ok(entry) = config.add_library(&format!("Library {count}")) {
                        switch_library(&controller, entry, cx);
                    }
                }
            }),
    )
}

/// Register-free removal: the entry leaves the config and its directories go
/// with it. The library database is the only thing that disappears.
fn remove_library(entry: &trove_core::config::LibraryEntry, cx: &mut App) {
    let mut config = AppConfig::load();
    let _ = config.forget_library(&entry.slug);
    let _ = std::fs::remove_dir_all(entry.dir());
    let _ = std::fs::remove_dir_all(entry.cache_dir());
    cx.refresh_windows();
}

// ============================ watched folders ================================

/// General ▸ Watched folders: folders scanned for new files, which import
/// automatically (unfiled). The list re-reads the config on every settings
/// render, so add/remove applies immediately.
fn watch_folders_group() -> SettingGroup {
    let mut group = SettingGroup::new().title(rust_i18n::t!("settings.watch_folders").to_string());
    // The watch list belongs to the open library, not to the application: two
    // libraries can watch different folders.
    let config = LibraryConfig::load(&library_dir());
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

/// The open library's data directory — where its `library.json` lives.
fn library_dir() -> PathBuf {
    AppConfig::load().active_entry().dir()
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
                let dir = library_dir();
                let mut config = LibraryConfig::load(&dir);
                config.watch_folders_enabled = Some(!config.watch_folders_enabled());
                let _ = config.save(&dir);
                cx.refresh_windows();
            }),
    )
}

/// Toggle for the point-cloud preview's eye-dome lighting and gap filling.
///
/// The viewport re-reads the config each frame, so flipping this reaches a
/// preview that is already open.
fn point_enhance_row(_cx: &mut App) -> Div {
    let enabled = AppConfig::load().point_enhance();
    h_flex().w_full().justify_end().child(
        Button::new("point-enhance-toggle")
            .outline()
            .small()
            .label(if enabled {
                rust_i18n::t!("settings.enhance_on").to_string()
            } else {
                rust_i18n::t!("settings.enhance_off").to_string()
            })
            .on_click(|_, _, cx| {
                let mut config = AppConfig::load();
                config.point_enhance = Some(!config.point_enhance());
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

/// The open library: where Trove keeps it (read-only — the path is not the
/// user's to pick), the last switch/maintenance notice, and a button to open
/// that directory in the file manager.
fn library_current_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let dir = AppConfig::load().active_entry().dir();
    let notice = controller.read(cx).notice.clone();
    v_flex()
        .gap_1()
        .child(
            div()
                .w_full()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(dir.display().to_string()),
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
                Button::new("open-library-dir")
                    .outline()
                    .small()
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

/// Hot-switch the open library to `entry`: swap the controller's library, and
/// only record the choice when the library actually opened. Any error is
/// reported on [`LibraryController::notice`].
fn switch_library(
    controller: &Entity<LibraryController>,
    entry: trove_core::config::LibraryEntry,
    cx: &mut App,
) {
    controller.update(cx, |ctl, cx| {
        let outcome = ctl
            .swap_library(entry.dir(), entry.cache_dir())
            .and_then(|()| {
                let mut config = AppConfig::load();
                config.set_active_library(&entry.slug)
            });
        // The old watch task scanned for the previous library; restart the
        // resident watch on the new one.
        if outcome.is_ok()
            && let Some(handle) = ctl.watch_handle
        {
            let entity = cx.entity();
            crate::library::jobs::start_watch_service(&entity, handle, cx);
        }
        ctl.notice = match outcome {
            Ok(()) => None,
            Err(e) => Some(
                rust_i18n::t!("settings.library_switch_failed", error = e.to_string()).to_string(),
            ),
        };
        cx.notify();
    });
    cx.refresh_windows();
}

// ========================= preview zoom limits ==============================

/// General ▸ Preview Zoom: min/max zoom for image and 3D previews.
fn zoom_group() -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.preview_zoom").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.preview_zoom_min").to_string(),
                SettingField::number_input(
                    NumberFieldOptions {
                        min: 0.1,
                        max: 1.0,
                        step: 0.05,
                    },
                    |_cx| AppConfig::load().min_preview_zoom() as f64,
                    |value, cx| {
                        let mut config = AppConfig::load();
                        config.min_preview_zoom = Some(value as f32);
                        if config.save().is_ok() {
                            cx.refresh_windows();
                        }
                    },
                ),
            )
            .description(rust_i18n::t!("settings.preview_zoom_min_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.preview_zoom_max").to_string(),
                SettingField::number_input(
                    NumberFieldOptions {
                        min: 2.0,
                        max: 100.0,
                        step: 1.0,
                    },
                    |_cx| AppConfig::load().max_preview_zoom() as f64,
                    |value, cx| {
                        let mut config = AppConfig::load();
                        config.max_preview_zoom = Some(value as f32);
                        if config.save().is_ok() {
                            cx.refresh_windows();
                        }
                    },
                ),
            )
            .description(rust_i18n::t!("settings.preview_zoom_max_desc").to_string()),
        )
}
