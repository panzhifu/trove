//! Smart-collection rule editor: a dialog that builds the JSON condition
//! tree stored in a smart collection.
//!
//! The editor covers the flat shape — one group (`and`/`or`) over N match
//! conditions — which is everything the save-search flow produces and the
//! overwhelming majority of hand-written rules. Deeper nesting round-trips
//! through [`load_rows`]: only the top-level match conditions are surfaced.
//!
//! Draft state lives in a [`RuleDraft`] entity created before the dialog
//! opens (the dialog's content closure is a re-run-per-frame `Fn` and
//! cannot own state). Every mutation bumps [`RuleDraft::revision`]; the
//! dialog recomputes the live match count whenever the revision moved.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, SmartCollection, SmartCompare, SmartField, SmartNode};
use trove_core::store::{smart, smart_collections, tags};
use uuid::Uuid;

use crate::panels::common::color_swatch;
use crate::state::LibraryController;

// ============================ draft state ====================================

/// One condition row. `text` backs every free-text value (text, extension,
/// color, size); the other value kinds keep their state inline.
struct ConditionRow {
    field: SmartField,
    op: SmartCompare,
    text: Entity<InputState>,
    kind: AssetKind,
    favorite: bool,
    tag: String,
    rating: u8,
}

impl ConditionRow {
    fn new(window: &mut Window, cx: &mut App, field: SmartField) -> Self {
        let placeholder = match field {
            SmartField::Color => rust_i18n::t!("rules.value_color_hint").to_string(),
            SmartField::SizeBytes => rust_i18n::t!("rules.value_bytes").to_string(),
            SmartField::Extension => "jpg, png…".to_string(),
            _ => String::new(),
        };
        Self {
            field,
            op: SmartCompare::Eq,
            text: cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
            kind: AssetKind::Image,
            favorite: true,
            tag: String::new(),
            rating: 3,
        }
    }

    /// A row is complete when its value side carries a usable value.
    fn is_complete(&self, cx: &App) -> bool {
        match self.field {
            SmartField::Text
            | SmartField::Extension
            | SmartField::Color
            | SmartField::SizeBytes => !self.text.read(cx).value().trim().is_empty(),
            SmartField::Tag => !self.tag.is_empty(),
            SmartField::Kind | SmartField::IsFavorite | SmartField::Rating => true,
        }
    }
}

/// The editor's draft: group mode, condition rows and the live match status.
struct RuleDraft {
    controller: Entity<LibraryController>,
    /// `Some(id)` while editing an existing smart collection.
    editing: Option<Uuid>,
    name_input: Entity<InputState>,
    and_mode: bool,
    rows: Vec<ConditionRow>,
    /// Tag names for the tag dropdown (snapshot at open).
    tag_names: Vec<String>,
    /// Display color of the smart collection itself (`#rrggbb` or none).
    color: Option<String>,
    /// HSL pickers feeding [`Self::color`] (kept in sync both ways).
    pick: (f32, f32, f32),
    hue: Entity<SliderState>,
    sat: Entity<SliderState>,
    lig: Entity<SliderState>,
    /// Lives with the draft: when the dialog closes the draft (and these)
    /// drop, unsubscribing the sliders.
    _subs: Vec<Subscription>,
    /// Bumped by every mutation; the dialog recomputes the match count when
    /// it drifts from `evaluated`.
    revision: u64,
    evaluated: u64,
    match_total: Option<u64>,
    error: Option<String>,
}

impl RuleDraft {
    fn touch(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        cx.notify();
    }

    fn add_row(&mut self, field: SmartField, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.push(ConditionRow::new(window, cx, field));
        self.touch(cx);
    }

