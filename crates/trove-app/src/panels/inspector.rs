//! Inspector: editable details of the selected asset plus its tags.
//!
//! The Edit section writes through [`trove_core::model::AssetPatch`]:
//! text fields commit on Enter or when the field loses focus, kind and
//! rating commit immediately on click. `editing_id` guards the refills so
//! switching assets repopulates the inputs exactly once and typing is never
//! interrupted by a render.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, AssetPatch, MAX_RATING, Origin};
use trove_core::store::{assets, tags};
use uuid::Uuid;

use crate::library::LibraryController;

use super::common::{color_swatch, hex_to_rgb, human_bytes, kind_icon, observe_controller};

// ==================== Inspector: details + tags ==============================

/// Which text field an input-state subscription is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextField {
    Title,
    Description,
    SourceUrl,
}

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    tag_input: Entity<InputState>,
    title_input: Entity<InputState>,
    description_input: Entity<InputState>,
    source_input: Entity<InputState>,
    /// The asset the edit inputs currently hold. Refills happen only when
    /// the selection changes.
    editing_id: Option<Uuid>,
    /// Font families already registered with the text system for previews
    /// (registration is process-global; skip repeats).
    font_previews: std::collections::HashSet<String>,
    /// Section ids the user collapsed (absent = expanded). Persisted on the
    /// panel so collapse state survives re-renders and asset switches.
    collapsed: std::collections::HashSet<&'static str>,
}

