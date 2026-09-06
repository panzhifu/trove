//! Inspector: details of the selected asset plus its tags.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, Sizable};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;

use trove_core::store::{assets, tags};

use crate::state::LibraryController;

use super::common::{
    display_name, hex_to_rgb, human_bytes, kind_icon, observe_controller, separator_label,
};

// ==================== Inspector: details + tags ==============================

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    tag_input: Entity<InputState>,
}

impl InspectorPanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add tag + Enter"));
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            tag_input,
        };
        observe_controller(cx, &this.controller);

        let input = this.tag_input.clone();
        cx.subscribe_in(&input, window, |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.add_tag_from_input(cx);
            }
        })
        .detach();
        this
    }

    fn add_tag_from_input(&mut self, cx: &mut Context<Self>) {
        let name: String = self.tag_input.read(cx).value().to_string();
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let controller = self.controller.clone();
        let Some(asset_id) = controller.read(cx).primary() else {
            return;
        };
        controller.update(cx, |ctl, cx| {
            let conn = ctl.library.store().conn();
            if let Ok(tag) = tags::ensure_named(conn, &name) {
                let _ = tags::add_to_asset(conn, asset_id, tag.id);
            }
            ctl.generation += 1;
            cx.notify();
        });
    }
}

impl BasePanel for InspectorPanel {
    fn panel_name(&self) -> &'static str {
        "InspectorPanel"
    }
    fn closable(&self, _: &App) -> bool {
        false
    }
}
impl DockPanel for InspectorPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Inspector"
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }
}
impl EventEmitter<PanelEvent> for InspectorPanel {}
impl Focusable for InspectorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ctl = self.controller.read(cx);
        let Some(asset_id) = ctl.primary() else {
            return v_flex()
                .p_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing selected."),
                )
                .into_any_element();
        };
        let conn = ctl.library.store().conn();
        let Some(asset) = assets::get(conn, asset_id).ok().flatten() else {
            return v_flex()
                .p_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Asset no longer exists."),
                )
                .into_any_element();
        };

        let asset_tags = tags::for_asset(conn, asset_id).unwrap_or_default();
        let dims = asset
            .width
            .zip(asset.height)
            .map(|(w, h)| format!("{w} × {h}"))
            .unwrap_or_else(|| "—".into());
        let hash = asset
            .sha256
            .as_deref()
            .map(|s| s.chars().take(12).collect())
            .unwrap_or_else(|| "—".into());

        // Small thumbnail preview at the top of the inspector.
        let thumb_path = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(ctl.library.root(), sha))
            .filter(|p| p.is_file());
        let preview: AnyElement = match thumb_path {
            Some(path) => img(path)
                .max_w(px(220.))
                .max_h(px(160.))
                .object_fit(gpui_kit::ObjectFit::Contain)
                .rounded(cx.theme().radius)
                .into_any_element(),
            None => v_flex()
                .w(px(120.))
                .h(px(120.))
                .items_center()
                .justify_center()
                .bg(cx.theme().secondary)
                .rounded_full()
                .child(Icon::new(kind_icon(asset.kind)).size_10())
                .into_any_element(),
        };

        // The mined color palette (`dominant_color` + `dominant_colors`).
        let swatches: Vec<(u32, String)> = asset
            .extra
            .get("dominant_colors")
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            })
            .or_else(|| {
                asset
                    .extra
                    .get("dominant_color")
                    .and_then(|v| v.as_str())
                    .map(|s| vec![s.to_string()])
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| hex_to_rgb(&s).map(|rgb| (rgb, s)))
            .collect();

        v_flex()
            .p_3()
            .gap_2()
            .w_full()
            .child(
                div()
                    .flex()
                    .w_full()
                    .justify_center()
                    .child(preview),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(cx.theme().foreground)
                    .w_full()
                    .truncate()
                    .child(display_name(&asset)),
            )
            .child(separator_label(cx, "Tags"))
            .child(
                v_flex()
                    .gap_1()
                    .children(asset_tags.iter().map(|tag| {
                        let id = tag.id;
                        let controller = self.controller.clone();
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().secondary)
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(tag.name.clone()),
                            )
                            .child(
                                Button::new(format!("untag-{id}"))
                                    .xsmall()
                                    .ghost()
                                    .label("×")
                                    .on_click(move |_, _, cx| {
                                        controller.update(cx, move |ctl, cx| {
                                            let conn = ctl.library.store().conn();
                                            let _ = tags::remove_from_asset(conn, asset_id, id);
                                            ctl.generation += 1;
                                            cx.notify();
                                        });
                                    }),
                            )
                            .into_any_element()
                    })),
            )
            .child(Input::new(&self.tag_input).small())
            .when(!swatches.is_empty(), |this| {
                this.child(separator_label(cx, "Colors")).child(
                    h_flex()
                        .gap_2()
                        .px_1()
                        .children(swatches.iter().map(|(rgb, _hex)| {
                            let bg = gpui_kit::rgb(*rgb);
                            div()
                                .size_8()
                                .rounded_full()
                                .bg(bg)
                                .border_1()
                                .border_color(cx.theme().border)
                        })),
                )
            })
            .child(separator_label(cx, "Properties"))
            .child(property_row(cx, "Kind", format!("{:?}", asset.kind)))
            .child(property_row(cx, "Type", asset.mime))
            .child(property_row(cx, "Size", human_bytes(asset.size_bytes)))
            .child(property_row(cx, "Dimensions", dims))
            .child(property_row(
                cx,
                "Added",
                asset.created_at.format("%Y-%m-%d %H:%M").to_string(),
            ))
            .child(property_row(cx, "SHA-256", hash))
            .into_any_element()
    }
}

fn property_row(cx: &Context<impl Render>, name: &'static str, value: String) -> Div {
    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_right()
                .text_xs()
                .text_color(cx.theme().foreground)
                .truncate()
                .child(value),
        )
}