    fn remove_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() {
            self.rows.remove(ix);
            self.touch(cx);
        }
    }

    fn set_field(&mut self, ix: usize, field: SmartField, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            if row.field != field {
                row.field = field;
                row.op = SmartCompare::Eq;
                row.kind = AssetKind::Image;
                row.favorite = true;
                row.tag = self.tag_names.first().cloned().unwrap_or_default();
                row.rating = 3;
            }
        }
        self.touch(cx);
    }

    fn set_op(&mut self, ix: usize, op: SmartCompare, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.op = op;
        }
        self.touch(cx);
    }

    fn set_kind(&mut self, ix: usize, kind: AssetKind, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.kind = kind;
        }
        self.touch(cx);
    }

    fn set_favorite(&mut self, ix: usize, favorite: bool, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.favorite = favorite;
        }
        self.touch(cx);
    }

    fn set_tag(&mut self, ix: usize, tag: String, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.tag = tag;
        }
        self.touch(cx);
    }

    fn set_rating(&mut self, ix: usize, rating: u8, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.rating = rating;
        }
        self.touch(cx);
    }

    fn set_and_mode(&mut self, and_mode: bool, cx: &mut Context<Self>) {
        self.and_mode = and_mode;
        self.touch(cx);
    }

    /// Set the color from a preset swatch (or clear it) and sync the HSL
    /// sliders to the new value.
    fn set_color(&mut self, color: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.color = color.clone();
        self.pick = color
            .as_deref()
            .and_then(hex_to_hsl)
            .unwrap_or(self.pick);
        let (h, sat, lig) = self.pick;
        self.hue.update(cx, |st, cx| st.set_value(h, window, cx));
        self.sat.update(cx, |st, cx| st.set_value(sat, window, cx));
        self.lig.update(cx, |st, cx| st.set_value(lig, window, cx));
        self.touch(cx);
    }

    /// Set the color from the slider panel (no slider sync — they are the
    /// source).
    fn set_pick(&mut self, h: f32, s: f32, l: f32, cx: &mut Context<Self>) {
        self.pick = (h, s, l);
        self.color = Some(hsl_to_hex(h, s, l));
        self.touch(cx);
    }

    /// Compile the draft into the stored JSON tree. Incomplete rows are
    /// skipped; an empty result is an error the dialog surfaces.
    fn build_json(&self, cx: &App) -> Result<serde_json::Value, String> {
        let t = |k: &str| rust_i18n::t!(k).to_string();
        let mut children = Vec::new();
        for row in &self.rows {
            if !row.is_complete(cx) {
                continue;
            }
            // The `op` tag is required by the internally-tagged SmartNode
            // enum — omitting it makes node_from_json fail with
            // "missing field `op`".
            let mut node = serde_json::json!({ "op": "match", "field": field_json(row.field) });
            if row.op != SmartCompare::Eq {
                node["compare"] = serde_json::json!(compare_json(row.op));
            }
            node["value"] = match row.field {
                SmartField::Text => serde_json::json!(row.text.read(cx).value().trim()),
                SmartField::Extension => serde_json::json!(normalize_extension(
                    row.text.read(cx).value().trim()
                )),
                SmartField::Color => match normalize_color(&row.text.read(cx).value()) {
                    Some(hex) => serde_json::json!(hex),
                    None => return Err(t("rules.invalid_color")),
                },
                SmartField::SizeBytes => match row.text.read(cx).value().trim().parse::<i64>() {
                    Ok(n) if n > 0 => serde_json::json!(n),
                    _ => return Err(t("rules.invalid_bytes")),
                },
                SmartField::Tag => serde_json::json!(row.tag),
                SmartField::Rating => serde_json::json!(row.rating),
                SmartField::Kind => serde_json::json!(row.kind),
                SmartField::IsFavorite => serde_json::json!(row.favorite),
            };
            children.push(node);
        }
        if children.is_empty() {
            return Err(t("rules.need_condition"));
        }
        let op = if self.and_mode { "and" } else { "or" };
        Ok(serde_json::json!({ "op": op, "children": children }))
    }

    /// Re-run the live match count when the draft moved since last draw.
    fn recompute_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.revision == self.evaluated {
            return;
        }
        self.evaluated = self.revision;
        let conn = self.controller.read(cx).library.store().conn();
        let outcome = self.build_json(cx).and_then(|json| {
            // The raw serde message is English internals; the localized
            // "invalid rule" label is enough for the live count status.
            let node = smart::node_from_json(&json)
                .map_err(|_| rust_i18n::t!("rules.match_error").to_string())?;
            smart::evaluate(conn, &node, None, 0)
                .map(|(total, _)| total)
                .map_err(|e| e.to_string())
        });
        match outcome {
            Ok(total) => {
                self.match_total = Some(total);
                self.error = None;
            }
            Err(msg) => {
                self.match_total = None;
                self.error = Some(msg);
            }
        }
    }
}

fn normalize_extension(raw: &str) -> String {
    raw.trim().trim_start_matches('.').to_lowercase()
}