impl InspectorPanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let tag_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("inspector.add_tag_placeholder").to_string())
        });
        let title_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("inspector.title").to_string())
        });
        let description_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("inspector.description").to_string())
        });
        let source_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("inspector.source_url").to_string())
        });
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            tag_input,
            title_input,
            description_input,
            source_input,
            editing_id: None,
            font_previews: std::collections::HashSet::new(),
            collapsed: std::collections::HashSet::new(),
        };
        observe_controller(cx, &this.controller);

        let input = this.tag_input.clone();
        cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.add_tag_from_input(window, cx);
            }
        })
        .detach();
        // Edit fields commit on Enter and on blur — an explicit save button
        // would sit far from the field it belongs to.
        for (input, field) in [
            (this.title_input.clone(), TextField::Title),
            (this.description_input.clone(), TextField::Description),
            (this.source_input.clone(), TextField::SourceUrl),
        ] {
            cx.subscribe_in(&input, window, move |this, _, event, _window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_text(field, cx);
                }
            })
            .detach();
        }
        this
    }

    fn add_tag_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            if let Ok(tag) = ctl.library.ensure_tag(&name) {
                let _ = ctl.library.tag_assets(&[asset_id], tag.id, true);
            }
            ctl.generation += 1;
            cx.notify();
        });
        // Clear the input after adding the tag.
        self.tag_input
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// Replace the asset's whole tag group with the comma-separated names in
    /// the tag input (backed by `tags::set_for_asset`). Missing names are
    /// created; a failure keeps the old group and surfaces a notice.
    fn replace_tags_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw: String = self.tag_input.read(cx).value().to_string();
        let names: Vec<String> = raw
            .split([',', '，', ';', '；'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            return;
        }
        let controller = self.controller.clone();
        let Some(asset_id) = controller.read(cx).primary() else {
            return;
        };
        let mut failed: Option<String> = None;
        controller.update(cx, |ctl, cx| {
            let mut ids = Vec::with_capacity(names.len());
            for name in &names {
                match ctl.library.ensure_tag(name) {
                    Ok(tag) => ids.push(tag.id),
                    Err(e) => {
                        failed = Some(e.to_string());
                        break;
                    }
                }
            }
            if failed.is_none() {
                let _ = ctl.library.set_asset_tags(asset_id, &ids);
            } else if let Some(e) = failed.clone() {
                ctl.notice =
                    Some(rust_i18n::t!("inspector.replace_tags_failed", error = e).to_string());
            }
            ctl.generation += 1;
            cx.notify();
        });
        if failed.is_none() {
            self.tag_input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    /// Write one text field back to the store when it changed. An empty
    /// field clears the column (display falls back to the file name).
    fn commit_text(&mut self, field: TextField, cx: &mut Context<Self>) {
        let Some(asset_id) = self.editing_id else {
            return;
        };
        let input = match field {
            TextField::Title => &self.title_input,
            TextField::Description => &self.description_input,
            TextField::SourceUrl => &self.source_input,
        };
        let value: String = input.read(cx).value().trim().to_string();
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            let conn = ctl.library.store().conn();
            let Some(asset) = assets::get(conn, asset_id).ok().flatten() else {
                return;
            };
            let original: Option<String> = match field {
                TextField::Title => asset.title,
                TextField::Description => asset.description,
                TextField::SourceUrl => asset.source_url,
            };
            let stored = (!value.is_empty()).then_some(value.clone());
            if original == stored {
                return;
            }
            let patch = match field {
                TextField::Title => AssetPatch {
                    title: Some(stored),
                    ..Default::default()
                },
                TextField::Description => AssetPatch {
                    description: Some(stored),
                    ..Default::default()
                },
                TextField::SourceUrl => AssetPatch {
                    source_url: Some(stored),
                    ..Default::default()
                },
            };
            if let Err(e) = patch.validate() {
                ctl.notice = Some(
                    rust_i18n::t!("inspector.invalid_patch", error = e.to_string()).to_string(),
                );
                cx.notify();
                return;
            }
            if let Err(e) = ctl.library.patch_asset(asset_id, &patch) {
                ctl.notice = Some(
                    rust_i18n::t!("inspector.update_failed", error = e.to_string()).to_string(),
                );
                cx.notify();
                return;
            }
            // Title/description feed the search index; the generation bump
            // refreshes the grid and any FTS-driven views.
            ctl.generation += 1;
            cx.notify();
        });
    }

    /// Sync the edit inputs with the asset about to be displayed. Only runs
    /// when the selection actually changed.
    fn sync_editors(&mut self, asset_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_id == Some(asset_id) {
            return;
        }
        let conn = self.controller.read(cx).library.store().conn();
        let (title, description, source) = assets::get(conn, asset_id)
            .ok()
            .flatten()
            .map(|a| {
                (
                    a.title.unwrap_or_else(|| a.file_name.clone()),
                    a.description.unwrap_or_default(),
                    a.source_url.unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        self.editing_id = Some(asset_id);
        for (input, value) in [
            (&self.title_input, title),
            (&self.description_input, description),
            (&self.source_input, source),
        ] {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
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
        rust_i18n::t!("panel.inspector").to_string()
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    /// No title suffix controls (zoom removed from inspector).
    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        None::<Div>
    }
}
impl EventEmitter<PanelEvent> for InspectorPanel {}
impl Focusable for InspectorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ctl = self.controller.read(cx);
        let Some(asset_id) = ctl.primary() else {
            return v_flex()
                .p_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!("inspector.nothing_selected").to_string()),
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
                        .child(rust_i18n::t!("inspector.asset_missing").to_string()),
                )
                .into_any_element();
        };

        // Everything read from the store happens under the `ctl` borrow,
        // which ends at the last snapshot below; the owned snapshots
        // (and `sync_editors`'s own re-borrow) follow.
        let asset_tags = tags::for_asset(conn, asset_id).unwrap_or_default();
        let dims = asset
            .width
            .zip(asset.height)
            .map(|(w, h)| format!("{w} × {h}"))
            .unwrap_or_else(|| "—".into());
        let hash: String = asset
            .sha256
            .as_deref()
            .map(|s| s.chars().take(12).collect())
            .unwrap_or_else(|| "—".into());
        let thumb_path = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(ctl.library.root(), sha))
            .filter(|p| p.is_file());
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
        // Where the file lives: linked assets point at their original
        // location (recorded at import), stored assets at the library blob.
        let linked = asset.origin == Origin::Linked;
        let disk_path: Option<std::path::PathBuf> = if linked {
            asset
                .extra
                .get("source_path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
        } else {
            asset
                .rel_path
                .as_ref()
                .map(|rel| ctl.library.root().join(rel))
        };
        let kind = asset.kind;
        let rating = asset.rating;
        let added = asset.created_at.format("%Y-%m-%d %H:%M").to_string();
        let mime = asset.mime.clone();
        let (font_family, font_style, font_weight, font_glyphs, font_italic) = (
            asset
                .extra
                .get("font_family")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            asset
                .extra
                .get("font_style")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            asset.extra.get("font_weight").and_then(|v| v.as_u64()),
            asset.extra.get("font_glyphs").and_then(|v| v.as_u64()),
            asset
                .extra
                .get("font_italic")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        let font_blob = if kind == AssetKind::Font {
            if linked {
                // Linked fonts are read straight from their original file.
                disk_path.clone()
            } else {
                asset
                    .rel_path
                    .as_ref()
                    .map(|rel| ctl.library.root().join(rel))
            }
        } else {
            None
        };

        // Re-populate the edit inputs when the selection changed.
        self.sync_editors(asset_id, window, cx);

        // Preview container height based on image aspect ratio.
        // Falls back to 200px if dimensions are unknown.
        let preview_height = match (asset.width, asset.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                let aspect = w as f32 / h as f32;
                // Clamp to reasonable range: min 120px, max 360px.
                (300.0 / aspect).clamp(120.0, 360.0)
            }
            _ => 200.0,
        };

        // Animated images (GIF / animated WebP / APNG) play from the
        // original file; everything else uses the static thumbnail.
        let animated =
            super::common::animated_preview_source(Some(asset.mime.as_str()), disk_path.as_deref());
        let preview: AnyElement = if let Some(source) = animated {
            img(source)
                .w_full()
                .h(px(preview_height))
                .object_fit(gpui_kit::ObjectFit::Contain)
                .into_any_element()
        } else {
            match thumb_path {
                Some(path) => img(path)
                    .w_full()
                    .h(px(preview_height))
                    .object_fit(gpui_kit::ObjectFit::Contain)
                    .into_any_element(),
                None => v_flex()
                    .w_full()
                    .h(px(120.))
                    .items_center()
                    .justify_center()
                    .bg(cx.theme().secondary)
                    .rounded(cx.theme().radius)
                    .child(Icon::new(kind_icon(kind)).size_10())
                    .into_any_element(),
            }
        };

        let edit_label = |key: &'static str| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!(key).to_string())
        };

        // The edit section made the panel taller than its dock slot: the
        // whole content scrolls inside a bounded container (same pattern as
        // the tags panel). Information is grouped into collapsible sections;
        // the preview always stays on top.
        let edit_content = v_flex()
            .gap_2()
            .child(Input::new(&self.title_input).small().appearance(true))
            .child(Input::new(&self.description_input).small().appearance(true))
            .child(Input::new(&self.source_input).small().appearance(true))
            .child(edit_label("inspector.kind"))
            .child(self.kind_row(kind))
            .child(edit_label("inspector.rating"))
            .child(self.rating_row(rating))
            .child(edit_label("inspector.color_label"))
            .child(self.color_label_row(cx, asset.color_label.as_deref()));

        let tag_chips = h_flex()
            .flex_wrap()
            .gap_1p5()
            .children(asset_tags.iter().map(|tag| {
                let id = tag.id;
                let controller = self.controller.clone();
                let tag_color = tag.color.as_deref().and_then(hex_to_rgb);
                div()
                    .px_2()
                    .py_0p5()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().secondary)
                    .text_sm()
                    .text_color(
                        tag_color
                            .map(gpui_kit::rgb)
                            .unwrap_or_else(|| cx.theme().foreground.into()),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .items_center()
                            .child(tag.name.clone())
                            .child(
                                Button::new(format!("untag-{id}"))
                                    .xsmall()
                                    .ghost()
                                    .label("×")
                                    .on_click(move |_, _, cx| {
                                        controller.update(cx, move |ctl, cx| {
                                            let _ = ctl.library.tag_assets(&[asset_id], id, false);
                                            ctl.generation += 1;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    )
                    .into_any_element()
            }));

        let tag_input_row = h_flex()
            .gap_1()
            .items_center()
            .child(Input::new(&self.tag_input).small().flex_1())
            .child(
                Button::new("replace-tags")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Replace)
                    .tooltip(rust_i18n::t!("inspector.replace_tags_hint").to_string())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.replace_tags_from_input(window, cx);
                    })),
            );

        let tags_content = v_flex().gap_2().child(tag_chips).child(tag_input_row);

        let props_content = v_flex()
            .gap_1()
            .child(property_row(cx, "inspector.mime_type", mime))
            .child(property_row(
                cx,
                "inspector.size",
                human_bytes(asset.size_bytes),
            ))
            .when_some(asset.duration_ms, |this, ms| {
                this.child(property_row(cx, "inspector.duration", format_duration(ms)))
            })
            .child(property_row(cx, "inspector.dimensions", dims))
            .child(property_row(cx, "inspector.added", added))
            .child(property_row(cx, "inspector.sha256", hash))
            .when_some(disk_path, |row, path| {
                // Linked files can go missing (moved / deleted on disk);
                // surface that and offer a relink pick.
                let missing = !path.is_file();
                row.child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(rust_i18n::t!("inspector.location").to_string()),
                                )
                                .when(linked, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().warning)
                                            .child(rust_i18n::t!("inspector.linked").to_string()),
                                    )
                                })
                                .when(linked && missing, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().danger)
                                            .child(rust_i18n::t!("inspector.missing").to_string()),
                                    )
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .when(linked, |row| {
                                    let controller = self.controller.clone();
                                    row.child(
                                        Button::new(format!("relink-{asset_id}"))
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::RotateCw)
                                            .tooltip(rust_i18n::t!("inspector.relink").to_string())
                                            .on_click(move |_, _, cx| {
                                                prompt_relink(&controller, asset_id, cx);
                                            }),
                                    )
                                })
                                .child(
                                    Button::new(format!("reveal-{asset_id}"))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Folder)
                                        .tooltip(
                                            rust_i18n::t!("workspace.reveal_in_file_manager")
                                                .to_string(),
                                        )
                                        .on_click(move |_, _, _cx| {
                                            crate::panels::common::reveal_path(&path);
                                        }),
                                ),
                        ),
                )
            });

        let mut content = v_flex()
            .p_3()
            .gap_2()
            .w_full()
            .child(
                // Preview: dynamic height based on aspect ratio, fills width.
                div()
                    .w_full()
                    .h(px(preview_height))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(cx.theme().radius)
                    .overflow_hidden()
                    .child(preview),
            )
            .child(self.collapsible_section(
                "edit",
                rust_i18n::t!("inspector.edit").to_string(),
                cx,
                edit_content,
            ))
            .child(self.collapsible_section(
                "tags",
                rust_i18n::t!("inspector.tags").to_string(),
                cx,
                tags_content,
            ))
            .child(self.collapsible_section(
                "properties",
                rust_i18n::t!("inspector.properties").to_string(),
                cx,
                props_content,
            ));

        if !swatches.is_empty() {
            let controller = self.controller.clone();
            let colors_content =
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .px_1()
                    .children(swatches.iter().map(|(_rgb, hex)| {
                        let hex = hex.clone();
                        let menu_controller = controller.clone();
                        let menu_hex = hex.clone();
                        // Right-click opens the swatch menu: search images
                        // whose palette contains this colour, or copy the hex.
                        div()
                            .id(format!("swatch-wrap-{hex}"))
                            .context_menu(move |menu, _, _| {
                                menu.item(
                                    PopupMenuItem::new(
                                        rust_i18n::t!("inspector.search_same_color").to_string(),
                                    )
                                    .on_click({
                                        let controller = menu_controller.clone();
                                        let hex = menu_hex.clone();
                                        move |_, window, cx| {
                                            super::workspace_search::open_color_search(
                                                &hex,
                                                &controller,
                                                window,
                                                cx,
                                            );
                                        }
                                    }),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new(
                                        rust_i18n::t!("inspector.copy_hex").to_string(),
                                    )
                                    .on_click({
                                        let hex = menu_hex.clone();
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                hex.clone(),
                                            ));
                                        }
                                    }),
                                )
                            })
                            .child(color_swatch(
                                cx,
                                format!("swatch-{hex}"),
                                &hex,
                                false,
                                |_, _, _| {},
                            ))
                    }));
            content = content.child(self.collapsible_section(
                "colors",
                rust_i18n::t!("inspector.colors").to_string(),
                cx,
                colors_content,
            ));
        }

        if kind == AssetKind::Font {
            let font_content = self.font_section(
                cx,
                font_family,
                font_style,
                font_weight,
                font_glyphs,
                font_italic,
                font_blob.as_deref(),
                asset.sha256.clone(),
            );
            content = content.child(self.collapsible_section(
                "font",
                rust_i18n::t!("inspector.font").to_string(),
                cx,
                font_content,
            ));
        }

        v_flex()
            .size_full()
            .flex_1() // ← 填满 Dock 分配的垂直空间
            .gap_0() // ← 子元素之间无间距，内容紧贴
            .bg(cx.theme().background) // ← 设置背景色，填满整个面板
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .size_full() // ← 水平方向也填满
                    .overflow_y_scrollbar()
                    .child(content),
            )
            .into_any_element()
    }
}

