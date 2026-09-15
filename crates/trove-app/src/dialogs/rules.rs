//! Smart-collection rule editor: a dialog that builds the JSON condition
//! tree stored in a smart collection.
//!
//! The editor covers a two-layer shape: N condition groups, each holding M
//! match conditions. Every group picks its own combination
//! ([`RuleGroup::and_mode`]) and the groups combine under a switchable
//! top-level operator (match any group / match all groups). A single group
//! is stored flat — `{"op": …, "children": [matches…]}`, exactly the shape
//! earlier one-group versions wrote — so existing rules round-trip.
//! Deeper nesting is out of scope: [`split_tree`] keeps only match leaves
//! below the second layer, and the editor itself never produces such trees.
//!
//! Draft state lives in a [`RuleDraft`] entity created before the dialog
//! opens (the dialog's content closure is a re-run-per-frame `Fn` and
//! cannot own state). Every mutation bumps [`RuleDraft::revision`]; the
//! dialog recomputes the live match count whenever the revision moved.
//!
//! The accent color is picked with the gpui-kit base [`ColorPickerState`]
//! (hex field + HSL sliders kept in sync by the component itself); the
//! swatch row is built from base [`ColorSwatch`]s over a curated palette.

use gpui::{Hsla, Rgba};
use gpui_kit::base::{ColorPickerEvent, ColorPickerState, ColorSwatch};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, SmartCollection, SmartCompare, SmartField, SmartNode};
use trove_core::store::{smart, smart_collections, tags};
use uuid::Uuid;

use crate::library::LibraryController;

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

/// A group of conditions with its own combination operator.
struct RuleGroup {
    /// `true` = all conditions in this group must match, `false` = any.
    and_mode: bool,
    rows: Vec<ConditionRow>,
}

/// The editor's draft: group layout, condition rows and the live match status.
struct RuleDraft {
    controller: Entity<LibraryController>,
    /// `Some(id)` while editing an existing smart collection.
    editing: Option<Uuid>,
    /// Where a *new* smart collection is created: another smart collection or
    /// a regular collection, `None` for the top level. Ignored while editing
    /// — [`open_rule_editor`] never moves the edited row.
    parent: Option<Uuid>,
    name_input: Entity<InputState>,
    /// How groups combine: `false` = match any group (or), `true` = match
    /// all groups (and). Meaningless while only one group exists.
    inter_and_mode: bool,
    groups: Vec<RuleGroup>,
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

    fn add_row(
        &mut self,
        gix: usize,
        field: SmartField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.groups.get_mut(gix) {
            group.rows.push(ConditionRow::new(window, cx, field));
        }
        self.touch(cx);
    }

    fn remove_row(&mut self, gix: usize, ix: usize, cx: &mut Context<Self>) {
        if let Some(group) = self.groups.get_mut(gix)
            && ix < group.rows.len()
        {
            group.rows.remove(ix);
        }
        self.touch(cx);
    }

    fn add_group(&mut self, cx: &mut Context<Self>) {
        self.groups.push(RuleGroup {
            and_mode: true,
            rows: Vec::new(),
        });
        self.touch(cx);
    }

    /// Removes a group; the last remaining group is kept so the editor never
    /// ends up with nothing to edit.
    fn remove_group(&mut self, gix: usize, cx: &mut Context<Self>) {
        if self.groups.len() > 1 && gix < self.groups.len() {
            self.groups.remove(gix);
        }
        self.touch(cx);
    }

    fn set_group_and_mode(&mut self, gix: usize, and_mode: bool, cx: &mut Context<Self>) {
        if let Some(group) = self.groups.get_mut(gix) {
            group.and_mode = and_mode;
        }
        self.touch(cx);
    }

    fn set_inter_and_mode(&mut self, inter_and_mode: bool, cx: &mut Context<Self>) {
        self.inter_and_mode = inter_and_mode;
        self.touch(cx);
    }