/// HSL (h 0–360, s/l 0–100) → `#rrggbb`.
fn hsl_to_hex(h: f32, s: f32, l: f32) -> String {
    let (r, g, b) = hsl_to_rgb(h, s, l);
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.) / 360.;
    let s = (s / 100.).clamp(0., 1.);
    let l = (l / 100.).clamp(0., 1.);
    if s == 0. {
        let v = (l * 255.).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |mut t: f32| -> u8 {
        if t < 0. {
            t += 1.;
        }
        if t > 1. {
            t -= 1.;
        }
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 1. / 2. {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        (v * 255.).round() as u8
    };
    (channel(h + 1. / 3.), channel(h), channel(h - 1. / 3.))
}

fn hex_to_hsl(hex: &str) -> Option<(f32, f32, f32)> {
    let hex = normalize_color(hex)?;
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).ok()?;
    let (r, g, b) = (
        ((n >> 16) & 0xff) as f32 / 255.,
        ((n >> 8) & 0xff) as f32 / 255.,
        (n & 0xff) as f32 / 255.,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    if (max - min).abs() < f32::EPSILON {
        return Some((0., 0., l * 100.));
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2. - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6. } else { 0. }
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    } * 60.;
    Some((h, s * 100., l * 100.))
}

fn normalize_color(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches('#').to_lowercase();
    (s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(format!("#{s}"))
}

// ============================ json (de)serialization =========================

fn field_json(field: SmartField) -> &'static str {
    match field {
        SmartField::Kind => "kind",
        SmartField::IsFavorite => "is_favorite",
        SmartField::Rating => "rating",
        SmartField::Tag => "tag",
        SmartField::Text => "text",
        SmartField::Extension => "extension",
        SmartField::SizeBytes => "size_bytes",
        SmartField::Color => "color",
    }
}

fn compare_json(op: SmartCompare) -> &'static str {
    match op {
        SmartCompare::Eq => "eq",
        SmartCompare::Ne => "ne",
        SmartCompare::Gt => "gt",
        SmartCompare::Gte => "gte",
        SmartCompare::Lt => "lt",
        SmartCompare::Lte => "lte",
    }
}