impl InspectorPanel {
    /// A titled section whose body can be collapsed. Clicking the header
    /// toggles the state stored in `self.collapsed` (keyed by `id`), so it
    /// survives re-renders and asset switches.
    fn collapsible_section(
        &self,
        id: &'static str,
        title: String,
        cx: &mut Context<Self>,
        content: Div,
    ) -> Div {
        let open = !self.collapsed.contains(id);
        let header = h_flex()
            .id(id)
            .w_full()
            .items_center()
            .justify_between()
            .px_1()
            .py_0p5()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().secondary))
            .on_click(cx.listener(move |this, _, _, cx| {
                // Toggle: absent → insert (collapse), present → remove.
                if !this.collapsed.remove(id) {
                    this.collapsed.insert(id);
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .child(
                // Chevron points down when open, right when collapsed.
                // percentage() panics on negatives — use 0.75 (270° cw)
                // rather than -0.25 for the right-pointing state.
                Icon::new(IconName::ChevronDown)
                    .size_3()
                    .text_color(cx.theme().muted_foreground)
                    .rotate(gpui::percentage(if open { 0. } else { 0.75 })),
            );
        div()
            .w_full()
            .child(header)
            .when(open, |this| this.child(content))
    }

    /// One small button per [`AssetKind`]; the active kind is highlighted.
    fn kind_row(&self, active: AssetKind) -> Div {
        fn key(kind: AssetKind) -> &'static str {
            match kind {
                AssetKind::Image => "asset.kind.image",
                AssetKind::Video => "asset.kind.video",
                AssetKind::Audio => "asset.kind.audio",
                AssetKind::Document => "asset.kind.document",
                AssetKind::Archive => "asset.kind.archive",
                AssetKind::Font => "asset.kind.font",
                AssetKind::Other => "asset.kind.other",
            }
        }
        h_flex().flex_wrap().gap_1().children(
            [
                AssetKind::Image,
                AssetKind::Video,
                AssetKind::Audio,
                AssetKind::Document,
                AssetKind::Archive,
                AssetKind::Font,
                AssetKind::Other,
            ]
            .map(|kind| {
                let controller = self.controller.clone();
                Button::new(format!("kind-{:?}", kind).to_lowercase())
                    .xsmall()
                    .when(kind == active, |b| b.primary())
                    .when(kind != active, |b| b.ghost())
                    .label(rust_i18n::t!(key(kind)).to_string())
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |ctl, cx| {
                            let Some(id) = ctl.primary() else { return };
                            let patch = AssetPatch {
                                kind: Some(kind),
                                ..Default::default()
                            };
                            let _ = ctl.library.patch_asset(id, &patch);
                            ctl.generation += 1;
                            cx.notify();
                        });
                    })
            }),
        )
    }

    /// Font facts + a live specimen. The blob is registered with the text
    /// system once per family (registration is process-global); until that
    /// succeeds the section shows only the metadata lines.
    #[allow(clippy::too_many_arguments)]
    fn font_section(
        &mut self,
        cx: &mut Context<Self>,
        family: Option<String>,
        style: Option<String>,
        weight: Option<u64>,
        glyphs: Option<u64>,
        italic: bool,
        blob: Option<&std::path::Path>,
        sha: Option<String>,
    ) -> Div {
        let registered = family
            .as_ref()
            .is_some_and(|f| self.ensure_font_registered(f, blob, cx));

        let mut section = v_flex().gap_1();
        if let (Some(family), true) = (&family, registered) {
            section = section.child(
                div()
                    .font_family(family.clone())
                    .text_xl()
                    .text_color(cx.theme().foreground)
                    .child("AaBbYyZz 允 123"),
            );
        }
        if let Some(family) = &family {
            section = section.child(
                div()
                    .text_sm()
                    .truncate()
                    .text_color(cx.theme().foreground)
                    .child(family.clone()),
            );
        }
        let mut meta: Vec<String> = Vec::new();
        if let Some(style) = &style {
            meta.push(style.clone());
        }
        if let Some(weight) = weight {
            meta.push(weight.to_string());
        }
        if italic {
            meta.push(rust_i18n::t!("inspector.italic").to_string());
        }
        if let Some(glyphs) = glyphs {
            meta.push(format!("{glyphs} glyphs"));
        }
        if !meta.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta.join(" · ")),
            );
        }

        // System install: user-level fonts directory, hash-named copy. The
        // button state re-evaluates on the next render after the action.
        if let Some(sha) = sha {
            let installed = crate::fonts::is_installed(&sha);
            let controller = self.controller.clone();
            let controller_err = controller.clone();
            let sha_err = sha.clone();
            section = section.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(installed, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("inspector.font_installed").to_string()),
                        )
                    })
                    .child(if installed {
                        Button::new("font-uninstall")
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("inspector.font_uninstall").to_string())
                            .on_click(move |_, _, cx| {
                                if let Err(e) = crate::fonts::uninstall(&sha_err) {
                                    controller_err.update(cx, |ctl, cx| {
                                        ctl.notice = Some(
                                            rust_i18n::t!("notice.font_install_failed", error = e)
                                                .to_string(),
                                        );
                                        cx.notify();
                                    });
                                }
                                cx.refresh_windows();
                            })
                            .into_any_element()
                    } else {
                        let sha_install = sha.clone();
                        Button::new("font-install")
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("inspector.font_install").to_string())
                            .on_click({
                                let blob = blob.map(|p| p.to_path_buf());
                                move |_, _, cx| {
                                    let Some(blob) = &blob else { return };
                                    match crate::fonts::install(blob, &sha_install) {
                                        Ok(_) => cx.refresh_windows(),
                                        Err(e) => controller_err.update(cx, |ctl, cx| {
                                            ctl.notice = Some(
                                                rust_i18n::t!(
                                                    "notice.font_install_failed",
                                                    error = e
                                                )
                                                .to_string(),
                                            );
                                            cx.notify();
                                        }),
                                    }
                                }
                            })
                            .into_any_element()
                    }),
            );
        }

        section
    }

    /// Register the font bytes behind `family` with the process text system
    /// so `.font_family(family)` resolves to the imported face. Best-effort.
    fn ensure_font_registered(
        &mut self,
        family: &str,
        blob: Option<&std::path::Path>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.font_previews.contains(family) {
            return true;
        }
        let Some(path) = blob else {
            return false;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let ok = cx
            .text_system()
            .add_fonts(vec![std::borrow::Cow::Owned(bytes)])
            .is_ok();
        if ok {
            self.font_previews.insert(family.to_string());
        }
        ok
    }

    /// Color-label palette row — the shared [`super::color_label`] widget.
    /// Left-click applies to the primary asset, right-click opens the
    /// function menu (set any color / clear).
    fn color_label_row(&self, cx: &App, current: Option<&str>) -> impl IntoElement {
        super::color_label::picker(&self.controller, current, cx)
    }

    /// Five star toggles; clicking the current top star clears the rating.
    fn rating_row(&self, rating: Option<u8>) -> Div {
        let current = rating.unwrap_or(0);
        h_flex().gap_0p5().children((1..=MAX_RATING).map(|star| {
            let filled = star <= current;
            let controller = self.controller.clone();
            Button::new(format!("rating-{star}"))
                .xsmall()
                .ghost()
                .icon(if filled {
                    IconName::StarFill
                } else {
                    IconName::Star
                })
                .on_click(move |_, _, cx| {
                    controller.update(cx, |ctl, cx| {
                        let Some(id) = ctl.primary() else { return };
                        let value = if filled && current == star {
                            None
                        } else {
                            Some(star)
                        };
                        let patch = AssetPatch {
                            rating: Some(value),
                            ..Default::default()
                        };
                        let _ = ctl.library.patch_asset(id, &patch);
                        ctl.generation += 1;
                        cx.notify();
                    });
                })
        }))
    }
}