    fn set_field(&mut self, gix: usize, ix: usize, field: SmartField, cx: &mut Context<Self>) {
        // Pulled out first: borrowing the row mutably would conflict with
        // reading `tag_names` below.
        let default_tag = self.tag_names.first().cloned().unwrap_or_default();
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix))
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

    fn set_op(&mut self, gix: usize, ix: usize, op: SmartCompare, cx: &mut Context<Self>) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
            row.op = op;
        }
        self.touch(cx);
    }

    fn set_kind(&mut self, gix: usize, ix: usize, kind: AssetKind, cx: &mut Context<Self>) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
            row.kind = kind;
        }
        self.touch(cx);
    }

    fn set_favorite(&mut self, gix: usize, ix: usize, favorite: bool, cx: &mut Context<Self>) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
            row.favorite = favorite;
        }
        self.touch(cx);
    }

    fn set_tag(&mut self, gix: usize, ix: usize, tag: String, cx: &mut Context<Self>) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
            row.tag = tag;
        }
        self.touch(cx);
    }

    fn set_rating(&mut self, gix: usize, ix: usize, rating: u8, cx: &mut Context<Self>) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
            row.rating = rating;
        }
        self.touch(cx);
    }

    fn set_orientation(
        &mut self,
        gix: usize,
        ix: usize,
        orientation: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.groups.get_mut(gix).and_then(|g| g.rows.get_mut(ix)) {
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
    /// skipped; an empty result is an error the dialog surfaces.
    fn build_json(&self, cx: &App) -> Result<serde_json::Value, String> {
        let t = |k: &str| rust_i18n::t!(k).to_string();
        let mut groups: Vec<(bool, Vec<SmartNode>)> = Vec::new();
        for group in &self.groups {
            let mut children = Vec::new();
            for row in &group.rows {
                if !row.is_complete(cx) {
                    continue;
                }
                children.push(SmartNode::Match {
                    field: row.field,
                    op: row.op,
                    value: row_value(row, cx)?,
                });
            }
            if !children.is_empty() {
                groups.push((group.and_mode, children));
            }
        }
        if groups.is_empty() {
            return Err(t("rules.need_condition"));
        }
        serde_json::to_value(join_tree(self.inter_and_mode, &groups))
            .map_err(|_| t("rules.match_error"))
    }

    /// Re-run the live match count when the draft moved since last draw.
    fn recompute_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.revision == self.evaluated {
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

// ============================ tree ⇄ groups (pure) ===========================

/// Splits a stored tree into `(groups combine with and, groups)`. The top
/// level is `and`/`or`; each child group keeps its own operator. Matches
/// sitting directly at the top level collapse into one group with the
/// top-level operator — that is the flat shape earlier one-group versions
/// wrote, and it must come back as one group. A bare match becomes a
/// single-condition `and` group. Non-match nodes below the second layer
/// cannot be surfaced by the flat editor and are dropped (the editor itself
/// never produces such trees).
fn split_tree(node: SmartNode) -> (bool, Vec<(bool, Vec<SmartNode>)>) {
    let (and_mode, children) = match node {
        SmartNode::And { children } => (true, children),
        SmartNode::Or { children } => (false, children),
        match_node @ SmartNode::Match { .. } => {
            return (true, vec![(true, vec![match_node])]);
        }
    };
    let mut top_level: Vec<SmartNode> = Vec::new();
    let mut groups: Vec<(bool, Vec<SmartNode>)> = Vec::new();
    for child in children {
        match child {
            SmartNode::And { children } => groups.push((true, keep_matches(children))),
            SmartNode::Or { children } => groups.push((false, keep_matches(children))),
            match_node @ SmartNode::Match { .. } => top_level.push(match_node),
        }
    }
    if !top_level.is_empty() {
        groups.insert(0, (and_mode, top_level));
    }
    (and_mode, groups)
}

fn keep_matches(children: Vec<SmartNode>) -> Vec<SmartNode> {
    children
        .into_iter()
        .filter(|child| matches!(child, SmartNode::Match { .. }))
        .collect()
}

/// Joins groups back into a tree. A single group is stored flat (the
/// top-level operator would be redundant); multiple groups are wrapped in
/// `inter_and`'s operator.
fn join_tree(inter_and: bool, groups: &[(bool, Vec<SmartNode>)]) -> SmartNode {
    let wrap = |and_mode: bool, children: Vec<SmartNode>| {
        if and_mode {
            SmartNode::And { children }
        } else {
            SmartNode::Or { children }
        }
    };
    if groups.len() == 1 {
        let (and_mode, children) = &groups[0];
        return wrap(*and_mode, children.clone());
    }
    let children = groups
        .iter()
        .map(|(and_mode, children)| wrap(*and_mode, children.clone()))
        .collect();
    wrap(inter_and, children)
}

// ============================ color helpers ==================================

/// Curated palette for smart-collection colors.
const COLOR_PALETTE: [u32; 16] = [
    0xef4444, 0xf97316, 0xeab308, 0x84cc16, 0x22c55e, 0x14b8a6, 0x06b6d4, 0x3b82f6, 0x6366f1,
    0xa855f7, 0xec4899, 0xf43f5e, 0x78716c, 0x57534e, 0x1f2937, 0x0f172a,
];

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

/// Load a stored tree into `(inter-group and mode, groups)`. Groups whose
/// children all vanish (non-match nodes below layer two) are kept as empty
/// groups so the structure stays visible; a tree with no groups at all
/// falls back to one empty `and` group.
fn load_groups(
    json: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) -> (bool, Vec<RuleGroup>) {
    let node = smart::node_from_json(json).unwrap_or(SmartNode::And {
        children: Vec::new(),
    });
    let (inter_and, layout) = split_tree(node);
    let mut groups: Vec<RuleGroup> = layout
        .into_iter()
        .map(|(and_mode, matches)| RuleGroup {
            and_mode,
            rows: matches
                .into_iter()
                .filter_map(|node| match_row(node, window, cx))
                .collect(),
        })
        .collect();
    if groups.is_empty() {
        groups.push(RuleGroup {
            and_mode: true,
            rows: Vec::new(),
        });
    }
    (inter_and, groups)
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
    let (inter_and_mode, groups) = match &editing {
        Some(sc) => load_groups(&sc.query, window, cx),
        None => (
            true,
            vec![RuleGroup {
                and_mode: true,
                rows: Vec::new(),
            }],
        ),
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
        inter_and_mode,
        groups,
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
            .width(px(680.))
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
    let (name_input, inter_and_mode, group_count) = {
        let d = draft.read(cx);
        (d.name_input.clone(), d.inter_and_mode, d.groups.len())
    };

    let mut body = v_flex()
        .gap_3()
        .w_full()
        .child(
            v_flex()
                .gap_1()
                .child(field_label(cx, "rules.name"))
                .child(Input::new(&name_input).small().appearance(true)),
        )
        .child(render_color_section(draft, cx))
        .child(field_label(cx, "rules.conditions"));

    for gix in 0..group_count {
        if gix == 1 {
            body = body.child(render_inter_separator(draft, inter_and_mode, cx));
        }
        let (and_mode, rows): (bool, Vec<(usize, SmartField, SmartCompare)>) = {
            let d = draft.read(cx);
            let group = &d.groups[gix];
            (
                group.and_mode,
                group
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(ix, row)| (ix, row.field, row.op))
                    .collect(),
            )
        };
        body = body.child(render_group(
            draft,
            gix,
            and_mode,
            rows,
            group_count > 1,
            cx,
        ));
    }

    body.child(
        h_flex()
            .items_center()
            .gap_2()
            .child({
                let d = draft.clone();
                Button::new("add-group")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Plus)
                    .label(t("rules.add_group"))
                    .on_click(move |_, _, cx| d.update(cx, |d, cx| d.add_group(cx)))
            })
            .child(match status {
                (Some(total), None) => div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("rules.match_count", count = total).to_string()),
                (_, Some(err)) => div().text_xs().text_color(cx.theme().danger).child(err),
                (None, None) => div(),
            }),
    )
}

/// Accent color: preview swatch + editable hex field + three HSL sliders +
/// curated swatch row, all synced through the picker state.
fn render_color_section(draft: &Entity<RuleDraft>, cx: &mut App) -> Div {
    let (picker, color) = {
        let d = draft.read(cx);
        (d.picker.clone(), d.color.clone())
    };
    let displayed = picker.read(cx).displayed_color();
    let hex_input = picker.read(cx).hex_input().clone();
    let sliders = picker.read(cx).sliders().clone();

    v_flex()
        .gap_1p5()
        .child(field_label(cx, "rules.color"))
        // Current pick, editable as hex; right-click copies the value (same
        // as the Inspector swatches).
        .child(
            h_flex()
                .items_center()
                .gap_1p5()
                .child(match displayed {
                    Some(c) => div()
                        .id("pick-preview")
                        .size_5()
                        .rounded_full()
                        .bg(c)
                        .border_1()
                        .border_color(cx.theme().border)
                        .on_mouse_down(gpui::MouseButton::Right, {
                            let hex = color.clone();
                            move |_, _, cx| {
                                if let Some(hex) = &hex {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        hex.clone(),
                                    ));
                                }
                            }
                        }),
                    None => div()
                        .id("pick-none")
                        .size_5()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().border),
                })
                .child(Input::new(&hex_input).small().w(px(92.)).appearance(true)),
        )
        .child(slider_row("rules.hue", sliders.hue(), cx))
        .child(slider_row("rules.saturation", sliders.saturation(), cx))
        .child(slider_row("rules.lightness", sliders.lightness(), cx))
        .child(color_swatches_row(draft, color, cx))
}

