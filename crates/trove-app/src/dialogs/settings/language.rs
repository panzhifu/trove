//! Language page: UI locale selection (system-follow sentinel included).

use super::*;

/// Sentinel value for the "follow the system language" choice, which the
/// config stores as `None`.
const SYSTEM_LANGUAGE: &str = "system";

// ============================ language page ==================================

/// Language ▸ Interface: a dropdown of supported catalogs plus the
/// "follow system" sentinel. Switching applies the locale immediately.
pub(super) fn language_page(controller: &Entity<LibraryController>) -> SettingPage {
    // Own the handle: the dropdown callback is an `Fn` that outlives this
    // frame, so it must capture a clone instead of borrowing the argument.
    let controller = controller.clone();
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
                            crate::apply_menus(cx);
                        },
                    ),
                )
                .description(rust_i18n::t!("settings.language_desc").to_string()),
            ),
        )
}