/// `90500` → `1:30.5`, `3725000` → `1:02:05`.
fn format_duration(ms: u64) -> String {
    let secs = (ms / 1000).max(1);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else if !ms.is_multiple_of(1000) {
        format!("{m}:{s:02}.{:.2}", (ms % 1000) / 10)
    } else {
        format!("{m}:{s:02}")
    }
}

fn property_row(cx: &Context<impl Render>, key: &'static str, value: String) -> Div {
    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!(key).to_string()),
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

/// Pick a new location for a linked asset whose file moved on disk, then
/// re-point the record (the core verifies the content hash still matches).
fn prompt_relink(controller: &Entity<LibraryController>, asset_id: Uuid, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(rust_i18n::t!("inspector.relink_prompt").into_owned().into()),
    });
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = rx.await
            && let Some(path) = paths.into_iter().next()
        {
            cx.update(|cx| {
                controller.update(cx, |ctl, cx| {
                    match ctl.library.relink_asset(asset_id, &path) {
                        Ok(()) => {
                            ctl.notice = Some(rust_i18n::t!("notice.relink_done").to_string());
                            ctl.generation += 1;
                            cx.notify();
                        }
                        Err(e) => {
                            ctl.notice = Some(
                                rust_i18n::t!("notice.relink_failed", error = e.to_string())
                                    .to_string(),
                            );
                            cx.notify();
                        }
                    }
                });
            });
        }
    })
    .detach();
}