/// The separator between condition groups: two rules with the switchable
/// inter-group operator in the middle. Only rendered with 2+ groups.
fn render_inter_separator(draft: &Entity<RuleDraft>, inter_and: bool, cx: &mut App) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let d = draft.clone();
    h_flex()
        .items_center()
        .gap_2()
        .child(div().flex_1().h(px(1.)).bg(cx.theme().border))
        .child(dropdown_button(
            "inter-mode",
            t(if inter_and {
                "rules.inter_all"
            } else {
                "rules.inter_any"
            }),
            vec![(false, t("rules.inter_any")), (true, t("rules.inter_all"))],
            inter_and,
            move |picked: bool, cx| d.update(cx, |d, cx| d.set_inter_and_mode(picked, cx)),
        ))
        .child(div().flex_1().h(px(1.)).bg(cx.theme().border))
}

fn render_group(
    draft: &Entity<RuleDraft>,
    gix: usize,
    and_mode: bool,
    rows: Vec<(usize, SmartField, SmartCompare)>,
    show_remove: bool,
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let mut header = h_flex()
        .items_center()
        .gap_1()
        .child({
            let d = draft.clone();
            Button::new(format!("group-{gix}-mode-and"))
                .xsmall()
                .when(and_mode, |b| b.primary())
                .when(!and_mode, |b| b.ghost())
                .label(t("rules.match_all"))
                .on_click(move |_, _, cx| d.update(cx, |d, cx| d.set_group_and_mode(gix, true, cx)))
        })
        .child({
            let d = draft.clone();
            Button::new(format!("group-{gix}-mode-or"))
                .xsmall()
                .when(!and_mode, |b| b.primary())
                .when(and_mode, |b| b.ghost())
                .label(t("rules.match_any"))
                .on_click(move |_, _, cx| {
                    d.update(cx, |d, cx| d.set_group_and_mode(gix, false, cx))
                })
        });
    if show_remove {
        header = header.child(div().flex_1()).child({
            let d = draft.clone();
            Button::new(format!("group-{gix}-remove"))
                .xsmall()
                .ghost()
                .icon(IconName::Close)
                .on_click(move |_, _, cx| d.update(cx, |d, cx| d.remove_group(gix, cx)))
        });
    }

    let mut group = v_flex()
        .gap_1p5()
        .p_2()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius)
        .child(header);
    for (ix, field, op) in rows {
        group = group.child(render_row(draft, gix, ix, field, op, cx));
    }
    group.child({
        let d = draft.clone();
        Button::new(format!("group-{gix}-add"))
            .xsmall()
            .ghost()
            .icon(IconName::Plus)
            .label(t("rules.add_condition"))
            .on_click(move |_, window, cx| {
                d.update(cx, |d, cx| d.add_row(gix, SmartField::Text, window, cx));
            })
    })
}

