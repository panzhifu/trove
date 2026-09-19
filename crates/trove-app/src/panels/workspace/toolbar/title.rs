//! The workspace panel's title bar: the title and suffix the dock skin
//! renders in the panel's tab strip.
//!
//! The suffix has two modes — grid (item count, zoom, view/sort/favorites,
//! search) and preview (asset name + close button) — switched on whether
//! a main-area preview is open.

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::dock::{Panel as DockPanel, PanelControl};
use gpui_kit::component::slider::Slider;
use gpui_kit::component::{IconName, Sizable as _, WindowExt as _};
use gpui_kit::*;

use crate::components::preview::{AssetPreviewPanel, ModelViewport};
use crate::library::LibraryController;
use crate::panels::WorkspacePanel;
use crate::panels::workspace::MainPreview;
use crate::panels::workspace::title_controls;
use trove_core::media::edit::ImageEdit;

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
            Some(MainPreview::Asset(preview)) => {
                return Some(preview_toolbar(preview, &self.controller, cx).into_any_element());
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

/// The still / video preview's title-bar controls: the picture's edit tools
/// (rotate, flip, the full edit dialog) for images, then the close button.
/// The tools always show for an image — disabled, with the reason on their
/// tooltips, when the backend would refuse the edit (a linked file belongs
/// to its source; a trashed asset is not editable until restored) — a
/// silently missing toolbar reads as a bug, not as a constraint. The tab
/// beside the bar already names the asset; zoom is a gesture on the stage
/// itself, not a toolbar control.
fn preview_toolbar(
    preview: &Entity<AssetPreviewPanel>,
    controller: &Entity<LibraryController>,
    cx: &mut Context<WorkspacePanel>,
) -> Div {
    use gpui_kit::assets::IconName as ToolIcon;
    use gpui_kit::component::Disableable as _;

    let (asset_id, is_image, blocker, write_back, original) = {
        let panel = preview.read(cx);
        (
            panel.asset_id(),
            panel.is_image(),
            panel.edit_blocker(),
            panel.write_back(),
            panel.original_path().map(std::path::Path::to_path_buf),
        )
    };
    // The panel entity, so a write-back confirmation can reopen the preview
    // once the backend has written — the dialog callback runs outside any
    // listener on this panel.
    let panel_entity = cx.entity();
    let mut bar = h_flex().items_center().gap_1();

    // The pixel edits act on the picture on screen — not on the grid
    // selection, which the preview replaced. Each quick edit re-encodes at
    // the dialog's default quality and re-opens the preview, so the edited
    // result replaces the picture the moment the backend wrote it. A linked
    // asset's edit overwrites the user's own file, so it asks first.
    if let Some(id) = asset_id
        && is_image
    {
        let blocked = blocker.is_some();
        // A refusal reason overrides the button's own label: the user should
        // learn why the tools are grey, not what they do.
        let tooltip = |key: &'static str| match blocker {
            Some(why) => rust_i18n::t!(why).to_string(),
            None => rust_i18n::t!(key).to_string(),
        };
        for (btn_id, icon, key, edits) in [
            (
                "preview-rotate-cw",
                ToolIcon::RotateCw,
                "viewport.rotate_cw",
                vec![ImageEdit::Rotate90],
            ),
            (
                "preview-rotate-ccw",
                ToolIcon::RotateCcw,
                "viewport.rotate_ccw",
                vec![ImageEdit::Rotate270],
            ),
            (
                "preview-flip-h",
                ToolIcon::FlipHorizontal2,
                "viewport.flip_horizontal",
                vec![ImageEdit::FlipHorizontal],
            ),
            (
                "preview-flip-v",
                ToolIcon::FlipVertical2,
                "viewport.flip_vertical",
                vec![ImageEdit::FlipVertical],
            ),
        ] {
            let ctl = controller.clone();
            let panel = panel_entity.clone();
            let original = original.clone();
            bar = bar.child(
                Button::new(btn_id)
                    .ghost()
                    .xsmall()
                    .icon(icon)
                    .disabled(blocked)
                    .tooltip(tooltip(key))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        let apply = {
                            let ctl = ctl.clone();
                            let edits = edits.clone();
                            let panel = panel.clone();
                            // `Fn`, not `FnOnce`: the write-back confirmation
                            // hands it to a dialog callback that may be built
                            // more than once.
                            move |window: &mut Window, cx: &mut App| {
                                if crate::dialogs::edit::apply_single_edit(
                                    &ctl,
                                    id,
                                    edits.clone(),
                                    window,
                                    cx,
                                ) {
                                    panel.update(cx, |this, cx| {
                                        this.open_asset_preview(id, window, cx);
                                    });
                                }
                            }
                        };
                        if write_back {
                            confirm_write_back(window, cx, original.clone(), apply);
                        } else {
                            apply(window, cx);
                        }
                    })),
            );
        }
        // Everything the quick buttons cannot express — a crop, a rotation
        // composed with it, a different quality — stays in the dialog, now
        // aimed at this one asset.
        let ctl = controller.clone();
        bar = bar.child(
            Button::new("preview-edit")
                .ghost()
                .xsmall()
                .icon(ToolIcon::Pencil)
                .disabled(blocked)
                .tooltip(tooltip("viewport.edit_image"))
                .on_click(move |_, window, cx| {
                    crate::dialogs::edit::EditDialog::open_for_asset(window, cx, ctl.clone(), id);
                }),
        );
    }

    bar.child(
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

/// The write-back confirmation for a linked asset: editing overwrites the
/// file the user keeps, so the edit runs only after an OK that names the
/// path and says the act cannot be undone.
fn confirm_write_back(
    window: &mut Window,
    cx: &mut App,
    path: Option<std::path::PathBuf>,
    run: impl Fn(&mut Window, &mut App) + 'static,
) {
    let Some(path) = path else {
        // No recorded path to name: run as-is — the backend reports what
        // happens (a link without a reachable original fails loudly there).
        run(window, cx);
        return;
    };
    // `Rc` rather than a move: the dialog builder closure is `Fn` (the
    // framework may build it again), and each build hands a clone to `on_ok`.
    let run = std::rc::Rc::new(run);
    window.open_dialog(cx, move |dialog, _, _| {
        let run = run.clone();
        let shown = path.display().to_string();
        dialog
            .title(rust_i18n::t!("edit.writeback_title").to_string())
            .width(px(440.))
            .close_button(false)
            .child(
                div()
                    .text_sm()
                    .p_1()
                    .child(rust_i18n::t!("edit.writeback_body", path = shown).to_string()),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("edit.writeback_ok").to_string())
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                run(window, cx);
                true
            })
    });
}
