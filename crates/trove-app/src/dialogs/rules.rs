//! Smart-collection rule editor: a dialog that builds the JSON condition
//! tree stored in a smart collection.
//!
//! The editor covers a flat shape: one list of match conditions combined
//! under a single switchable operator (match all / match any), every row
//! free to negate (text *not contains*, tag *is not*, …). The stored JSON
//! is `{"op": "and"|"or", "children": [matches…]}`, which earlier
//! one-group versions also round-trip. Deeper nesting is out of scope:
//! [`split_tree`] rejects trees whose children are not all matches, and
//! such collections open in a read-only state instead of being mangled.
//!
//! Draft state lives in a [`RuleDraft`] entity created before the dialog
//! opens (the dialog's content closure is a re-run-per-frame `Fn` and
//! cannot own state). Every mutation bumps [`RuleDraft::revision`]; the
//! dialog recomputes the live match count whenever the revision moved.
//!
//! The accent color is picked with the gpui-kit base [`ColorPickerState`],
//! reusing the workspace colour filter's panels (`color_panel`): a
//! palette / HSLA tab pair over the shared picker state. The dialog is a
//! two-column layout — conditions on the left, the color column on the right.

use gpui::{Hsla, Rgba};
use gpui_kit::base::{ColorPickerEvent, ColorPickerState};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, SmartCollection, SmartCompare, SmartField, SmartNode};
use trove_core::store::{smart, smart_collections, tags};
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::workspace::{color_panel, recent_picker_colors};

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
    orientation: String,
}

impl ConditionRow {
    fn new(window: &mut Window, cx: &mut App, field: SmartField) -> Self {
        let placeholder = match field {
            SmartField::Color => rust_i18n::t!("rules.value_color_hint").to_string(),
            SmartField::SizeBytes => rust_i18n::t!("rules.value_bytes").to_string(),
            SmartField::Extension => "jpg, png…".to_string(),
            SmartField::CapturedAt => rust_i18n::t!("rules.value_date_hint").to_string(),
            SmartField::AspectRatio => rust_i18n::t!("rules.value_aspect_hint").to_string(),
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
            orientation: String::new(),
        }
    }

    /// A row is complete when its value side carries a usable value.
    fn is_complete(&self, cx: &App) -> bool {
        match self.field {
            SmartField::Text
            | SmartField::Extension
            | SmartField::Color
            | SmartField::SizeBytes
            | SmartField::CapturedAt
            | SmartField::AspectRatio => !self.text.read(cx).value().trim().is_empty(),
            SmartField::Tag => !self.tag.is_empty(),
            SmartField::Orientation => !self.orientation.is_empty(),
            SmartField::Kind | SmartField::IsFavorite | SmartField::Rating => true,
        }
    }
}