fn render_row(
    draft: &Entity<RuleDraft>,
    gix: usize,
    ix: usize,
    field: SmartField,
    op: SmartCompare,
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let (kind, favorite, tag, rating, orientation, tag_names, text_input) = {
        let d = draft.read(cx);
        let row = &d.groups[gix].rows[ix];
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
            format!("row-{gix}-{ix}-field"),
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
                move |picked: SmartField, cx| d.update(cx, |d, cx| d.set_field(gix, ix, picked, cx))
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
            format!("row-{gix}-{ix}-op"),
            t(op_key(field, op)),
            ops.iter().map(|&op| (op, t(op_key(field, op)))).collect(),
            op,
            {
                let d = draft.clone();
                move |picked: SmartCompare, cx| d.update(cx, |d, cx| d.set_op(gix, ix, picked, cx))
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
            format!("row-{gix}-{ix}-kind"),
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
                move |picked: AssetKind, cx| d.update(cx, |d, cx| d.set_kind(gix, ix, picked, cx))
            },
        )),
        SmartField::IsFavorite => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{gix}-{ix}-fav"),
            t(if favorite { "rules.yes" } else { "rules.no" }),
            vec![(true, t("rules.yes")), (false, t("rules.no"))],
            favorite,
            {
                let d = draft.clone();
                move |picked: bool, cx| d.update(cx, |d, cx| d.set_favorite(gix, ix, picked, cx))
            },
        )),
        SmartField::Tag => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{gix}-{ix}-tag"),
            if tag.is_empty() {
                t("rules.has_tag")
            } else {
                tag.clone()
            },
            tag_names.into_iter().map(|n| (n.clone(), n)).collect(),
            tag,
            {
                let d = draft.clone();
                move |picked: String, cx| d.update(cx, |d, cx| d.set_tag(gix, ix, picked, cx))
            },
        )),
        SmartField::Orientation => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{gix}-{ix}-orientation"),
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
                move |picked: String, cx| {
                    d.update(cx, |d, cx| d.set_orientation(gix, ix, picked, cx))
                }
            },
        )),
        SmartField::Rating => h_flex().flex_1().min_w_0().child(dropdown_button(
            format!("row-{gix}-{ix}-rating"),
            format!("{rating} ★"),
            (0..=5u8).map(|n| (n, format!("{n} ★"))).collect(),
            rating,
            {
                let d = draft.clone();
                move |picked: u8, cx| d.update(cx, |d, cx| d.set_rating(gix, ix, picked, cx))
            },
        )),
    };
    row_el = row_el.child(value);

    row_el.child({
        let d = draft.clone();
        Button::new(format!("row-{gix}-{ix}-remove"))
            .xsmall()
            .ghost()
            .icon(IconName::Close)
            .on_click(move |_, _, cx| d.update(cx, |d, cx| d.remove_row(gix, ix, cx)))
    })
}