/// Load a stored tree into `(and_mode, rows)`. Only top-level match
/// conditions are surfaced (see the module docs); a bare match becomes a
/// single-condition `and` group.
fn load_rows(
    json: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> (bool, Vec<ConditionRow>) {
    let node = smart::node_from_json(json).unwrap_or(SmartNode::And { children: Vec::new() });
    let (and_mode, matches) = match node {
        SmartNode::And { children } => (true, children),
        SmartNode::Or { children } => (false, children),
        match_node @ SmartNode::Match { .. } => (true, vec![match_node]),
    };

    let mut rows = Vec::new();
    for child in matches {
        let SmartNode::Match { field, op, value } = child else {
            continue;
        };
        let mut row = ConditionRow::new(window, cx, field);
        row.op = op;
        match field {
            SmartField::Text | SmartField::Extension | SmartField::Color => {
                let s = value.as_str().unwrap_or_default().to_string();
                row.text.update(cx, |state, cx| state.set_value(s, window, cx));
            }
            SmartField::SizeBytes => {
                let s = value.as_i64().map(|n| n.to_string()).unwrap_or_default();
                row.text.update(cx, |state, cx| state.set_value(s, window, cx));
            }
            SmartField::Rating => {
                row.rating = value.as_u64().map(|n| n.clamp(0, 5) as u8).unwrap_or(3);
            }
            SmartField::Kind => {
                row.kind = serde_json::from_value(value).unwrap_or(AssetKind::Image);
            }
            SmartField::IsFavorite => {
                row.favorite = value.as_bool().unwrap_or(true);
            }
            SmartField::Tag => {
                row.tag = value.as_str().unwrap_or_default().to_string();
            }
        }
        rows.push(row);
    }
    (and_mode, rows)
}

// ============================ the dialog =====================================

/// Open the rule editor: empty for a new smart collection, prefilled from
/// `editing`'s stored tree otherwise.
pub fn open_rule_editor(
    window: &mut Window,
    cx: &mut App,
    controller: Entity<LibraryController>,
    editing: Option<SmartCollection>,
) {
    let is_edit = editing.is_some();
    let name_input = cx.new(|cx| {
        InputState::new(window, cx).placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
    });
    if let Some(name) = editing.as_ref().map(|sc| sc.name.clone()) {
        name_input.update(cx, |state, cx| state.set_value(name, window, cx));
    }
    let (and_mode, rows) = match &editing {
        Some(sc) => load_rows(&sc.query, window, cx),
        None => (true, Vec::new()),
    };
    let tag_names = {
        let conn = controller.read(cx).library.store().conn();
        tags::list(conn)
            .map(|list| list.into_iter().map(|t| t.name).collect())
            .unwrap_or_default()
    };
    let color = editing
        .as_ref()
        .and_then(|sc| sc.color.clone())
        .and_then(|c| normalize_color(&c));
    let pick = color
        .as_deref()
        .and_then(hex_to_hsl)
        .unwrap_or((210., 75., 55.));
    let hue = cx.new(|_| SliderState::new().min(0.).max(360.).default_value(pick.0));
    let sat = cx.new(|_| SliderState::new().min(0.).max(100.).default_value(pick.1));
    let lig = cx.new(|_| SliderState::new().min(0.).max(100.).default_value(pick.2));
    let draft = cx.new(|_| RuleDraft {
        controller,
        editing: editing.map(|sc| sc.id),
        name_input,
        and_mode,
        rows,
        tag_names,
        color,
        pick,
        hue: hue.clone(),
        sat: sat.clone(),
        lig: lig.clone(),
        _subs: Vec::new(),
        revision: 1,
        evaluated: 0,
        match_total: None,
        error: None,
    });

    // Slider → draft: pick channels update the color live while dragging.
    #[derive(Clone, Copy)]
    enum Channel {
        H,
        S,
        L,
    }
    let mut subs = Vec::new();
    let sliders = [
        (hue.clone(), Channel::H),
        (sat.clone(), Channel::S),
        (lig.clone(), Channel::L),
    ];
    for (slider, channel) in &sliders {
        let channel = *channel;
        let d = draft.clone();
        subs.push(cx.subscribe(slider, move |_slider, event: &SliderEvent, cx| {
            let value = match event {
                SliderEvent::Change(v) | SliderEvent::Release(v) => v.start(),
            };
            d.update(cx, |d, cx| {
                let (h, s, l) = d.pick;
                match channel {
                    Channel::H => d.set_pick(value.clamp(0., 360.), s, l, cx),
                    Channel::S => d.set_pick(h, value.clamp(0., 100.), l, cx),
                    Channel::L => d.set_pick(h, s, value.clamp(0., 100.), cx),
                }
            });
        }));
    }
    draft.update(cx, |d, _| d._subs = subs);

    window.open_dialog(cx, move |dialog, _, cx| {
        draft.update(cx, RuleDraft::recompute_if_stale);
        let (and_mode, status, _color) = {
            let d = draft.read(cx);
            (
                d.and_mode,
                (d.match_total, d.error.clone()),
                d.color.clone(),
            )
        };

        dialog
            .title(
                rust_i18n::t!(if is_edit {
                    "rules.title_edit"
                } else {
                    "rules.title_new"
                })
                .to_string(),
            )
            .width(px(680.))
            .child(render_body(&draft, and_mode, status, cx))
            .on_ok({
                let draft = draft.clone();
                move |_, _, cx| save_draft(&draft, cx)
            })
    });
}

/// Persist the draft: create a smart collection or update the edited one's
/// tree. Returning `false` keeps the dialog open with the error shown.
fn save_draft(draft: &Entity<RuleDraft>, cx: &mut App) -> bool {
    let fail = |msg: String, draft: &Entity<RuleDraft>, cx: &mut App| {
        draft.update(cx, |d, cx| {
            d.error = Some(msg);
            cx.notify();
        });
        false
    };

    let name = {
        let d = draft.read(cx);
        d.name_input.read(cx).value().trim().to_string()
    };
    if name.is_empty() {
        return fail(rust_i18n::t!("rules.name_required").to_string(), draft, cx);
    }
    let json = match draft.read(cx).build_json(cx) {
        Ok(json) => json,
        Err(msg) => return fail(msg, draft, cx),
    };

    let outcome = draft.update(cx, |d, cx| {
        let conn = d.controller.read(cx).library.store().conn();
        match d.editing {
            Some(id) => smart_collections::update_query(conn, id, &json, d.color.as_deref())
                .map(|_| id)
                .map_err(|e| e.to_string()),
            None => {
                let input = trove_core::model::NewSmartCollection {
                    name: name.clone(),
                    query: json,
                    color: d.color.clone(),
                    position: smart_collections::list(conn).map(|l| l.len()).unwrap_or(0)
                        as i64,
                };
                input
                    .validate()
                    .map_err(|e| e.to_string())?;
                smart_collections::create(conn, &input)
                    .map(|created| created.id)
                    .map_err(|e| e.to_string())
            }
        }
    });

    match outcome {
        Ok(_) => {
            draft.update(cx, |d, cx| {
                d.controller.update(cx, |ctl, cx| {
                    ctl.generation += 1;
                    cx.notify();
                });
            });
            true
        }
        Err(e) => fail(e.to_string(), draft, cx),
    }
}

// ============================ rendering ======================================

fn render_body(
    draft: &Entity<RuleDraft>,
    and_mode: bool,
    status: (Option<u64>, Option<String>),
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let name_input = draft.read(cx).name_input.clone();

    let mut body = v_flex()
        .gap_3()
        .w_full()
        .child(
            v_flex()
                .gap_1()
                .child(field_label(cx, "rules.name"))
                .child(Input::new(&name_input).small().appearance(true)),
        )
        .child(
            h_flex()
                .gap_1()
                .child({
                    let d = draft.clone();
                    Button::new("mode-and")
                        .xsmall()
                        .when(and_mode, |b| b.primary())
                        .when(!and_mode, |b| b.ghost())
                        .label(t("rules.match_all"))
                        .on_click(move |_, _, cx| {
                            d.update(cx, |d, cx| d.set_and_mode(true, cx));
                        })
                })
                .child({
                    let d = draft.clone();
                    Button::new("mode-or")
                        .xsmall()
                        .when(!and_mode, |b| b.primary())
                        .when(and_mode, |b| b.ghost())
                        .label(t("rules.match_any"))
                        .on_click(move |_, _, cx| {
                            d.update(cx, |d, cx| d.set_and_mode(false, cx));
                        })
                }),
        )
        .child({
            let (hue, sat, lig, color) = {
                let d = draft.read(cx);
                (d.hue.clone(), d.sat.clone(), d.lig.clone(), d.color.clone())
            };
            let hex = color.clone();
            v_flex()
                .gap_1p5()
                .child(field_label(cx, "rules.color"))
                // Current pick + hex, right-click copies the value (same as
                // the Inspector swatches).
                .child({
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(match &color {
                            Some(hex) => color_swatch(
                                cx,
                                "pick-preview".into(),
                                hex,
                                false,
                                |_, _, _| {},
                            )
                            .on_mouse_down(gpui::MouseButton::Right, {
                                let hex = hex.clone();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(
                                        gpui::ClipboardItem::new_string(hex.clone()),
                                    );
                                }
                            }),
                            None => div()
                                .id("pick-none")
                                .size_5()
                                .rounded_full()
                                .border_1()
                                .border_color(cx.theme().border),
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(hex.unwrap_or_else(|| "—".into())),
                        )
                })
                .child(slider_row("rules.hue", &hue, cx))
                .child(slider_row("rules.saturation", &sat, cx))
                .child(slider_row("rules.lightness", &lig, cx))
                .child(color_swatch_row(draft, color, cx))
        })
        .child(field_label(cx, "rules.conditions"));

    let rows: Vec<(usize, SmartField, SmartCompare)> = {
        let d = draft.read(cx);
        d.rows
            .iter()
            .enumerate()
            .map(|(ix, row)| (ix, row.field, row.op))
            .collect()
    };
    for (ix, field, op) in rows {
        body = body.child(render_row(draft, ix, field, op, cx));
    }

    body = body.child(
        h_flex()
            .items_center()
            .gap_2()
            .child({
                let d = draft.clone();
                Button::new("add-condition")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Plus)
                    .label(t("rules.add_condition"))
                    .on_click(move |_, window, cx| {
                        d.update(cx, |d, cx| d.add_row(SmartField::Text, window, cx));
                    })
            })
            .child(match status {
                (Some(total), None) => div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("rules.match_count", count = total).to_string()),
                (_, Some(err)) => div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(err),
                (None, None) => div(),
            }),
    );

    body
}