/// The editor's draft: the condition rows, the combination operator and
/// the live match status.
struct RuleDraft {
    controller: Entity<LibraryController>,
    /// `Some(id)` while editing an existing smart collection.
    editing: Option<Uuid>,
    /// Where a *new* smart collection is created: another smart collection or
    /// a regular collection, `None` for the top level. Ignored while editing
    /// — [`open_rule_editor`] never moves the edited row.
    parent: Option<Uuid>,
    name_input: Entity<InputState>,
    /// How the rows combine: `true` = all conditions must match (and),
    /// `false` = any (or).
    match_all: bool,
    rows: Vec<ConditionRow>,
    /// Set when the stored tree cannot be surfaced as a flat list (nested
    /// groups from older editors): rows stay empty, editing and saving are
    /// blocked, and the dialog explains why.
    readonly: bool,
    /// Tag names for the tag dropdown (snapshot at open).
    tag_names: Vec<String>,
    /// Display color of the smart collection itself (`#rrggbb` or none).
    color: Option<String>,
    /// gpui-kit picker state: owns the hex field and the HSL sliders and
    /// keeps them synced with [`Self::color`].
    picker: Entity<ColorPickerState>,
    /// Lives with the draft: when the dialog closes the draft (and these)
    /// drop, unsubscribing the picker.
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
        }
        self.touch(cx);
    }

    fn set_match_all(&mut self, match_all: bool, cx: &mut Context<Self>) {
        self.match_all = match_all;
        self.touch(cx);
    }

    fn set_field(&mut self, ix: usize, field: SmartField, cx: &mut Context<Self>) {
        // Pulled out first: borrowing the row mutably would conflict with
        // reading `tag_names` below.
        let default_tag = self.tag_names.first().cloned().unwrap_or_default();
        if let Some(row) = self.rows.get_mut(ix)
            && row.field != field
        {
            row.field = field;
            row.op = SmartCompare::Eq;
            row.kind = AssetKind::Image;
            row.favorite = true;
            row.tag = default_tag;
            row.rating = 3;
            row.orientation = String::new();
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

    fn set_orientation(&mut self, ix: usize, orientation: String, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.orientation = orientation;
        }
        self.touch(cx);
    }

    /// Set the color from a preset swatch (or clear it) and sync the picker
    /// state (hex field + sliders follow automatically).
    fn set_color(&mut self, color: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.color = color.clone();
        let hsla = color.as_deref().and_then(hex_to_hsla);
        self.picker.update(cx, |picker, cx| match hsla {
            Some(c) => picker.set_value(c, window, cx),
            None => picker.clear_value(window, cx),
        });
        self.touch(cx);
    }

    /// Compile the draft into the stored JSON tree. Incomplete rows are
    /// skipped; an empty result is an error the dialog surfaces. A
    /// read-only draft (unrepresentable stored tree) never compiles.
    fn build_json(&self, cx: &App) -> Result<serde_json::Value, String> {
        let t = |k: &str| rust_i18n::t!(k).to_string();
        if self.readonly {
            return Err(t("rules.readonly_complex"));
        }
        let children: Vec<SmartNode> = self
            .rows
            .iter()
            .filter(|row| row.is_complete(cx))
            .map(|row| {
                Ok(SmartNode::Match {
                    field: row.field,
                    op: row.op,
                    value: row_value(row, cx)?,
                })
            })
            .collect::<Result<Vec<SmartNode>, String>>()?;
        if children.is_empty() {
            return Err(t("rules.need_condition"));
        }
        serde_json::to_value(join_tree(self.match_all, children))
            .map_err(|_| t("rules.match_error"))
    }

    /// Re-run the live match count when the draft moved since last draw.
    fn recompute_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.revision == self.evaluated || self.readonly {
            return;
        }
        self.evaluated = self.revision;
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();
        let text_index = ctl.library.text_index();
        let outcome = self.build_json(cx).and_then(|json| {
            // The raw serde message is English internals; the localized
            // "invalid rule" label is enough for the live count status.
            let node = smart::node_from_json(&json)
                .map_err(|_| rust_i18n::t!("rules.match_error").to_string())?;
            smart::evaluate(conn, Some(text_index), &node, None, 0)
                .map(|page| page.total)
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

/// The stored value side of one condition row.
fn row_value(row: &ConditionRow, cx: &App) -> Result<serde_json::Value, String> {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    Ok(match row.field {
        SmartField::Text => serde_json::json!(row.text.read(cx).value().trim()),
        SmartField::Extension => {
            serde_json::json!(normalize_extension(row.text.read(cx).value().trim()))
        }
        SmartField::Color => match normalize_color(&row.text.read(cx).value()) {
            Some(hex) => serde_json::json!(hex),
            None => return Err(t("rules.invalid_color")),
        },
        SmartField::SizeBytes => match row.text.read(cx).value().trim().parse::<i64>() {
            Ok(n) if n > 0 => serde_json::json!(n),
            _ => return Err(t("rules.invalid_bytes")),
        },
        SmartField::CapturedAt => {
            let s = row.text.read(cx).value().trim().to_string();
            if smart::valid_date(&s) {
                serde_json::json!(s)
            } else {
                return Err(t("rules.invalid_date"));
            }
        }
        SmartField::AspectRatio => match row.text.read(cx).value().trim().parse::<f64>() {
            Ok(v) if v > 0.0 && v.is_finite() => serde_json::json!(v),
            _ => return Err(t("rules.invalid_aspect")),
        },
        SmartField::Orientation => serde_json::json!(row.orientation),
        SmartField::Tag => serde_json::json!(row.tag),
        SmartField::Rating => serde_json::json!(row.rating),
        SmartField::Kind => serde_json::json!(row.kind),
        SmartField::IsFavorite => serde_json::json!(row.favorite),
    })
}

// ============================ tree ⇄ rows (pure) ============================

/// Splits a stored tree into `(combine with and, matches)`. The top level
/// is `and`/`or`; every child must be a match leaf — anything nested
/// (groups inside groups from older two-layer editors) yields `Err`, which
/// the dialog turns into a read-only view. A bare match becomes a single
/// `and` condition.
fn split_tree(node: SmartNode) -> Result<(bool, Vec<SmartNode>), ()> {
    let (all, children) = match node {
        SmartNode::And { children } => (true, children),
        SmartNode::Or { children } => (false, children),
        match_node @ SmartNode::Match { .. } => return Ok((true, vec![match_node])),
    };
    if children
        .iter()
        .any(|c| !matches!(c, SmartNode::Match { .. }))
    {
        return Err(());
    }
    Ok((all, children))
}

/// Joins rows back into a tree: one `and`/`or` node over the matches.
fn join_tree(match_all: bool, matches: Vec<SmartNode>) -> SmartNode {
    if match_all {
        SmartNode::And { children: matches }
    } else {
        SmartNode::Or { children: matches }
    }
}

// ============================ color helpers ==================================

fn u32_to_hsla(n: u32) -> Hsla {
    Rgba {
        r: ((n >> 16) & 0xff) as f32 / 255.,
        g: ((n >> 8) & 0xff) as f32 / 255.,
        b: (n & 0xff) as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// `#rrggbb` → opaque [`Hsla`], for feeding the picker state.
fn hex_to_hsla(hex: &str) -> Option<Hsla> {
    let hex = normalize_color(hex)?;
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).ok()?;
    Some(u32_to_hsla(n))
}

/// [`Hsla`] → `#rrggbb` (lowercase). Alpha is dropped: smart-collection
/// colors have no transparency semantics.
fn picker_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value * 255.) as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    )
}

fn normalize_extension(raw: &str) -> String {
    raw.trim().trim_start_matches('.').to_lowercase()
}

fn normalize_color(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches('#').to_lowercase();
    (s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(format!("#{s}"))
}

// ============================ load ===========================================

/// Load a stored tree into `(combine with and, rows, read-only)`. A tree
/// the flat editor cannot surface (nested groups) opens read-only with no
/// rows; an empty tree falls back to no rows and editable.
fn load_rules(
    json: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> (bool, Vec<ConditionRow>, bool) {
    let node = smart::node_from_json(json).unwrap_or(SmartNode::And {
        children: Vec::new(),
    });
    match split_tree(node) {
        Ok((match_all, matches)) => {
            let rows = matches
                .into_iter()
                .filter_map(|node| match_row(node, window, cx))
                .collect();
            (match_all, rows, false)
        }
        Err(()) => (true, Vec::new(), true),
    }
}

/// Builds one condition row from a stored match node.
fn match_row(node: SmartNode, window: &mut Window, cx: &mut App) -> Option<ConditionRow> {
    let SmartNode::Match { field, op, value } = node else {
        return None;
    };
    let mut row = ConditionRow::new(window, cx, field);
    row.op = op;
    match field {
        SmartField::Text | SmartField::Extension | SmartField::Color => {
            let s = value.as_str().unwrap_or_default().to_string();
            row.text
                .update(cx, |state, cx| state.set_value(s, window, cx));
        }
        SmartField::SizeBytes => {
            let s = value.as_i64().map(|n| n.to_string()).unwrap_or_default();
            row.text
                .update(cx, |state, cx| state.set_value(s, window, cx));
        }
        SmartField::CapturedAt => {
            let s = value.as_str().unwrap_or_default().to_string();
            row.text
                .update(cx, |state, cx| state.set_value(s, window, cx));
        }
        SmartField::AspectRatio => {
            let s = value
                .as_f64()
                .map(|v| {
                    if v.fract() == 0.0 {
                        format!("{v:.0}")
                    } else {
                        format!("{v}")
                    }
                })
                .unwrap_or_default();
            row.text
                .update(cx, |state, cx| state.set_value(s, window, cx));
        }
        SmartField::Orientation => {
            row.orientation = value.as_str().unwrap_or_default().to_string();
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
    Some(row)
}

// ============================ the dialog =====================================

/// Open the rule editor: empty for a new smart collection, prefilled from
/// `editing`'s stored tree otherwise. `parent` places a new smart collection
/// under another one (or under a regular collection); it is ignored when
/// `editing` is `Some`.
pub fn open_rule_editor(
    window: &mut Window,
    cx: &mut App,
    controller: Entity<LibraryController>,
    editing: Option<SmartCollection>,
    parent: Option<Uuid>,
) {
    let is_edit = editing.is_some();
    // Editing never reparents; only a fresh collection has somewhere to land.
    let parent = if is_edit { None } else { parent };
    let name_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
    });
    if let Some(name) = editing.as_ref().map(|sc| sc.name.clone()) {
        name_input.update(cx, |state, cx| state.set_value(name, window, cx));
    }
    let (match_all, rows, readonly) = match &editing {
        Some(sc) => load_rules(&sc.query, window, cx),
        None => (true, Vec::new(), false),
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
    let init = color.as_deref().and_then(hex_to_hsla);
    let picker = cx.new(|cx| {
        let state = ColorPickerState::new(window, cx);
        match init {
            Some(hsla) => state.default_value(hsla),
            None => state,
        }
    });
    // Flush the builder-supplied value into the hex field and sliders.
    picker.update(cx, |picker, cx| picker.sync_pending_value(window, cx));
    let draft = cx.new(|_| RuleDraft {
        controller,
        editing: editing.map(|sc| sc.id),
        parent,
        name_input,
        match_all,
        rows,
        readonly,
        tag_names,
        color,
        picker: picker.clone(),
        _subs: Vec::new(),
        revision: 1,
        evaluated: 0,
        match_total: None,
        error: None,
    });

    // Picker → draft: the component keeps hex field and sliders in sync and
    // reports the committed color here. The color does not touch the rule
    // tree, so the revision is left alone.
    let d = draft.clone();
    let sub = cx.subscribe(&picker, move |_, event: &ColorPickerEvent, cx| {
        d.update(cx, |d, _| match event {
            ColorPickerEvent::Change(color) => d.color = color.map(picker_hex),
        });
    });
    draft.update(cx, |d, _| d._subs = vec![sub]);

    window.open_dialog(cx, move |dialog, _, cx| {
        draft.update(cx, RuleDraft::recompute_if_stale);
        let status = {
            let d = draft.read(cx);
            (d.match_total, d.error.clone())
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
            .width(px(760.))
            .child(render_body(&draft, status, cx))
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
                // Appended after its siblings, which share one ordering
                // space per parent.
                let position = smart_collections::list(conn)
                    .map(|all| all.iter().filter(|sc| sc.parent_id == d.parent).count())
                    .unwrap_or(0) as i64;
                let input = trove_core::model::NewSmartCollection {
                    parent_id: d.parent,
                    name: name.clone(),
                    query: json,
                    color: d.color.clone(),
                    position,
                };
                // Name checks live in the model; the condition tree is
                // validated where it compiles (store::smart).
                input.validate().map_err(|e| e.to_string())?;
                trove_core::store::smart::validate_json(&input.query).map_err(|e| e.to_string())?;
                smart_collections::create(conn, &input)
                    .map(|created| created.id)
                    .map_err(|e| e.to_string())
            }
        }
    });

    match outcome {
        Ok(saved) => {
            draft.update(cx, |d, cx| {
                let created = d.editing.is_none();
                d.controller.update(cx, |ctl, cx| {
                    // A fresh smart collection becomes the browsed one, the
                    // way the collections "+" selects what it just created;
                    // an edit leaves the current view alone.
                    if created {
                        ctl.select_smart(Some(saved));
                    }
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
    status: (Option<u64>, Option<String>),
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let (name_input, match_all, readonly, rows) = {
        let d = draft.read(cx);
        (
            d.name_input.clone(),
            d.match_all,
            d.readonly,
            d.rows
                .iter()
                .enumerate()
                .map(|(ix, row)| (ix, row.field, row.op))
                .collect::<Vec<_>>(),
        )
    };

    // Left column: the name, the condition rows and the live match status.
    let mut conditions = v_flex()
        .gap_3()
        .flex_1()
        .min_w_0()
        .child(
            v_flex()
                .gap_1()
                .child(field_label(cx, "rules.name"))
                .child(Input::new(&name_input).small().appearance(true)),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(field_label(cx, "rules.conditions"))
                // The single combination operator over all rows. Hidden
                // while read-only: the stored tree is not a flat list and
                // saving is blocked anyway.
                .when(!readonly, |header| {
                    let d = draft.clone();
                    header.child(dropdown_button(
                        "match-mode",
                        t(if match_all {
                            "rules.match_all"
                        } else {
                            "rules.match_any"
                        }),
                        vec![(true, t("rules.match_all")), (false, t("rules.match_any"))],
                        match_all,
                        move |picked: bool, cx| d.update(cx, |d, cx| d.set_match_all(picked, cx)),
                    ))
                }),
        );

    if readonly {
        conditions = conditions.child(
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(t("rules.readonly_complex")),
        );
    }
    for (ix, field, op) in rows {
        conditions = conditions.child(render_row(draft, ix, field, op, cx));
    }

    let mut footer = h_flex().items_center().gap_2();
    if !readonly {
        let d = draft.clone();
        footer = footer.child(
            Button::new("add-condition")
                .xsmall()
                .ghost()
                .icon(IconName::Plus)
                .label(t("rules.add_condition"))
                .on_click(move |_, window, cx| {
                    d.update(cx, |d, cx| d.add_row(SmartField::Text, window, cx));
                }),
        );
    }
    let conditions = conditions.child(
        footer.child(match status {
            (Some(total), None) => div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("rules.match_count", count = total).to_string()),
            (_, Some(err)) => div().text_xs().text_color(cx.theme().danger).child(err),
            (None, None) => div(),
        }),
    );

    // Two columns: conditions on the left, the color column on the right.
    v_flex().w_full().child(
        h_flex()
            .items_start()
            .gap_4()
            .child(conditions)
            .child(render_color_panel(draft, cx)),
    )
}

/// Right column: the accent color, rendered with the same panels as the
/// workspace colour filter (palette / HSLA tabs over the shared picker
/// state). A smart collection's color is optional, so a clear button sits
/// beside the label.
fn render_color_panel(draft: &Entity<RuleDraft>, cx: &mut App) -> Div {
    let picker = draft.read(cx).picker.clone();
    let featured = recent_picker_colors(cx);
    let d = draft.clone();

    v_flex()
        .w(px(300.))
        .flex_shrink_0()
        .gap_2()
        .border_l_1()
        .border_color(cx.theme().border)
        .pl_4()
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(field_label(cx, "rules.color"))
                .child(
                    Button::new("color-clear")
                        .xsmall()
                        .ghost()
                        .label(rust_i18n::t!("tags.no_color").to_string())
                        .on_click(move |_, window, cx| {
                            d.update(cx, |d, cx| d.set_color(None, window, cx));
                        }),
                ),
        )
        .child(color_panel(&picker, featured, cx))
}

fn render_row(
    draft: &Entity<RuleDraft>,
    ix: usize,
    field: SmartField,
    op: SmartCompare,
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let (kind, favorite, tag, rating, orientation, tag_names, text_input) = {
        let d = draft.read(cx);
        let row = &d.rows[ix];
        (
            row.kind,
            row.favorite,
            row.tag.clone(),
            row.rating,
            row.orientation.clone(),
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
                (SmartField::CapturedAt, "rules.f_captured"),
                (SmartField::AspectRatio, "rules.f_aspect"),
                (SmartField::Orientation, "rules.f_orientation"),
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
            ops.iter().map(|&op| (op, t(op_key(field, op)))).collect(),
            op,
            {
                let d = draft.clone();
                move |picked: SmartCompare, cx| d.update(cx, |d, cx| d.set_op(ix, picked, cx))
            },
        ));
    }

    // Value widget per field.
    let value: Div = match field {
        SmartField::Text
        | SmartField::Extension
        | SmartField::Color
        | SmartField::SizeBytes
        | SmartField::CapturedAt
        | SmartField::AspectRatio => h_flex()
            .flex_1()
            .min_w_0()
            .child(Input::new(&text_input).small().appearance(true)),
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
                AssetKind::Model,
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
        SmartField::Orientation => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{ix}-orientation"),
            if orientation.is_empty() {
                t("rules.f_orientation")
            } else {
                rust_i18n::t!(format!("rules.orient_{orientation}")).to_string()
            },
            ["landscape", "portrait", "square"]
                .iter()
                .map(|o| {
                    (
                        o.to_string(),
                        rust_i18n::t!(format!("rules.orient_{o}")).to_string(),
                    )
                })
                .collect(),
            orientation,
            {
                let d = draft.clone();
                move |picked: String, cx| d.update(cx, |d, cx| d.set_orientation(ix, picked, cx))
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

// ---- small widget helpers ---------------------------------------------------

/// A button whose dropdown lets the user pick one of `options`
/// (`(value, label)`); the current value is check-marked, `on_pick` fires
/// with the picked value.
fn dropdown_button<T: PartialEq + Clone + 'static>(
    id: impl Into<ElementId>,
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
        SmartField::CapturedAt => "rules.f_captured",
        SmartField::AspectRatio => "rules.f_aspect",
        SmartField::Orientation => "rules.f_orientation",
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
        AssetKind::Model => "asset.kind.model",
        AssetKind::Other => "asset.kind.other",
    }
}

fn op_key(field: SmartField, op: SmartCompare) -> &'static str {
    match (field, op) {
        (SmartField::Text, SmartCompare::Eq) => "rules.op_contains",
        (SmartField::Text, _) => "rules.op_excludes",
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
        SmartField::Text => &[SmartCompare::Eq, SmartCompare::Ne],
        SmartField::Tag
        | SmartField::Kind
        | SmartField::IsFavorite
        | SmartField::Color
        | SmartField::Orientation
        | SmartField::Extension => &[SmartCompare::Eq, SmartCompare::Ne],
        SmartField::Rating
        | SmartField::SizeBytes
        | SmartField::CapturedAt
        | SmartField::AspectRatio => &[
            SmartCompare::Eq,
            SmartCompare::Ne,
            SmartCompare::Gt,
            SmartCompare::Gte,
            SmartCompare::Lt,
            SmartCompare::Lte,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{join_tree, split_tree};
    use trove_core::model::{SmartCompare, SmartField, SmartNode};
    use trove_core::store::smart;

    fn m(field: SmartField, value: i64) -> SmartNode {
        SmartNode::Match {
            field,
            op: SmartCompare::Eq,
            value: serde_json::json!(value),
        }
    }

    fn round_trip(node: SmartNode) -> Result<(bool, Vec<SmartNode>), ()> {
        let json = serde_json::to_value(&node).unwrap();
        split_tree(smart::node_from_json(&json).unwrap())
    }

    #[test]
    fn bare_match_loads_as_single_and_condition() {
        let (match_all, rows) = split_tree(m(SmartField::Rating, 4)).unwrap();
        assert!(match_all);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn flat_list_round_trips() {
        for all in [true, false] {
            let json = serde_json::to_value(join_tree(
                all,
                vec![m(SmartField::Rating, 4), m(SmartField::Kind, 1)],
            ))
            .unwrap();
            assert_eq!(json["op"], if all { "and" } else { "or" });
            let (loaded_all, rows) = round_trip(smart::node_from_json(&json).unwrap()).unwrap();
            assert_eq!(loaded_all, all);
            assert_eq!(rows.len(), 2);
        }
    }

    #[test]
    fn nested_group_tree_is_rejected() {
        // Two-layer groups from older editors: Or[ And[ m ] ] — the inner
        // group cannot be surfaced as a flat list.
        let node = SmartNode::Or {
            children: vec![SmartNode::And {
                children: vec![m(SmartField::Rating, 4)],
            }],
        };
        assert!(round_trip(node).is_err());
    }

    #[test]
    fn any_nested_group_tree_is_rejected() {
        // Two-layer shapes from older editors, even when every leaf is a
        // match: the flat list cannot represent the inner operator, so the
        // tree opens read-only instead of being silently reshaped.
        for node in [
            SmartNode::Or {
                children: vec![SmartNode::And {
                    children: vec![m(SmartField::Rating, 4)],
                }],
            },
            SmartNode::Or {
                children: vec![SmartNode::And {
                    children: vec![m(SmartField::Rating, 4), m(SmartField::Kind, 1)],
                }],
            },
            SmartNode::And {
                children: vec![
                    m(SmartField::Rating, 4),
                    SmartNode::Or {
                        children: vec![m(SmartField::Kind, 1)],
                    },
                ],
            },
        ] {
            assert!(round_trip(node).is_err());
        }
    }
}
