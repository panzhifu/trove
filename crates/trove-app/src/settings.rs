//! Settings dialog: sidebar pages built on gpui-kit's `Settings` widget
//! (the same scaffold OpenLogi uses for its settings window — page
//! navigation, groups and data-bound fields all come from the component).
//!
//! Pages are assembled inside the dialog's content closure so a live
//! language switch re-localizes every title on the next refresh.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, IconName, Sizable, WindowExt};
use gpui_kit::*;

use crate::i18n::SUPPORTED;
use trove_core::config::AppConfig;

/// Sentinel value for the "follow the system language" choice, which the
/// config stores as `None`.
const SYSTEM_LANGUAGE: &str = "system";

pub struct SettingsDialog;

impl SettingsDialog {
    /// Open the settings dialog.
    pub fn open(window: &mut Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, _| {
            // Pages are rebuilt on every dialog render, so titles follow the
            // active locale after `refresh_windows`.
            let settings = Settings::new("trove-settings")
                .sidebar_width(px(170.))
                .page(general_page())
                .page(language_page());

            dialog
                .title(rust_i18n::t!("settings.title").to_string())
                .width(px(760.))
                .child(v_flex().w_full().h(px(480.)).child(settings))
        });
    }
}

/// General ▸ Library: where the library lives, with a folder picker.
fn general_page() -> SettingPage {
    SettingPage::new(rust_i18n::t!("settings.general").to_string())
        .icon(IconName::Settings)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.library").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.library_location").to_string(),
                        SettingField::render(|_, _, cx| library_location_row(cx)),
                    )
                    .description(rust_i18n::t!("settings.library_location_desc").to_string()),
                ),
        )
}

/// The library-location row: the resolved path plus a Browse button that
/// persists the choice and refreshes so the new path shows immediately.
fn library_location_row(cx: &mut App) -> Div {
    let current = AppConfig::load().resolved_library_path();
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
        .child(
            Button::new("browse-library")
                .outline()
                .small()
                .label(rust_i18n::t!("settings.browse").to_string())
                .on_click(|_, _, cx| {
                    let rx = cx.prompt_for_paths(PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some(rust_i18n::t!("settings.select_folder").into_owned().into()),
                    });
                    cx.spawn(async move |cx| {
                        if let Ok(Ok(Some(paths))) = rx.await
                            && let Some(path) = paths.first()
                        {
                            let path: PathBuf = path.clone();
                            let _ = cx.update(|cx| {
                                let result = AppConfig::load().set_library_path(path);
                                if let Err(e) = result {
                                    eprintln!("save library path: {e}");
                                }
                                cx.refresh_windows();
                            });
                        }
                    })
                    .detach();
                }),
        )
}

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