fn render_row(
    draft: &Entity<RuleDraft>,
    ix: usize,
    field: SmartField,
    op: SmartCompare,
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let (kind, favorite, tag, rating, tag_names, text_input) = {
        let d = draft.read(cx);
        let row = &d.rows[ix];
        (
            row.kind,
            row.favorite,
            row.tag.clone(),
            row.rating,
            d.tag_names.clone(),
            row.text.clone(),
        )
    };

    let mut row_el = h_flex()
        .items_center()
        .gap_1()
        // Field picker.
        .child(dropdown_button(
            format!("row-{ix}-field"),
            t(field_key(field)),
            [
                (SmartField::Text, "rules.f_text"),
                (SmartField::Kind, "rules.f_kind"),
                (SmartField::Rating, "rules.f_rating"),
                (SmartField::Tag, "rules.f_tag"),
                (SmartField::IsFavorite, "rules.f_favorite"),
                (SmartField::Extension, "rules.f_extension"),
                (SmartField::SizeBytes, "rules.f_size"),
                (SmartField::Color, "rules.f_color"),
            ]
            .map(|(f, key)| (f, t(key)))
            .into_iter()
            .collect(),
            field,
            {
                let d = draft.clone();
                move |picked: SmartField, cx| d.update(cx, |d, cx| d.set_field(ix, picked, cx))
            },
        ));

    // Operator: fixed label for eq-only fields, dropdown otherwise.
    let ops = allowed_ops(field);
    if ops.len() == 1 {
        row_el = row_el.child(
            div()
                .w(px(72.))
                .text_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t(op_key(field, op))),
        );
    } else {
        row_el = row_el.child(dropdown_button(
            format!("row-{ix}-op"),
            t(op_key(field, op)),
            ops.iter()
                .map(|&op| (op, t(op_key(field, op))))
                .collect(),
            op,
            {
                let d = draft.clone();
                move |picked: SmartCompare, cx| d.update(cx, |d, cx| d.set_op(ix, picked, cx))
            },
        ));
    }

    // Value widget per field.
    let value: Div = match field {
        SmartField::Text | SmartField::Extension | SmartField::Color | SmartField::SizeBytes => {
            h_flex()
                .flex_1()
                .min_w_0()
                .child(Input::new(&text_input).small().appearance(true))
        }
        SmartField::Kind => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{ix}-kind"),
            t(kind_key(kind)),
            [
                AssetKind::Image,
                AssetKind::Video,
                AssetKind::Audio,
                AssetKind::Document,
                AssetKind::Archive,
                AssetKind::Font,
                AssetKind::Other,
            ]
            .map(|k| (k, t(kind_key(k))))
            .into_iter()
            .collect(),
            kind,
            {
                let d = draft.clone();
                move |picked: AssetKind, cx| d.update(cx, |d, cx| d.set_kind(ix, picked, cx))
            },
        )),
        SmartField::IsFavorite => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{ix}-fav"),
            t(if favorite { "rules.yes" } else { "rules.no" }),
            vec![(true, t("rules.yes")), (false, t("rules.no"))],
            favorite,
            {
                let d = draft.clone();
                move |picked: bool, cx| d.update(cx, |d, cx| d.set_favorite(ix, picked, cx))
            },
        )),
        SmartField::Tag => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{ix}-tag"),
            if tag.is_empty() {
                t("rules.has_tag")
            } else {
                tag.clone()
            },
            tag_names.into_iter().map(|n| (n.clone(), n)).collect(),
            tag,
            {
                let d = draft.clone();
                move |picked: String, cx| d.update(cx, |d, cx| d.set_tag(ix, picked, cx))
            },
        )),
        SmartField::Rating => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{ix}-rating"),
            format!("{rating} ★"),
            (0..=5u8).map(|n| (n, format!("{n} ★"))).collect(),
            rating,
            {
                let d = draft.clone();
                move |picked: u8, cx| d.update(cx, |d, cx| d.set_rating(ix, picked, cx))
            },
        )),
    };
    row_el = row_el.child(value);

    row_el.child({
        let d = draft.clone();
        Button::new(format!("row-{ix}-remove"))
            .xsmall()
            .ghost()
            .icon(IconName::Close)
            .on_click(move |_, _, cx| d.update(cx, |d, cx| d.remove_row(ix, cx)))
    })
}

