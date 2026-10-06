//! The workspace panel's title bar: the title and suffix the dock skin
//! renders in the panel's tab strip.
//!
//! The suffix has two modes — grid (item count, zoom, view/sort/favorites,
//! search) and preview (asset name + close button) — switched on whether
//! a main-area preview is open.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::dock::{Panel as DockPanel, PanelControl};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::components::controls::{icon_button, muted_label};
use crate::components::preview::{AssetPreviewPanel, ModelViewport, SubtitleEditor, font};
use crate::library::LibraryController;
use crate::panels::WorkspacePanel;
use crate::panels::workspace::MainPreview;
use crate::panels::workspace::title_controls;
use crate::panels::workspace::{confirm_destruction, empty_trash_on, purge_warning};
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
            Some(MainPreview::Subtitle(editor)) => editor.read(cx).title().to_string(),
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // A preview open: the title bar carries *that* preview's tools instead
        // of the grid's, so the content area is nothing but the picture. Which
        // set it is follows the preview, so opening one switches the bar.
        match &self.preview {
            Some(MainPreview::Asset(preview)) => {
                return Some(
                    preview_toolbar(preview, &self.controller, window, cx).into_any_element(),
                );
            }
            Some(MainPreview::Model(viewport)) => {
                return Some(model_toolbar(viewport, cx));
            }
            Some(MainPreview::Subtitle(editor)) => {
                return Some(subtitle_toolbar(editor, cx));
            }
            None => {}
        }

        let ctl = self.controller.read(cx);
        let in_trash = ctl.showing_trash;
        let in_recent = ctl.showing_recent;
        let loaded = ctl.grid_loaded.min(self.last_total);
        let total = self.last_total;
        let controller = self.controller.clone();
        // The refinement marker only means something on a search: the legs
        // it announces refine the *ranking*, and an empty box has none.
        let refining =
            ctl.refining() && !ctl.search_text.trim().is_empty() && !in_trash && !in_recent;
        let slider_value = self.zoom_slider.read(cx).value().start();
        let zoom_label = format!("{:.0}%", (slider_value * 100.0).round());
        let count_label = if loaded < total {
            if self.last_truncated {
                // Every number here is a floor: the query ran out of candidates,
                // and there is more of the library behind it than was seen.
                rust_i18n::t!(
                    "workspace.scroll_hint_at_least",
                    loaded = loaded,
                    total = total
                )
                .to_string()
            } else {
                rust_i18n::t!("workspace.scroll_hint", loaded = loaded, total = total).to_string()
            }
        } else if self.last_truncated {
            rust_i18n::t!("workspace.items_at_least", count = total).to_string()
        } else if total == 1 {
            rust_i18n::t!("workspace.item_one").to_string()
        } else {
            rust_i18n::t!("workspace.items_many", count = total).to_string()
        };
        let mut row = h_flex()
            .items_center()
            .gap_1()
            .child(muted_label(count_label, cx))
            // The listing on screen is the fast answer; these two legs are
            // still re-ordering it in the background. An indeterminate
            // spinner, not dots: nothing about the wait is known.
            .when(refining, |row| {
                row.child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(Spinner::new().xsmall().color(cx.theme().muted_foreground))
                        .child(muted_label(
                            rust_i18n::t!("workspace.refining_results").to_string(),
                            cx,
                        )),
                )
            })
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
            .child(title_controls(&controller, cx));
        if in_trash || in_recent {
            // Zoom has no effect in list view contexts of trash/recent? It
            // still does (grid layout), so keep everything; only these two
            // contextual actions differ.
            //
            // Icon-only, and styled like every other control in this bar: the
            // glyph carries the meaning, the tooltip spells it out. The
            // destructive variant is deliberately not used — `ghost()` and
            // `danger()` set the same field, so the two chained together were
            // never a ghost in danger colours but a plain danger button, and a
            // lone red one among ghosts reads as a different kind of control
            // rather than as a warning.
            //
            // The glyphs come from the complete Lucide catalog: the
            // component-level `IconName` is a compatibility subset and has
            // neither a trash nor an eraser.
            use gpui_kit::assets::IconName as CatalogIcon;
            let action = if in_trash {
                icon_button(
                    "empty-trash",
                    CatalogIcon::Trash,
                    rust_i18n::t!("workspace.empty_all_tooltip").to_string(),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    // The most destructive click in the app, and until now the
                    // only irreversible one with nothing in front of it: it
                    // deletes every trashed blob, and every linked source file
                    // too when that setting is on.
                    let count = this
                        .controller
                        .read(cx)
                        .library
                        .stats()
                        .map(|stats| stats.trashed as usize)
                        .unwrap_or(0);
                    if count == 0 {
                        this.empty_trash(cx);
                        return;
                    }
                    let body = purge_warning(this.controller.read(cx), count);
                    let controller = this.controller.clone();
                    // The gate's opt-out path runs the action synchronously,
                    // still inside this listener body — and this listener body
                    // runs inside the panel's own update. Only the controller
                    // may be touched here; a `this.update` re-enters the panel
                    // and panics (see `empty_trash_on`).
                    confirm_destruction(&controller, window, cx, body, {
                        let controller = controller.clone();
                        move |cx| empty_trash_on(&controller, cx)
                    });
                }))
            } else {
                icon_button(
                    "clear-history",
                    CatalogIcon::Eraser,
                    rust_i18n::t!("workspace.clear_history_tooltip").to_string(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.clear_view_history(cx)))
            };
            row = row.child(action);
        }
        // The magnifier holds the last slot in every view, so it always sits
        // beside the dock's collapse button instead of shifting left when a
        // view brings an action of its own (the trash's empty button, the
        // recent list's clear-history one).
        Some(row.child(self.search_box.clone()).into_any_element())
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

