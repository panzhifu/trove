//! The workspace panel's title bar: the title and suffix the dock skin
//! renders in the panel's tab strip.
//!
//! The suffix has two modes — grid (item count, zoom, view/sort/favorites,
//! search) and preview (asset name + close button) — switched on whether
//! a main-area preview is open.

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{Panel as DockPanel, PanelControl};
use gpui_kit::component::slider::Slider;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;

use crate::components::preview::ModelViewport;
use crate::panels::WorkspacePanel;
use crate::panels::workspace::MainPreview;
use crate::panels::workspace::title_controls;

impl DockPanel for WorkspacePanel {
    /// Title text: follows the browsed view (collection name, smart
    /// collection, trash, or the all-assets fallback). The interactive
    /// buttons live in [`title_suffix`] which renders outside the title's
    /// clipping container, so they stay visible.
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A preview open: the tab names what is on screen, the way a document
        // window does, and the suffix carries that preview's tools. Back in
        // grid mode it goes back to naming the browsed view.
        let label = match &self.preview {
            Some(MainPreview::Asset(preview)) => preview.read(cx).asset_name().to_string(),
            Some(MainPreview::Model(viewport)) => viewport.read(cx).name().to_string(),
            None => self.title_label(cx),
        };
        div()
            .text_sm()
            .font_weight(FontWeight::BOLD)
            .text_color(cx.theme().foreground)
            .child(label)
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    /// The panel title bar. In grid mode it carries the item count, zoom,
    /// view/sort/favourites and search; while a main-area preview is open it
    /// carries that preview's own tools instead — the asset name and close
    /// button for a still or a video, the geometry stats and camera controls
    /// for a model. Opening a preview switches the bar with it.
    fn title_suffix(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // A preview open: the title bar carries *that* preview's tools instead
        // of the grid's, so the content area is nothing but the picture. Which
        // set it is follows the preview, so opening one switches the bar.
        match &self.preview {
            Some(MainPreview::Asset(_)) => {
                return Some(preview_toolbar(cx).into_any_element());
            }
            Some(MainPreview::Model(viewport)) => {
                return Some(model_toolbar(viewport, cx));
            }
            None => {}
        }

        let ctl = self.controller.read(cx);
        let in_trash = ctl.showing_trash;
        let in_recent = ctl.showing_recent;
        let loaded = ctl.grid_loaded.min(self.last_total);
        let total = self.last_total;
        let controller = self.controller.clone();
        let slider_value = self.zoom_slider.read(cx).value().start();
        let zoom_label = format!("{:.0}%", (slider_value * 100.0).round());
        let count_label = if loaded < total {
            rust_i18n::t!("workspace.scroll_hint", loaded = loaded, total = total).to_string()
        } else if total == 1 {
            rust_i18n::t!("workspace.item_one").to_string()
        } else {
            rust_i18n::t!("workspace.items_many", count = total).to_string()
        };
        let mut row = h_flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(count_label),
            )
            .child(
                div()
                    .id("grid-zoom")
                    .flex_none()
                    .w(px(96.0))
                    .px_1()
                    .child(Slider::new(&self.zoom_slider)),
            )
            .child(
                div()
                    .text_xs()
                    .w(px(34.0))
                    .text_color(cx.theme().muted_foreground)
                    .child(zoom_label),
            )
            .child(title_controls(&controller, cx))
            .child(self.search_box.clone());
        if in_trash || in_recent {
            // Zoom has no effect in list view contexts of trash/recent? It
            // still does (grid layout), so keep everything; only these two
            // contextual actions differ.
            let action = if in_trash {
                Button::new("empty-trash")
                    .ghost()
                    .danger()
                    .xsmall()
                    .label(rust_i18n::t!("workspace.empty_all").to_string())
                    .tooltip(rust_i18n::t!("workspace.empty_all_tooltip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.empty_trash(cx)))
            } else {
                Button::new("clear-history")
                    .ghost()
                    .danger()
                    .xsmall()
                    .label(rust_i18n::t!("workspace.clear_history").to_string())
                    .tooltip(rust_i18n::t!("workspace.clear_history_tooltip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.clear_view_history(cx)))
            };
            row = row.child(action);
        }
        Some(row.into_any_element())
    }
}

/// The model viewport's controls, for the panel title bar.
///
/// A thin adaptation rather than a reimplementation: the viewport owns these
/// buttons, so the bar asks it for them and the two cannot drift.
fn model_toolbar(viewport: &Entity<ModelViewport>, cx: &mut Context<WorkspacePanel>) -> AnyElement {
    viewport.update(cx, |viewport, cx| {
        viewport.title_tools(cx).into_any_element()
    })
}

/// The still / video preview's title-bar controls: just the close button,
/// because the tab beside it already names the asset. Zoom is wheel-only.
fn preview_toolbar(cx: &mut Context<WorkspacePanel>) -> Div {
    h_flex().items_center().child(
        Button::new("preview-close")
            .ghost()
            .xsmall()
            .icon(IconName::Close)
            .tooltip(rust_i18n::t!("viewport.close").to_string())
            .on_click(cx.listener(|this, _, window, cx| {
                this.dismiss_preview(window, cx);
            })),
    )
}
