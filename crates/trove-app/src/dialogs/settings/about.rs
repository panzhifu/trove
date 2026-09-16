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
                .item(SettingItem::new(
                    rust_i18n::t!("settings.update_check").to_string(),
                    SettingField::switch(
                        |_cx| AppConfig::load().update_check(),
                        |enabled, cx| {
                            let mut config = AppConfig::load();
                            config.update_check = Some(enabled);
                            if enabled {
                                // A missing timestamp reads as "never
                                // checked", which is due at once — so
                                // switching back on checks on the next
                                // launch rather than in 24 hours.
                                config.last_update_check = None;
                            }
                            let _ = config.save();
                            cx.refresh_windows();
                        },
                    ),
                )),
        )
        .group(language_group(controller))
}

/// The version line: the build this window is running, and — at the right
/// edge, where the row's action lives — the button that runs a check now.
///
/// The state is re-read here rather than captured, so the row is live: a
/// check started from the button repaints into this line when it lands.
///
/// Only states the user has to be told about get a line: a check in flight, a
/// newer release, a failure. "Not checked yet" and "up to date" stay silent —
/// the version number beside them is the answer, and repeating it read as
/// noise.
fn version_row(cx: &mut App) -> Div {
    let version = env!("CARGO_PKG_VERSION");
    let state = update::state();
    let note = match &state {
        UpdateState::Unknown | UpdateState::Current { .. } => None,
        UpdateState::Checking => Some((
            rust_i18n::t!("settings.update_checking").to_string(),
            cx.theme().muted_foreground,
        )),
        UpdateState::Available { version, .. } => Some((
            rust_i18n::t!("settings.update_available", version = version).to_string(),
            cx.theme().info,
        )),
        UpdateState::Failed { error } => Some((
            rust_i18n::t!("settings.update_failed", error = error).to_string(),
            cx.theme().warning,
        )),
    };

    // The row's own action sits at the right edge, level with the number and
    // whatever the last check said.
    let mut row = h_flex()
        .w_full()
        .items_center()
        .justify_end()
        .gap_2()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(version.to_string()),
        );
    if let Some((text, tone)) = note {
        row = row.child(div().text_xs().text_color(tone).child(text));
    }
    row = row.child(
        Button::new("update-check-now")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.update_now").to_string())
            .on_click(|_, _, cx| crate::app::run_update_check(cx)),
    );

    let mut column = v_flex().gap_1().w_full().child(row);

    // A newer release is on the table: offer its page, and the right to stop
    // hearing about this particular version. They get their own line so the
    // check button stays where the eye expects it.
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