/// Curated palette for smart-collection colors.
const COLOR_PALETTE: &[&str] = &[
    "#ef4444", "#f97316", "#eab308", "#84cc16", "#22c55e", "#14b8a6", "#06b6d4", "#3b82f6",
    "#6366f1", "#a855f7", "#ec4899", "#f43f5e", "#78716c", "#57534e", "#1f2937", "#0f172a",
];

fn slider_row(
    label_key: &'static str,
    state: &Entity<SliderState>,
    cx: &App,
) -> Div {
    h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(36.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!(label_key).to_string()),
        )
        .child(Slider::new(state).horizontal().flex_1())
}

fn color_swatch_row(draft: &Entity<RuleDraft>, color: Option<String>, cx: &App) -> Div {
    let mut row = h_flex().flex_wrap().gap_1().child({
        // "No color" clears the accent.
        let d = draft.clone();
        let selected = color.is_none();
        div()
            .id("swatch-none")
            .cursor_pointer()
            .size_5()
            .rounded_full()
            .border_1()
            .border_color(if selected {
                cx.theme().foreground
            } else {
                cx.theme().border
            })
            .when(selected, |this| this.border_2())
            .on_click(move |_, window, cx| {
                d.update(cx, |d, cx| d.set_color(None, window, cx));
            })
    });
    for hex in COLOR_PALETTE {
        let d = draft.clone();
        let hex = hex.to_string();
        let selected = color.as_deref() == Some(hex.as_str());
        let id = format!("swatch-{hex}");
        let hex_arg = hex.clone();
        row = row.child(color_swatch(
            cx,
            id,
            &hex_arg,
            selected,
            move |_, window, cx| {
                d.update(cx, |d, cx| d.set_color(Some(hex.clone()), window, cx));
            },
        ));
    }
    row
}

