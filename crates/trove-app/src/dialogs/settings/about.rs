//! About page: the running version, whether a newer release exists, and the
//! interface language.
//!
//! These belong together: they are the two things a user comes to Settings to
//! find out about the application itself rather than about their library.

use super::*;
use crate::app::settings_write;
use crate::components::controls::muted_label;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::WindowExt as _;
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
                            settings_write::note(config.save(), "app config");
                            cx.refresh_windows();
                        },
                    ),
                )),
        )
        .group(license_group(&controller))
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
                                settings_write::note(
                                    config.skip_version(&version),
                                    "skipped release",
                                );
                                cx.refresh_windows();
                            }
                        }),
                ),
        );
    }
    column
}


// ================================ license ====================================

/// About ▸ License: the offline activation state, and the way into it.
///
/// The state is re-read (and re-verified) per render like every other row on
/// this page; an Ed25519 check on 85 bytes does not need caching. An expired
/// key — a build newer than its update coverage — keeps the "enter a key"
/// door open: renewal is the same dialog as first activation, and the
/// covered builds keep working either way. Nothing here can lock the
/// library.
fn license_group(controller: &Entity<LibraryController>) -> SettingGroup {
    let status = crate::license::current();
    let mut group = SettingGroup::new()
        .title(rust_i18n::t!("settings.license").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.license_state").to_string(),
                SettingField::render(|_, _, cx| license_actions(cx)),
            )
            .description(license_summary()),
        );

    // The free tier's meter, next to the decision that clears it. Rendered
    // live off the open library, like every other row on this page; an
    // activated install has no cap, so the row would only be noise.
    if !matches!(status, crate::license::LicenseStatus::Active(_)) {
        let controller = controller.clone();
        group = group.item(
            SettingItem::new(
                rust_i18n::t!("settings.license_usage").to_string(),
                SettingField::render(move |_, _, cx| {
                    let count = controller.read(cx).library.asset_count();
                    muted_label(
                        rust_i18n::t!(
                            "settings.license_usage_value",
                            count = count,
                            cap = crate::license::FREE_ASSET_CAP
                        )
                        .to_string(),
                        cx,
                    )
                }),
            )
            .description(rust_i18n::t!("settings.license_usage_desc").to_string()),
        );
    }
    group
}

/// One line of prose describing the current state — the SettingItem's
/// description slot, so the buttons stay where every other row keeps them.
fn license_summary() -> String {
    match crate::license::current() {
        crate::license::LicenseStatus::Active(info) => {
            let edition = if info.edition_name() == "standard" {
                rust_i18n::t!("settings.license_edition_standard").to_string()
            } else {
                info.edition_name()
            };
            let coverage = info
                .updates_until
                .map(|until| {
                    rust_i18n::t!(
                        "settings.license_until",
                        until = until.format("%Y-%m-%d").to_string()
                    )
                    .to_string()
                })
                .unwrap_or_else(|| rust_i18n::t!("settings.license_perpetual").to_string());
            rust_i18n::t!(
                "settings.license_active_summary",
                edition = edition,
                serial = info.serial,
                licensee = info.licensee_hex(),
                coverage = coverage,
            )
            .to_string()
        }
        crate::license::LicenseStatus::Expired { until } => rust_i18n::t!(
            "settings.license_expired_summary",
            until = until.format("%Y-%m-%d").to_string()
        )
        .to_string(),
        crate::license::LicenseStatus::NotActivated => {
            rust_i18n::t!("settings.license_inactive_summary").to_string()
        }
    }
}

/// The row's action side: "enter a key" whenever the app is not — or no
/// longer — active, "deactivate" when it is.
fn license_actions(_cx: &mut App) -> Div {
    let row = h_flex().w_full().items_center().justify_end().gap_2();
    match crate::license::current() {
        crate::license::LicenseStatus::Active(_) => row.child(
            Button::new("license-deactivate")
                .ghost()
                .small()
                .label(rust_i18n::t!("settings.license_deactivate").to_string())
                .on_click(|_, window, cx| {
                    crate::license::deactivate();
                    window.push_notification(
                        Notification::info(
                            rust_i18n::t!("settings.license_deactivated").to_string(),
                        ),
                        cx,
                    );
                    cx.refresh_windows();
                }),
        ),
        crate::license::LicenseStatus::NotActivated
        | crate::license::LicenseStatus::Expired { .. } => {
            row.child(
                Button::new("license-how-to-get")
                    .ghost()
                    .small()
                    .label(rust_i18n::t!("settings.license_how_to_get").to_string())
                    .on_click(|_, _, _| {
                        let _ = trove_core::services::open_external::open_url(
                            crate::license::PURCHASE_URL,
                        );
                    }),
            )
            .child(
                Button::new("license-activate")
                    .outline()
                    .small()
                    .label(rust_i18n::t!("settings.license_activate").to_string())
                    .on_click(|_, window, cx| open_activation_dialog(window, cx)),
            )
        }
    }
}

/// The activation dialog: paste the key from the purchase email, and the OK
/// button verifies before it closes — a bad key keeps the dialog up with an
/// explanation, so nothing retyped on a second try.
fn open_activation_dialog(window: &mut Window, cx: &mut App) {
    // The input entity lives *outside* the builder: the dialog's content
    // closure re-runs per frame, and an entity created inside would be
    // rebuilt under every keystroke — no focus, no typing (the same shape
    // prompt_import_url and the tag dialogs already use).
    let key_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(rust_i18n::t!("settings.license_key_placeholder").to_string())
    });
    window.open_dialog(cx, move |dialog, _, cx| {
        let key_input = key_input.clone();
        dialog
            .title(rust_i18n::t!("settings.license_dialog_title").to_string())
            .width(px(520.))
            .close_button(false)
            .child(
                v_flex()
                    .gap_2()
                    .p_1()
                    .child(muted_label(
                        rust_i18n::t!("settings.license_dialog_hint").to_string(),
                        cx,
                    ))
                    .child(Input::new(&key_input).small().appearance(true)),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("settings.license_ok").to_string())
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let key = key_input.read(cx).value().trim().to_string();
                match crate::license::activate(&key) {
                    Ok(_) => {
                        window.push_notification(
                            Notification::success(
                                rust_i18n::t!("settings.license_activated").to_string(),
                            ),
                            cx,
                        );
                        cx.refresh_windows();
                        true
                    }
                    Err(trove_core::license::LicenseError::UpdatesExpired { until, .. }) => {
                        window.push_notification(
                            Notification::warning(
                                rust_i18n::t!(
                                    "settings.license_build_expired",
                                    until = until.format("%Y-%m-%d").to_string()
                                )
                                .to_string(),
                            ),
                            cx,
                        );
                        false
                    }
                    Err(_) => {
                        window.push_notification(
                            Notification::warning(
                                rust_i18n::t!("settings.license_invalid").to_string(),
                            ),
                            cx,
                        );
                        false
                    }
                }
            })
    });
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
