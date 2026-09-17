//! Appearance page: the light/dark mode, the named theme filling each slot,
//! and the folder the user drops their own themes into.

use super::*;

// ========================== appearance page =================================

/// Appearance ▸ three groups, in the order they matter: what decides light or
/// dark, which named theme fills each side, and where to put themes that are
/// not built in. Every change applies live — the framework's `Theme` global is
/// swapped and all windows repaint.
pub(super) fn appearance_page(controller: &Entity<LibraryController>, cx: &App) -> SettingPage {
    // Own the handle: the reload row outlives this frame.
    let controller = controller.clone();

    SettingPage::new(rust_i18n::t!("settings.appearance").to_string())
        .icon(IconName::Palette)
        .resettable(false)
        .group(mode_group())
        .group(themes_group(cx))
        .group(custom_themes_group(controller))
}

/// Basics: follow the system, or pin light / dark.
fn mode_group() -> SettingGroup {
    let options: Vec<(SharedString, SharedString)> = vec![
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

    SettingGroup::new()
        .title(rust_i18n::t!("settings.appearance_basics").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.appearance_mode").to_string(),
                SettingField::dropdown(
                    options,
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
}

/// Theme: one named theme per side. Both are stored even when the mode pins
/// one of them, so switching modes later keeps the user's choice.
fn themes_group(cx: &App) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.themes").to_string())
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
}

/// Custom themes: the folder the user drops `*.json` files into, and the one
/// button that opens it.
fn custom_themes_group(controller: Entity<LibraryController>) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.custom_themes").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.theme_dir").to_string(),
                SettingField::render(move |_, _, _| theme_dir_row(&controller)),
            )
            .description(rust_i18n::t!("settings.theme_dir_desc").to_string()),
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

/// Custom-themes row: one button that opens the folder, and nothing else —
/// the path itself is not the user's to read or edit.
///
/// Opening also re-scans. The reason to open that folder is to drop a `.json`
/// into it, and a second button that only reloads would be a step nobody
/// expects to take; the rescan happens either way, so a theme added while the
/// window was open is in the picker the moment the user comes back.
fn theme_dir_row(controller: &Entity<LibraryController>) -> Div {
    let dir = crate::app::theme::themes_dir();
    let controller = controller.clone();

    h_flex().w_full().justify_end().child(
        Button::new("open-theme-dir")
            .outline()
            .small()
            .icon(IconName::Folder)
            .label(rust_i18n::t!("settings.theme_dir_open").to_string())
            .on_click(move |_, _, cx| {
                // Created on demand, so there is always a folder to open
                // and to drop files into.
                let _ = std::fs::create_dir_all(&dir);
                let loaded = crate::app::theme::register_user_themes(cx);
                crate::app::theme::apply_from_settings(None, cx);
                if loaded > 0 {
                    controller.update(cx, |ctl, cx| {
                        ctl.notice = Some(
                            rust_i18n::t!("settings.theme_dir_reloaded", count = loaded)
                                .to_string(),
                        );
                        cx.notify();
                    });
                }
                crate::panels::common::reveal_path(&dir);
            }),
    )
}
