//! About page: the running version, whether a newer release exists, and the
//! interface language.
//!
//! These belong together: they are the two things a user comes to Settings to
//! find out about the application itself rather than about their library.

use super::*;
use trove_core::services::update::{self, UpdateState};

/// Sentinel value for the "follow the system language" choice, which the
/// config stores as `None`.
const SYSTEM_LANGUAGE: &str = "system";

// ============================== about page ===================================

/// About ▸ Version and language.
pub(super) fn about_page(controller: &Entity<LibraryController>) -> SettingPage {
    // Own the handle: the language dropdown is an `Fn` that outlives this
    // frame, so it must capture a clone instead of borrowing the argument.
    let controller = controller.clone();
    SettingPage::new(rust_i18n::t!("settings.about").to_string())
        .icon(IconName::Info)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.about").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.about_version").to_string(),
                        SettingField::render(|_, _, cx| version_row(cx)),
                    )
                    .description(rust_i18n::t!("settings.about_version_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.update_check").to_string(),
                        SettingField::render(|_, _, cx| update_toggle_row(cx)),
                    )
                    .description(rust_i18n::t!("settings.update_check_desc").to_string()),
                )
                .item(SettingItem::new(
                    rust_i18n::t!("settings.update_now").to_string(),
                    SettingField::render(|_, _, cx| update_now_row(cx)),
                )),
        )
        .group(language_group(controller))
}

/// The version line: the build this window is running, with the last check's
/// verdict beside it and the two actions a released update offers.
///
/// The state is re-read here rather than captured, so the row is live: a
/// check started from the button below repaints into this line when it lands.
fn version_row(cx: &mut App) -> Div {
    let version = env!("CARGO_PKG_VERSION");
    let state = update::state();
    let (status, tone) = match &state {
        UpdateState::Unknown => (rust_i18n::t!("settings.update_idle").to_string(), None),
        UpdateState::Checking => (rust_i18n::t!("settings.update_checking").to_string(), None),
        UpdateState::Current { version } => (
            rust_i18n::t!("settings.update_current", version = version).to_string(),
            None,
        ),
        UpdateState::Available { version, .. } => (
            rust_i18n::t!("settings.update_available", version = version).to_string(),
            Some(cx.theme().info),
        ),
        UpdateState::Failed { error } => (
            rust_i18n::t!("settings.update_failed", error = error).to_string(),
            Some(cx.theme().warning),
        ),
    };

    let mut column = v_flex().gap_1().w_full().child(
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(format!("v{version}")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(tone.unwrap_or(cx.theme().muted_foreground))
                    .child(status),
            ),
    );

    // A newer release is on the table: offer the page, and the right to stop
    // hearing about this particular version.
    if let UpdateState::Available { version, url } = state {
        column = column.child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("update-open-page")
                        .outline()
                        .small()
                        .label(rust_i18n::t!("settings.update_open_page").to_string())
                        .on_click({
                            let url = url.clone();
                            move |_, _, _| {
                                let _ = trove_core::services::open_external::open_url(&url);
                            }
                        }),
                )
                .child(
                    Button::new("update-skip")
                        .ghost()
                        .small()
                        .label(rust_i18n::t!("settings.update_skip").to_string())
                        .on_click({
                            let version = version.clone();
                            move |_, _, cx| {
                                let mut config = AppConfig::load();
                                let _ = config.skip_version(&version);
                                cx.refresh_windows();
                            }
                        }),
                ),
        );
    }
    column
}

/// The check-on-launch switch. Turning it back on clears the timestamp, so
/// the next launch checks immediately instead of tomorrow.
fn update_toggle_row(_cx: &mut App) -> Div {
    let enabled = AppConfig::load().update_check();
    h_flex().w_full().justify_end().child(
        Button::new("update-check-toggle")
            .outline()
            .small()
            .label(if enabled {
                rust_i18n::t!("settings.update_on").to_string()
            } else {
                rust_i18n::t!("settings.update_off").to_string()
            })
            .on_click(|_, _, cx| {
                let mut config = AppConfig::load();
                let enabled = !config.update_check();
                config.update_check = Some(enabled);
                if enabled {
                    // A missing timestamp reads as "never checked", which is
                    // due at once — so switching back on checks on the next
                    // launch rather than in 24 hours.
                    config.last_update_check = None;
                }
                let _ = config.save();
                cx.refresh_windows();
            }),
    )
}

/// Ask GitHub right now, without waiting for the next launch.
fn update_now_row(_cx: &mut App) -> Div {
    h_flex().w_full().justify_end().child(
        Button::new("update-check-now")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.update_check_now").to_string())
            .on_click(|_, _, cx| crate::app::run_update_check(cx)),
    )
}

// ================================ language ===================================

/// About ▸ Interface language: a dropdown of the supported catalogs plus the
/// "follow system" sentinel. Switching applies the locale immediately.
fn language_group(controller: Entity<LibraryController>) -> SettingGroup {
    let mut options: Vec<(SharedString, SharedString)> = vec![(
        SharedString::from(SYSTEM_LANGUAGE),
        rust_i18n::t!("settings.follow_system").into_owned().into(),
    )];
    options.extend(
        SUPPORTED
            .iter()
            .map(|(code, name)| (SharedString::from(*code), SharedString::from(*name))),
    );

    SettingGroup::new()
        .title(rust_i18n::t!("settings.language").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.language").to_string(),
                SettingField::dropdown(
                    options,
                    |_cx| {
                        let lang = AppConfig::load().language;
                        SharedString::from(lang.unwrap_or_else(|| SYSTEM_LANGUAGE.into()))
                    },
                    move |value, cx| {
                        let language = (&*value != SYSTEM_LANGUAGE).then(|| value.to_string());
                        // The locale switch applies regardless; persist
                        // failures (rare: full disk, ...) surface here.
                        if let Err(e) = crate::app::i18n::set_language(language) {
                            controller.update(cx, |ctl, cx| {
                                ctl.notice = Some(
                                    rust_i18n::t!(
                                        "settings.language_save_failed",
                                        error = e.to_string()
                                    )
                                    .to_string(),
                                );
                                cx.notify();
                            });
                        }
                        // The locale is a process global: repaint every open
                        // window and rebuild the (already localized) menus.
                        cx.refresh_windows();
                        crate::app::title_bar::apply_menus(cx);
                    },
                ),
            )
            .description(rust_i18n::t!("settings.language_desc").to_string()),
        )
}
