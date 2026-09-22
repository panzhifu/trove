//! Model page: how the 3D preview draws a model.
//!
//! These decide what a point cloud or a mesh looks like *before* a file is
//! opened, and how far the viewport will zoom. The same look switches sit on
//! the viewport's own toolbar, where they are one click away while a model is
//! on screen; both write the same config, and the viewport re-reads it every
//! frame, so flipping a switch here reaches a preview that is already open.

use super::*;
use gpui_kit::component::setting::NumberFieldOptions;

// =============================== model page ==================================

/// Model ▸ the point cloud's look and the zoom range shared by image and model
/// previews.
pub(super) fn model_page() -> SettingPage {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    SettingPage::new(t("settings.model"))
        .icon(IconName::Frame)
        .resettable(false)
        .group(
            SettingGroup::new()
                .title(t("settings.model_points"))
                .item(
                    SettingItem::new(
                        t("settings.point_enhance"),
                        config_switch(AppConfig::point_enhance, |config, on| {
                            config.point_enhance = Some(on);
                        }),
                    )
                    .description(t("settings.point_enhance_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.height_color"),
                        config_switch(AppConfig::height_color, |config, on| {
                            config.height_color = Some(on);
                        }),
                    )
                    .description(t("settings.height_color_desc")),
                ),
        )
        .group(zoom_group())
}

// ============================== look switches ================================

/// A boolean setting, stored as `Some(value)` on one `AppConfig` field.
///
/// The framework's switch, the same control the About page uses for automatic
/// updates: flipped, not labelled — the row's title already says what it is,
/// so the control only has to say whether it is on.
fn config_switch(
    read: fn(&AppConfig) -> bool,
    write: impl Fn(&mut AppConfig, bool) + 'static,
) -> SettingField<bool> {
    SettingField::switch(
        move |_cx| read(&AppConfig::load()),
        move |value, cx| {
            let mut config = AppConfig::load();
            write(&mut config, value);
            let _ = config.save();
            cx.refresh_windows();
        },
    )
}

// ========================= preview zoom limits ==============================

/// Model ▸ Preview zoom: how far the viewport will zoom in and out.
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
