//! Appearance page: light/dark mode and the theme picker per mode,
//! plus the user-themes directory row.

use super::*;

// ========================== appearance page =================================

/// Appearance ▸ Theme: which mode to use, and which named theme fills each
/// slot. Every change is applied live — the framework's `Theme` global is
/// swapped and all windows repaint.
pub(super) fn appearance_page(controller: &Entity<LibraryController>, cx: &App) -> SettingPage {
    // Own the handle: the reload row outlives this frame.
    let controller = controller.clone();

    let mode_options: Vec<(SharedString, SharedString)> = vec![
        (
            SharedString::from(Appearance::System.as_str()),
            rust_i18n::t!("settings.follow_system").into_owned().into(),
        ),
        (
            SharedString::from(Appearance::Light.as_str()),
            rust_i18n::t!("settings.appearance_light")
                .into_owned()
                .into(),
        ),
        (
            SharedString::from(Appearance::Dark.as_str()),
            rust_i18n::t!("settings.appearance_dark")
                .into_owned()
                .into(),
        ),
    ];

    SettingPage::new(rust_i18n::t!("settings.appearance").to_string())
        .icon(IconName::Palette)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.appearance").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.appearance_mode").to_string(),
                        SettingField::dropdown(
                            mode_options,
                            |_cx| SharedString::from(AppConfig::load().appearance.as_str()),
                            |value, cx| {
                                let mut config = AppConfig::load();
                                config.appearance = Appearance::parse(&value);
                                if config.save().is_ok() {
                                    crate::app::theme::apply_from_settings(None, cx);
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.appearance_mode_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.theme_light").to_string(),
                        SettingField::scrollable_dropdown(
                            theme_options(cx, ThemeMode::Light),
                            |cx| stored_theme(cx, ThemeMode::Light),
                            |value, cx| {
                                let mut config = AppConfig::load();
                                config.theme_light = Some(value.to_string());
                                if config.save().is_ok() {
                                    crate::app::theme::apply_from_settings(None, cx);
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.theme_light_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.theme_dark").to_string(),
                        SettingField::scrollable_dropdown(
                            theme_options(cx, ThemeMode::Dark),
                            |cx| stored_theme(cx, ThemeMode::Dark),
                            |value, cx| {
                                let mut config = AppConfig::load();
                                config.theme_dark = Some(value.to_string());
                                if config.save().is_ok() {
                                    crate::app::theme::apply_from_settings(None, cx);
                                }
                            },
                        ),
                    )
                    .description(rust_i18n::t!("settings.theme_dark_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.theme_dir").to_string(),
                        SettingField::render(move |_, _, cx| theme_dir_row(&controller, cx)),
                    )
                    .description(rust_i18n::t!("settings.theme_dir_desc").to_string()),
                ),
        )
}

/// Picker options for one mode: a theme's name is both its value and label.
fn theme_options(cx: &App, mode: ThemeMode) -> Vec<(SharedString, SharedString)> {
    crate::app::theme::theme_names(cx, mode)
        .into_iter()
        .map(|name| (name.clone(), name))
        .collect()
}

/// The stored name for `mode`, falling back to the framework default when
/// nothing is set or when the stored theme no longer exists (a renamed or
/// dropped file would otherwise leave the picker blank).
fn stored_theme(cx: &App, mode: ThemeMode) -> SharedString {
    let config = AppConfig::load();
    let (stored, fallback) = match mode {
        ThemeMode::Light => (config.theme_light, crate::app::theme::DEFAULT_LIGHT),
        ThemeMode::Dark => (config.theme_dark, crate::app::theme::DEFAULT_DARK),
    };
    let known = crate::app::theme::theme_names(cx, mode);
    match stored {
        Some(name) if known.iter().any(|candidate| candidate == &name) => name.into(),
        _ => fallback.into(),
    }
}

/// Custom-themes row: the folder path, a reveal button and a reload button
/// that re-scans the folder and re-applies the appearance.
fn theme_dir_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let dir = crate::app::theme::themes_dir().unwrap_or_else(std::env::temp_dir);
    let path = dir.display().to_string();
    let controller = controller.clone();
    let reveal = dir.clone();

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
            Button::new("open-theme-dir")
                .ghost()
                .xsmall()
                .icon(IconName::Folder)
                .tooltip(rust_i18n::t!("settings.theme_dir_open").to_string())
                .on_click(move |_, _, _cx| {
                    // Created on demand so the folder exists to drop files in.
                    let _ = std::fs::create_dir_all(&reveal);
                    crate::panels::common::reveal_path(&reveal);
                }),
        )
        .child(
            Button::new("reload-themes")
                .outline()
                .small()
                .label(rust_i18n::t!("settings.theme_dir_reload").to_string())
                .on_click(move |_, _, cx| {
                    let loaded = crate::app::theme::register_user_themes(cx);
                    crate::app::theme::apply_from_settings(None, cx);
                    controller.update(cx, |ctl, cx| {
                        ctl.notice = Some(
                            rust_i18n::t!("settings.theme_dir_reloaded", count = loaded)
                                .to_string(),
                        );
                        cx.notify();
                    });
                }),
        )
}