/// The subtitle editor's title-bar controls — copy, edit, save, then close —
/// the same set every other preview puts here. They ask the editor entity for
/// its state and drive it back, so the view's content area stays just cues.
fn subtitle_toolbar(
    editor: &Entity<SubtitleEditor>,
    cx: &mut Context<WorkspacePanel>,
) -> AnyElement {
    use gpui_kit::assets::IconName as MediaIcon;

    let (editing, copy_text) = {
        let view = editor.read(cx);
        (view.editing(), view.copy_text(cx))
    };
    let edit_host = editor.clone();
    let save_host = editor.clone();
    let close_host = editor.clone();
    h_flex()
        .items_center()
        .gap_1()
        .child(
            Clipboard::new("subtitle-copy")
                .value(copy_text)
                .tooltip(rust_i18n::t!("subtitle.copy").to_string()),
        )
        .child(
            Button::new("subtitle-edit")
                .ghost()
                .xsmall()
                .icon(MediaIcon::Pencil)
                .toggled(editing)
                .tooltip(rust_i18n::t!("subtitle.edit").to_string())
                .on_click(cx.listener(move |_, _, _, cx| {
                    edit_host.update(cx, |this, cx| this.toggle_editing(cx));
                })),
        )
        .when(editing, |row| {
            row.child(
                Button::new("subtitle-save")
                    .ghost()
                    .xsmall()
                    .icon(MediaIcon::Save)
                    .tooltip(rust_i18n::t!("subtitle.save").to_string())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        save_host.update(cx, |this, cx| this.save(cx));
                    })),
            )
        })
        .child(
            icon_button(
                "subtitle-close",
                IconName::Close,
                rust_i18n::t!("viewport.close").to_string(),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                close_host.update(cx, |this, cx| this.close(cx));
            })),
        )
        .into_any_element()
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
    window: &mut Window,
    cx: &mut Context<WorkspacePanel>,
) -> Div {
    use gpui_kit::assets::IconName as ToolIcon;
    use gpui_kit::component::Disableable as _;

    let (asset_id, is_image, blocker, write_back, original, has_video) = {
        let panel = preview.read(cx);
        (
            panel.asset_id(),
            panel.is_image(),
            panel.edit_blocker(),
            panel.write_back(),
            panel.original_path().map(std::path::Path::to_path_buf),
            panel.has_video(),
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if write_back {
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
                                            this.open_asset_preview(id, &[], window, cx);
                                        });
                                    }
                                }
                            };
                            confirm_write_back(window, cx, original.clone(), apply);
                        } else {
                            // A linked asset's edit asks; an owned blob's does
                            // not, so this branch runs inside the listener body
                            // — inside the panel's own update, where a
                            // `panel.update` would re-enter it and panic. `this`
                            // is the panel already; no handle needed here.
                            if crate::dialogs::edit::apply_single_edit(
                                &ctl,
                                id,
                                edits.clone(),
                                window,
                                cx,
                            ) {
                                this.open_asset_preview(id, &[], window, cx);
                            }
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

    // The exposure rail, for a scene-linear source (EXR / Radiance HDR). A
    // view control, not an edit — nothing is written, so the edit blockers
    // above don't apply. Committing a value re-decodes the original through
    // the display transform off the UI thread; the stage swaps the render in
    // when it lands. The trigger stays highlighted while the exposure is not
    // the file's own default.
    if preview.read(cx).exposure_supported() {
        let stops = preview.read(cx).stops();
        let slider = preview.read(cx).exposure_slider().clone();
        bar = bar.child(
            Popover::new("preview-exposure")
                .w(px(240.))
                .trigger(
                    Button::new("preview-exposure-trigger")
                        .ghost()
                        .xsmall()
                        .icon(ToolIcon::Sun)
                        .selected(stops != 0.0)
                        .tooltip(rust_i18n::t!("viewport.exposure").to_string()),
                )
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(rust_i18n::t!("viewport.exposure").to_string()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().foreground)
                                        .child(format_stops(stops)),
                                ),
                        )
                        .child(Slider::new(&slider)),
                ),
        );
    }

    // The EXR part selector, for a multi-part file. Same view-control shape
    // as the exposure rail: nothing is written, the parts are probed off
    // thread, and the selector appears only once the list has landed with
    // more than one entry — a single-part file would be a menu that chooses
    // nothing. One committed click re-decodes the original through the part
    // decoder, exactly as one committed slider value does.
    if let Some(parts) = preview.read(cx).exr_parts().filter(|parts| parts.len() > 1) {
        let part = preview.read(cx).part();
        let preview_entity = preview.clone();
        let list = parts
            .iter()
            .enumerate()
            .map(|(index, info)| {
                let preview_entity = preview_entity.clone();
                let label = if info.name.is_empty() {
                    rust_i18n::t!("viewport.exr_part_unnamed", n = index + 1).to_string()
                } else {
                    info.name.clone()
                };
                Button::new(("preview-exr-part", index))
                    .ghost()
                    .xsmall()
                    .label(label)
                    .selected(index == part)
                    .on_click(move |_, _, cx| {
                        preview_entity.update(cx, |this, cx| this.set_part(index, cx));
                    })
            })
            .collect::<Vec<_>>();
        bar = bar.child(
            Popover::new("preview-exr-parts")
                .w(px(240.))
                .trigger(
                    Button::new("preview-exr-parts-trigger")
                        .ghost()
                        .xsmall()
                        .icon(ToolIcon::GalleryVerticalEnd)
                        .selected(part != 0)
                        .tooltip(rust_i18n::t!("viewport.exr_parts").to_string()),
                )
                .child(v_flex().p_2().gap_1().children(list)),
        );
    }

    // A video paused on a frame can hand that frame to the library as an asset
    // of its own. The button belongs to the live player rather than to the
    // asset: with no ffmpeg there is no player, and a still of the poster this
    // panel already paints would only be a copy of what is on screen.
    if has_video {
        let ctl = controller.clone();
        let preview_entity = preview.clone();
        bar = bar.child(
            icon_button(
                "preview-grab-frame",
                ToolIcon::Camera,
                rust_i18n::t!("viewport.grab_frame").to_string(),
            )
            .on_click(move |_, window, cx| {
                preview_entity.update(cx, |this, cx| {
                    this.grab_frame(&ctl, window, cx);
                });
            }),
        );
    }

    // The font specimen's tools: sample text, reset, language and weight —
    // in the same slot every other preview's tools occupy. The state comes
    // out as a snapshot first, because the read of the preview must close
    // before the controls build their keyed input state.
    let font_state = font::tool_state(preview.read(cx));
    if let Some(state) = font_state {
        bar = bar.child(font::toolbar(preview.clone(), state, window, cx));
    }

    bar.child(
        icon_button(
            "preview-close",
            IconName::Close,
            rust_i18n::t!("viewport.close").to_string(),
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.dismiss_preview(window, cx);
        })),
    )
}

/// The exposure readout: one decimal, with an explicit sign whenever the
/// slider sits off the file's own default — the sign is what makes "+1.5"
/// read as "brighter" rather than as a temperature.
fn format_stops(stops: f32) -> String {
    if stops == 0.0 {
        "0.0".to_string()
    } else {
        format!("{stops:+.1}")
    }
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

#[cfg(test)]
mod tests {
    use super::format_stops;

    /// The sign is the whole point of the readout: a value off the file's own
    /// default says which way the picture moved, while zero stays unsigned —
    /// "+0.0" would read like something was applied when nothing was.
    #[test]
    fn the_exposure_readout_signs_the_nonzero_stops() {
        assert_eq!(format_stops(0.0), "0.0");
        assert_eq!(format_stops(1.5), "+1.5");
        assert_eq!(format_stops(-2.0), "-2.0");
    }
}
