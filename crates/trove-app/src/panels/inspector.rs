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
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;

use trove_core::model::{AssetKind, AssetPatch, MAX_RATING};
use trove_core::store::{assets, tags};
use uuid::Uuid;

use crate::state::LibraryController;

use super::common::{hex_to_rgb, human_bytes, kind_icon, observe_controller, separator_label};

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
            InputState::new(window, cx).placeholder(rust_i18n::t!("inspector.description").to_string())
        });
        let source_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("inspector.source_url").to_string())
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
        };
        observe_controller(cx, &this.controller);

        let input = this.tag_input.clone();
        cx.subscribe_in(&input, window, |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.add_tag_from_input(cx);
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
                eprintln!("invalid patch: {e}");
                return;
            }
            if let Err(e) = assets::update(conn, asset_id, &patch) {
                eprintln!("update asset: {e}");
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
                    a.title.unwrap_or_default(),
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
        let kind = asset.kind;
        let rating = asset.rating;
        let added = asset.created_at.format("%Y-%m-%d %H:%M").to_string();
        let mime = asset.mime.clone();
        let (font_family, font_style, font_weight, font_glyphs, font_italic) = (
            asset.extra.get("font_family").and_then(|v| v.as_str()).map(str::to_string),
            asset.extra.get("font_style").and_then(|v| v.as_str()).map(str::to_string),
            asset.extra.get("font_weight").and_then(|v| v.as_u64()),
            asset.extra.get("font_glyphs").and_then(|v| v.as_u64()),
            asset.extra.get("font_italic").and_then(|v| v.as_bool()).unwrap_or(false),
        );
        let font_blob = if kind == AssetKind::Font {
            asset.rel_path.as_ref().map(|rel| ctl.library.root().join(rel))
        } else {
            None
        };

        // Re-populate the edit inputs when the selection changed.
        self.sync_editors(asset_id, window, cx);

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
                .child(Icon::new(kind_icon(kind)).size_10())
                .into_any_element(),
        };

        let edit_label = |key: &'static str| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!(key).to_string())
        };

        // The edit section made the panel taller than its dock slot: the
        // whole content scrolls inside a bounded container (same pattern as
        // the tags panel).
        let content = v_flex()
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
            .child(separator_label(cx, rust_i18n::t!("inspector.edit").to_string()))
            .child(Input::new(&self.title_input).small().appearance(true))
            .child(Input::new(&self.description_input).small().appearance(true))
            .child(Input::new(&self.source_input).small().appearance(true))
            .child(edit_label("inspector.kind"))
            .child(self.kind_row(kind))
            .child(edit_label("inspector.rating"))
            .child(self.rating_row(rating))
            .child(separator_label(cx, rust_i18n::t!("inspector.tags").to_string()))
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
                this.child(
                    separator_label(cx, rust_i18n::t!("inspector.colors").to_string()),
                )
                .child(
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
            .when(kind == AssetKind::Font, |this| {
                this.child(separator_label(cx, rust_i18n::t!("inspector.font").to_string()))
                    .child(self.font_section(
                        cx,
                        font_family,
                        font_style,
                        font_weight,
                        font_glyphs,
                        font_italic,
                        font_blob.as_deref(),
                    ))
            })
            .child(separator_label(cx, rust_i18n::t!("inspector.properties").to_string()))
            .child(property_row(cx, "inspector.mime_type", mime))
            .child(property_row(cx, "inspector.size", human_bytes(asset.size_bytes)))
            .when_some(asset.duration_ms, |this, ms| {
                this.child(property_row(cx, "inspector.duration", format_duration(ms)))
            })
            .child(property_row(cx, "inspector.dimensions", dims))
            .child(property_row(cx, "inspector.added", added))
            .child(property_row(cx, "inspector.sha256", hash));

        v_flex()
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(content),
            )
            .into_any_element()
    }
}

impl InspectorPanel {
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
                            let conn = ctl.library.store().conn();
                            let patch = AssetPatch {
                                kind: Some(kind),
                                ..Default::default()
                            };
                            let _ = assets::update(conn, id, &patch);
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
                        let conn = ctl.library.store().conn();
                        let value = if filled && current == star {
                            None
                        } else {
                            Some(star)
                        };
                        let patch = AssetPatch {
                            rating: Some(value),
                            ..Default::default()
                        };
                        let _ = assets::update(conn, id, &patch);
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
    } else if ms % 1000 != 0 {
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
