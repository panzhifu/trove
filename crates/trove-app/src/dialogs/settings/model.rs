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

/// Model ▸ the point cloud's look, the scene's reference axes, and the zoom
/// range shared by image and model previews.
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
                        SettingField::render(|_, _, cx| point_enhance_row(cx)),
                    )
                    .description(t("settings.point_enhance_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.height_color"),
                        SettingField::render(|_, _, cx| height_color_row(cx)),
                    )
                    .description(t("settings.height_color_desc")),
                ),
        )
        .group(
            SettingGroup::new()
                .title(t("settings.model_axes"))
                .item(
                    SettingItem::new(
                        t("settings.scene_axes"),
                        SettingField::render(|_, _, cx| scene_axes_row(cx)),
                    )
                    .description(t("settings.scene_axes_desc")),
                )
                .item(
                    SettingItem::new(
                        t("settings.corner_axis"),
                        SettingField::render(|_, _, cx| corner_axis_row(cx)),
                    )
                    .description(t("settings.corner_axis_desc")),
                ),
        )
        .group(zoom_group())
}

// ============================== look switches ================================

/// A one-button switch: the label states what the setting currently is, and
/// clicking flips it. Every model toggle is one of these.
fn switch_row(
    id: &'static str,
    enabled: bool,
    labels: (String, String),
    flip: impl Fn(&mut App) + 'static,
    _cx: &mut App,
) -> Div {
    let (on, off) = labels;
    h_flex().w_full().justify_end().child(
        Button::new(id)
            .outline()
            .small()
            .label(if enabled { on } else { off })
            .on_click(move |_, _, cx| flip(cx)),
    )
}

/// `on` / `off` labels for a boolean setting.
fn on_off() -> (String, String) {
    (
        rust_i18n::t!("settings.toggle_on").to_string(),
        rust_i18n::t!("settings.toggle_off").to_string(),
    )
}

/// Eye-dome lighting and gap filling on a point-cloud preview.
fn point_enhance_row(cx: &mut App) -> Div {
    let enabled = AppConfig::load().point_enhance();
    switch_row(
        "point-enhance-toggle",
        enabled,
        on_off(),
        |cx| {
            let mut config = AppConfig::load();
            config.point_enhance = Some(!config.point_enhance());
            let _ = config.save();
            cx.refresh_windows();
        },
        cx,
    )
}

/// Paint a point cloud by height instead of by its own colours.
fn height_color_row(cx: &mut App) -> Div {
    let enabled = AppConfig::load().height_color();
    switch_row(
        "height-color-toggle",
        enabled,
        on_off(),
        |cx| {
            let mut config = AppConfig::load();
            config.height_color = Some(!config.height_color());
            let _ = config.save();
            cx.refresh_windows();
        },
        cx,
    )
}

/// Draw the scene's X/Y/Z axes on the model's bounding box.
fn scene_axes_row(cx: &mut App) -> Div {
    let enabled = AppConfig::load().scene_axes();
    switch_row(
        "scene-axes-toggle",
        enabled,
        on_off(),
        |cx| {
            let mut config = AppConfig::load();
            config.scene_axes = Some(!config.scene_axes());
            let _ = config.save();
            cx.refresh_windows();
        },
        cx,
    )
}

/// Draw the corner trihedron — the small axis indicator pinned to the
/// viewport's bottom-right corner.
fn corner_axis_row(cx: &mut App) -> Div {
    let enabled = AppConfig::load().corner_axis();
    switch_row(
        "corner-axis-toggle",
        enabled,
        on_off(),
        |cx| {
            let mut config = AppConfig::load();
            config.corner_axis = Some(!config.corner_axis());
            let _ = config.save();
            cx.refresh_windows();
        },
        cx,
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
