//! Smart-collection rule editor: a dialog that builds the JSON condition
//! tree stored in a smart collection.
//!
//! The editor covers a flat shape: one list of match conditions combined
//! under a single switchable operator (match all / match any), every row
//! free to negate (text *not contains*, tag *is not*, …). The stored JSON
//! is `{"op": "and"|"or", "children": [matches…]}`, which earlier
//! one-group versions also round-trip. Deeper nesting is out of scope:
//! [`split_tree`] rejects trees whose children are not all matches, so
//! such trees load as an empty row list instead of being silently
//! reshaped (saving without conditions is blocked).
//!
//! Draft state lives in a [`RuleDraft`] entity created before the dialog
//! opens (the dialog's content closure is a re-run-per-frame `Fn` and
//! cannot own state). Every mutation bumps [`RuleDraft::revision`]; the
//! dialog recomputes the live match count whenever the revision moved.
//!
//! The right column is the folder's own look — the same
//! [`crate::panels::appearance::Picker`] a folder's context menu carries, so a
//! smart collection can be given its glyph and accent while it is still a
//! draft. Here it writes nothing: the host reads the chooser on save and persists
//! the look with the row. It replaced the free-form colour panel: a stored hex
//! could only ever be right in the theme it was picked in, see
//! [`trove_core::model::Appearance`]. The dialog is a two-column layout —
//! conditions on the left, the appearance column on the right.

use gpui_kit::base::{ColorPickerEvent, ColorPickerState, h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::ColorPicker;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Colorize as _, IconName, Sizable};
use gpui_kit::*;

use trove_core::model::{AssetKind, SmartCollection, SmartCompare, SmartField, SmartNode};
use trove_core::store::smart;
use uuid::Uuid;

use crate::components::controls::{self, muted_label};
use crate::components::scrollbar;
use crate::library::LibraryController;
use crate::panels::appearance;

// ============================ draft state ====================================

/// One condition row. `text` backs every free-text value (text, extension,
/// size); `color` carries the color condition's picker state; the other value
/// kinds keep their state inline.
struct ConditionRow {
    field: SmartField,
    op: SmartCompare,
    text: Entity<InputState>,
    color: Entity<ColorPickerState>,
    kind: AssetKind,
    favorite: bool,
    tag: String,
    rating: u8,
    orientation: String,
}