/// Curated palette rendered as gpui-kit base swatches (radio semantics,
/// accessible hex names).
fn color_swatches_row(draft: &Entity<RuleDraft>, color: Option<String>, cx: &App) -> Div {
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
    for (ix, c) in COLOR_PALETTE.iter().enumerate() {
        let hex = format!("#{c:06x}");
        let selected = color.as_deref() == Some(hex.as_str());
        let d = draft.clone();
        let hex_click = hex.clone();
        row = row.child(
            ColorSwatch::new(("swatch", ix), u32_to_hsla(*c))
                .selected(selected)
                .h_5()
                .w_5()
                .rounded_full()
                .bg(u32_to_hsla(*c))
                .border_1()
                .border_color(cx.theme().border)
                .when(selected, |this| this.border_2())
                .on_click(move |_, _, window, cx| {
                    d.update(cx, |d, cx| d.set_color(Some(hex_click.clone()), window, cx));
                }),
        );
    }
    row
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

fn slider_row(label_key: &'static str, state: &Entity<SliderState>, cx: &App) -> Div {
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

    fn round_trip(node: SmartNode) -> (bool, Vec<(bool, Vec<SmartNode>)>) {
        let json = serde_json::to_value(&node).unwrap();
        split_tree(smart::node_from_json(&json).unwrap())
    }

    #[test]
    fn bare_match_loads_as_single_and_group() {
        let (inter_and, groups) = split_tree(m(SmartField::Rating, 4));
        assert!(inter_and);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].0);
        assert_eq!(groups[0].1.len(), 1);
    }

    #[test]
    fn single_flat_group_round_trips() {
        let groups = [(true, vec![m(SmartField::Rating, 4), m(SmartField::Kind, 1)])];
        let (inter, loaded) = round_trip(join_tree(true, &groups));
        assert!(loaded.len() == 1 && loaded[0].0);
        assert_eq!(loaded[0].1.len(), 2);
        // A single group carries no inter-group semantics.
        assert!(inter);
    }

    #[test]
    fn or_of_ands_round_trips() {
        let groups = [
            (
                true,
                vec![m(SmartField::Rating, 4), m(SmartField::IsFavorite, 1)],
            ),
            (true, vec![m(SmartField::Kind, 2)]),
        ];
        let json = serde_json::to_value(join_tree(false, &groups)).unwrap();
        // Top level must be `or`; each branch its own `and`.
        assert_eq!(json["op"], "or");
        assert_eq!(json["children"][0]["op"], "and");
        assert_eq!(json["children"][1]["op"], "and");

        let (inter, loaded) = split_tree(smart::node_from_json(&json).unwrap());
        assert!(!inter);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|(and_mode, _)| *and_mode));
        assert_eq!(loaded[0].1.len(), 2);
        assert_eq!(loaded[1].1.len(), 1);
    }

    #[test]
    fn inter_all_wraps_groups_in_and() {
        let groups = [
            (false, vec![m(SmartField::Kind, 2)]),
            (false, vec![m(SmartField::Rating, 3)]),
        ];
        let json = serde_json::to_value(join_tree(true, &groups)).unwrap();
        assert_eq!(json["op"], "and");
        assert_eq!(json["children"][0]["op"], "or");

        let (inter, loaded) = split_tree(smart::node_from_json(&json).unwrap());
        assert!(inter);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|(and_mode, _)| !*and_mode));
    }

    #[test]
    fn mixed_group_modes_survive_a_round_trip() {
        let groups = [
            (true, vec![m(SmartField::Rating, 4)]),
            (false, vec![m(SmartField::Kind, 1), m(SmartField::Kind, 3)]),
            (true, vec![m(SmartField::IsFavorite, 1)]),
        ];
        let (inter, loaded) = round_trip(join_tree(false, &groups));
        assert!(!inter);
        let modes: Vec<bool> = loaded.iter().map(|(and_mode, _)| *and_mode).collect();
        assert_eq!(modes, vec![true, false, true]);
        let sizes: Vec<usize> = loaded.iter().map(|(_, rows)| rows.len()).collect();
        assert_eq!(sizes, vec![1, 2, 1]);
    }

    #[test]
    fn non_match_nodes_below_layer_two_are_dropped() {
        // Or[ And[ Or[m] ] ] — the innermost Or cannot be surfaced.
        let node = SmartNode::Or {
            children: vec![SmartNode::And {
                children: vec![SmartNode::Or {
                    children: vec![m(SmartField::Rating, 4)],
                }],
            }],
        };
        let (_, loaded) = split_tree(node);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].1.is_empty(), "the unsurfable node is dropped");
    }
}