// ---- small widget helpers ---------------------------------------------------

/// A button whose dropdown lets the user pick one of `options`
/// (`(value, label)`); the current value is check-marked, `on_pick` fires
/// with the picked value.
fn dropdown_button<T: PartialEq + Clone + 'static>(
    id: String,
    label: String,
    options: Vec<(T, String)>,
    current: T,
    on_pick: impl Fn(T, &mut App) + Clone + 'static,
) -> impl IntoElement {
    Button::new(id)
        .xsmall()
        .outline()
        .label(label)
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
            let mut menu = menu.min_w(px(150.));
            for (value, option_label) in &options {
                let value = value.clone();
                let checked = value == current;
                let on_pick = on_pick.clone();
                menu = menu.item(
                    PopupMenuItem::new(option_label.clone())
                        .checked(checked)
                        .on_click(move |_, _, cx| on_pick(value.clone(), cx)),
                );
            }
            menu
        })
}

fn field_label(cx: &App, key: &'static str) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(rust_i18n::t!(key).to_string())
}

fn field_key(field: SmartField) -> &'static str {
    match field {
        SmartField::Rating => "rules.f_rating",
        SmartField::Kind => "rules.f_kind",
        SmartField::Text => "rules.f_text",
        SmartField::Tag => "rules.f_tag",
        SmartField::IsFavorite => "rules.f_favorite",
        SmartField::Extension => "rules.f_extension",
        SmartField::SizeBytes => "rules.f_size",
        SmartField::Color => "rules.f_color",
    }
}

fn kind_key(kind: AssetKind) -> &'static str {
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

fn op_key(field: SmartField, op: SmartCompare) -> &'static str {
    match (field, op) {
        (SmartField::Text, _) => "rules.op_match",
        (SmartField::Tag, SmartCompare::Eq) => "rules.has_tag",
        (SmartField::Tag, _) => "rules.no_tag",
        (_, SmartCompare::Eq) => "rules.op_eq",
        (_, SmartCompare::Ne) => "rules.op_ne",
        (_, SmartCompare::Gt) => "rules.op_gt",
        (_, SmartCompare::Gte) => "rules.op_gte",
        (_, SmartCompare::Lt) => "rules.op_lt",
        (_, SmartCompare::Lte) => "rules.op_lte",
    }
}

fn allowed_ops(field: SmartField) -> &'static [SmartCompare] {
    match field {
        SmartField::Text => &[SmartCompare::Eq],
        SmartField::Tag
        | SmartField::Kind
        | SmartField::IsFavorite
        | SmartField::Color
        | SmartField::Extension => &[SmartCompare::Eq, SmartCompare::Ne],
        SmartField::Rating | SmartField::SizeBytes => &[
            SmartCompare::Eq,
            SmartCompare::Ne,
            SmartCompare::Gt,
            SmartCompare::Gte,
            SmartCompare::Lt,
            SmartCompare::Lte,
        ],
    }
}