impl ConditionRow {
    fn new(window: &mut Window, cx: &mut App, field: SmartField) -> Self {
        let placeholder = match field {
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
            color: cx.new(|cx| ColorPickerState::new(window, cx)),
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
            SmartField::Color => self.color.read(cx).value().is_some(),
            SmartField::Text
            | SmartField::Extension
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
    /// Tag names for the tag dropdown (snapshot at open).
    tag_names: Vec<String>,
    /// The folder's glyph and accent, held by the shared chooser so this
    /// dialog and a folder's context menu cannot offer different looks for the
    /// same question. It is persisted with the row, not with the rule tree.
    chooser: Entity<appearance::Picker>,
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
        let row = ConditionRow::new(window, cx, field);
        // A picked colour bumps the revision, so the live match count keeps up.
        cx.subscribe(&row.color, |this, _, _: &ColorPickerEvent, cx| {
            this.touch(cx)
        })
        .detach();
        self.rows.push(row);
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

    /// Compile the draft into the stored rule tree. Incomplete rows are
    /// skipped; an empty result is an error the dialog surfaces.
    fn build_json(&self, cx: &App) -> Result<SmartNode, trove_core::Error> {
        let t = |k: &str| rust_i18n::t!(k).to_string();
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
            .collect::<Result<Vec<SmartNode>, trove_core::Error>>()?;
        if children.is_empty() {
            return Err(trove_core::Error::Message(t("rules.need_condition")));
        }
        Ok(join_tree(self.match_all, children))
    }

    /// Re-run the live match count when the draft moved since last draw.
    fn recompute_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.revision == self.evaluated {
            return;
        }
        self.evaluated = self.revision;
        let ctl = self.controller.read(cx);
        let outcome = self
            .build_json(cx)
            .map_err(|e| e.to_string())
            .and_then(|node| {
                ctl.library
                    .count_smart_rule(&node)
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
fn row_value(row: &ConditionRow, cx: &App) -> Result<serde_json::Value, trove_core::Error> {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    Ok(match row.field {
        SmartField::Text => serde_json::json!(row.text.read(cx).value().trim()),
        SmartField::Extension => {
            serde_json::json!(normalize_extension(row.text.read(cx).value().trim()))
        }
        SmartField::Color => match row.color.read(cx).value() {
            // The stored qualifier is an opaque sRGB hex; the picker's alpha
            // is dropped on the way out.
            Some(color) => serde_json::json!(Hsla { a: 1., ..color }.to_hex().to_lowercase()),
            None => return Err(trove_core::Error::Message(t("rules.invalid_color"))),
        },
        SmartField::SizeBytes => match row.text.read(cx).value().trim().parse::<i64>() {
            Ok(n) if n > 0 => serde_json::json!(n),
            _ => return Err(trove_core::Error::Message(t("rules.invalid_bytes"))),
        },
        SmartField::CapturedAt => {
            let s = row.text.read(cx).value().trim().to_string();
            if smart::valid_date(&s) {
                serde_json::json!(s)
            } else {
                return Err(trove_core::Error::Message(t("rules.invalid_date")));
            }
        }
        SmartField::AspectRatio => match row.text.read(cx).value().trim().parse::<f64>() {
            Ok(v) if v > 0.0 && v.is_finite() => serde_json::json!(v),
            _ => return Err(trove_core::Error::Message(t("rules.invalid_aspect"))),
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
/// the loader turns into an empty row list. A bare match becomes a single
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

// ============================ value normalisation ============================

fn normalize_extension(raw: &str) -> String {
    raw.trim().trim_start_matches('.').to_lowercase()
}

// ============================ load ===========================================

/// Load a stored tree into `(combine with and, rows)`. A tree the flat
/// editor cannot surface (nested groups) loads as no rows; an empty tree
/// also falls back to no rows.
fn load_rules(node: &SmartNode, window: &mut Window, cx: &mut App) -> (bool, Vec<ConditionRow>) {
    match split_tree(node.clone()) {
        Ok((match_all, matches)) => {
            let rows = matches
                .into_iter()
                .filter_map(|node| match_row(node, window, cx))
                .collect();
            (match_all, rows)
        }
        Err(()) => (true, Vec::new()),
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
        SmartField::Color => {
            // Seed the picker with the stored qualifier.
            if let Some(hex) = value.as_str()
                && let Ok(color) = Hsla::parse_hex(hex)
            {
                row.color
                    .update(cx, |state, cx| state.set_value(color, window, cx));
            }
        }
        SmartField::Text | SmartField::Extension => {
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
    let (match_all, rows) = match &editing {
        // A tree this build cannot read opens as no rows, exactly as a nested
        // group does — the record is not lost, it just cannot be edited here.
        Some(sc) => match sc.query.node() {
            Some(node) => load_rules(node, window, cx),
            None => (true, Vec::new()),
        },
        None => (true, Vec::new()),
    };
    let tag_names = {
        controller
            .read(cx)
            .library
            .list_tags()
            .map(|list| list.into_iter().map(|t| t.name).collect())
            .unwrap_or_default()
    };
    let chooser = appearance::Picker::draft(
        editing
            .as_ref()
            .map(|sc| sc.appearance.clone())
            .unwrap_or_default(),
        // A draft has no name of its own yet — the preview keeps the placeholder
        // rather than going stale against the name being typed.
        String::new(),
        cx,
    );
    let draft = cx.new(|cx| {
        let draft = RuleDraft {
            controller,
            editing: editing.map(|sc| sc.id),
            parent,
            name_input,
            match_all,
            rows,
            tag_names,
            chooser: chooser.clone(),
            revision: 1,
            evaluated: 0,
            match_total: None,
            error: None,
        };
        // Loaded rows' pickers feed the live match count too.
        for row in &draft.rows {
            cx.subscribe(
                &row.color,
                |this: &mut RuleDraft, _, _: &ColorPickerEvent, cx: &mut Context<RuleDraft>| {
                    this.touch(cx)
                },
            )
            .detach();
        }
        draft
    });

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
            .width(px(608.))
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
        Err(error) => return fail(error.to_string(), draft, cx),
    };

    let outcome = draft.update(cx, |d, cx| {
        match d.editing {
            Some(id) => d
                .controller
                .read(cx)
                .library
                .set_smart_collection_query(id, &json)
                .map(|_| id)
                .map_err(|e| e.to_string()),
            None => {
                // Appended after its siblings, which share one ordering
                // space per parent.
                let position = d
                    .controller
                    .read(cx)
                    .library
                    .list_smart_collections()
                    .map(|all| all.iter().filter(|sc| sc.parent_id == d.parent).count())
                    .unwrap_or(0) as i64;
                let input = trove_core::model::NewSmartCollection {
                    parent_id: d.parent,
                    name: name.clone(),
                    query: json,
                    position,
                };
                // The facade validates both halves of the input — the name in
                // the model, the condition tree where it compiles — so the
                // dialog does not repeat it.
                d.controller
                    .read(cx)
                    .library
                    .create_smart_collection(&input)
                    .map(|created| created.id)
                    .map_err(|e| e.to_string())
            }
        }
    });

    match outcome {
        Ok(saved) => {
            // The look rides along with the save but is written apart from the
            // rule tree: the two are edited in different places, and neither may
            // quietly rewrite the other.
            let appearance = draft.read(cx).chooser.read(cx).appearance().clone();
            if let Err(error) = appearance::Target::Smart(saved)
                .write(draft.read(cx).controller.read(cx), &appearance)
            {
                return fail(error.to_string(), draft, cx);
            }
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
    let (name_input, match_all, rows) = {
        let d = draft.read(cx);
        (
            d.name_input.clone(),
            d.match_all,
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
                .child(muted_label(rust_i18n::t!("rules.name").to_string(), cx))
                .child(Input::new(&name_input).small().appearance(true)),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(muted_label(
                    rust_i18n::t!("rules.conditions").to_string(),
                    cx,
                ))
                // The single combination operator over all rows.
                .child({
                    let d = draft.clone();
                    dropdown_button(
                        "match-mode",
                        t(if match_all {
                            "rules.match_all"
                        } else {
                            "rules.match_any"
                        }),
                        vec![(true, t("rules.match_all")), (false, t("rules.match_any"))],
                        match_all,
                        move |picked: bool, cx| d.update(cx, |d, cx| d.set_match_all(picked, cx)),
                    )
                }),
        );

    for (ix, field, op) in rows {
        conditions = conditions.child(render_row(draft, ix, field, op, cx));
    }

    let footer = {
        let d = draft.clone();
        h_flex().items_center().gap_2().child(
            Button::new("add-condition")
                .xsmall()
                .ghost()
                .icon(IconName::Plus)
                .label(t("rules.add_condition"))
                .on_click(move |_, window, cx| {
                    d.update(cx, |d, cx| d.add_row(SmartField::Text, window, cx));
                }),
        )
    };
    let conditions = conditions.child(footer.child(match status {
        (Some(total), None) => muted_label(
            rust_i18n::t!("rules.match_count", count = total).to_string(),
            cx,
        ),
        (_, Some(err)) => div().text_xs().text_color(cx.theme().danger).child(err),
        (None, None) => div(),
    }));

    // Two columns: conditions on the left, the color column on the right.
    v_flex().w_full().child(
        h_flex()
            .items_start()
            .gap_4()
            .child(scrollbar::vertical(
                div().flex_1().min_w_0().max_h(px(400.)).child(conditions),
            ))
            .child(appearance::column(&draft.read(cx).chooser, cx)),
    )
}

fn render_row(
    draft: &Entity<RuleDraft>,
    ix: usize,
    field: SmartField,
    op: SmartCompare,
    cx: &mut App,
) -> Div {
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let (kind, favorite, tag, rating, orientation, tag_names, text_input, color_picker) = {
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
            row.color.clone(),
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
        SmartField::Color => h_flex().flex_1().min_w_0().child(
            // The swatch opens the palette / slider panel; the picked colour
            // lands in the row's own picker state.
            ColorPicker::new(&color_picker).xsmall(),
        ),
        SmartField::Text
        | SmartField::Extension
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
            // The rule's own comparison value, so it is still a plain number and
            // not a `Rating`: "0 ★ or fewer" is a question a rule may ask. What it
            // can no longer answer is "which assets" -- a stored 0 folded onto
            // unrated in v22 -> v23, and no row can hold one any more, so this
            // choice now matches nothing. Typing the rule tree (`SmartNode`, the
            // plan's P5) is where that gets decided rather than left to a
            // dropdown.
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

/// The rules dialog's pickers, at the shared implementation's width and
/// anchor.
fn dropdown_button<T: PartialEq + Clone + 'static>(
    id: impl Into<ElementId>,
    label: String,
    options: Vec<(T, String)>,
    current: T,
    on_pick: impl Fn(T, &mut App) + Clone + 'static,
) -> impl IntoElement {
    controls::dropdown_button(id, label, options, current, on_pick, 150.0, Anchor::TopLeft)
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
        // tree is rejected instead of being silently reshaped.
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
