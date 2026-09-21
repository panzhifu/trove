//! Collections panel: the folder tree.
//!
//! * Rows show the name on the left and the asset count on the right.
//! * "All assets" is a pseudo-collection row without a management menu.
//! * The "+" button appends an inline editor after the last row.
//! * Right-click a collection: New collection inside / Rename (the row
//!   becomes a prefilled editor) / Delete. Enter confirms inline edits.
//! * Smart collections nest: rows are indented by depth, the section "+"
//!   creates inside the smart collection being browsed, and a row can be
//!   dragged onto another one (or onto the section header, to un-nest it).

use std::collections::{HashMap, HashSet};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, NewCollection};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::search_box::SearchBox;

use super::common::{
    AssetsDrag, CollectionDrag, SmartDrag, hex_to_rgb, live_count, observe_controller,
    separator_label, trash_count,
};

// ============================================================================
// Layout metrics
// ============================================================================

/// Left padding (`px_2`) of a top-level row; nested rows add one step per
/// level.
const ROW_PAD: f32 = 8.;
/// Nesting step: matches the collection rows' second level (`pl(px(22.))`).
const ROW_INDENT: f32 = 14.;

/// Extra trailing padding a section-header action needs so that its right
/// edge lands on the same line as the `title_suffix` action in the dock's
/// title bar. The dock wraps that suffix in a `px_2` box and keeps a `gap_1`
/// before the (empty) toolbar slot, putting it 12px from the panel's right
/// edge; the header already sits in the panel's own `p_1` column, so 8px of
/// it remain to be added.
const HEADER_ACTION_PAD: f32 = 8.;

// ============================================================================
// Counts
// ============================================================================

/// Live (non-trashed) entry count of the recently-viewed history.
fn recent_count(ctl: &LibraryController) -> u64 {
    trove_core::store::view_history::live_count(ctl.library.store().conn()).unwrap_or(0)
}

/// Live (non-trashed) font-asset count for the fonts pseudo-row.
fn fonts_count(ctl: &LibraryController) -> u64 {
    assets::query(
        ctl.library.store().conn(),
        &trove_core::model::AssetQuery {
            kind: Some(AssetKind::Font),
            ..Default::default()
        },
    )
    .map(|page| page.total)
    .unwrap_or(0)
}

// ============================================================================
// Render-time snapshots
// ============================================================================

/// What the single inline editor is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    None,
    /// New collection. `parent`: browse a folder? + adds under it; a
    /// right-click "New collection inside" sets it to that folder.
    Adding {
        parent: Option<Uuid>,
    },
    Renaming(Uuid),
    RenamingSmart(Uuid),
}

/// A managed collection row, flattened for rendering.
#[derive(Clone)]
struct CollectionRow {
    id: Uuid,
    name: String,
    is_root: bool,
    count: u64,
}

/// A smart collection row, flattened for rendering.
#[derive(Clone)]
struct SmartRow {
    id: Uuid,
    name: String,
    count: u64,
    accent: Option<u32>,
    /// Nesting level below the section's own top level (0 = top).
    depth: usize,
}

/// Nesting depth of every entry, in display order, as `(index, depth)` pairs.
///
/// An entry nests under the entry it points at; every other parent — the top
/// level, an id that is not in `entries` at all (a regular collection, which
/// this section does not list), a damaged parent chain — starts a root here.
/// Siblings keep their relative order, which is the store's `position` order.
fn nest_order(entries: &[(Uuid, Option<Uuid>)]) -> Vec<(usize, usize)> {
    let ids: HashSet<Uuid> = entries.iter().map(|(id, _)| *id).collect();
    let mut children: HashMap<Uuid, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (ix, (_, parent)) in entries.iter().enumerate() {
        match parent {
            Some(parent) if ids.contains(parent) => children.entry(*parent).or_default().push(ix),
            _ => roots.push(ix),
        }
    }

    let mut order: Vec<(usize, usize)> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|ix| (ix, 0)).collect();
    while let Some((ix, depth)) = stack.pop() {
        let id = entries[ix].0;
        // Only reachable through a parent chain damaged into a cycle: without
        // this the walk would revisit it forever.
        if !seen.insert(id) {
            continue;
        }
        for child in children.get(&id).into_iter().flatten().rev() {
            stack.push((*child, depth + 1));
        }
        order.push((ix, depth));
    }

    // A cycle leaves its members out of every root, and this section is the
    // only place smart collections are listed — so list them too, flat,
    // rather than silently dropping them.
    for (ix, (id, _)) in entries.iter().enumerate() {
        if seen.insert(*id) {
            order.push((ix, 0));
        }
    }
    order
}

/// Flatten the saved searches into indented rows.
fn flat_smart_rows(ctl: &LibraryController) -> Vec<SmartRow> {
    let conn = ctl.library.store().conn();
    let text_index = ctl.library.text_index();
    let all = smart_collections::list(conn).unwrap_or_default();
    let entries: Vec<(Uuid, Option<Uuid>)> = all.iter().map(|sc| (sc.id, sc.parent_id)).collect();

    nest_order(&entries)
        .into_iter()
        .map(|(ix, depth)| {
            let sc = &all[ix];
            let count = trove_core::store::smart::node_from_json(&sc.query)
                .ok()
                .and_then(|node| {
                    trove_core::store::smart::evaluate(conn, Some(text_index), &node, None, 0)
                        .ok()
                        .map(|page| page.total)
                })
                .unwrap_or(0);
            SmartRow {
                id: sc.id,
                name: sc.name.clone(),
                count,
                accent: sc.color.as_deref().and_then(hex_to_rgb),
                depth,
            }
        })
        .collect()
}

/// Everything the rows need, taken in one borrow of the controller so the
/// `read` guard ends before we build elements with `&mut cx`. Cached per
/// controller generation: every input (rows, counts) changes only when the
/// generation moves, so repeated renders between mutations reuse the last
/// snapshot instead of re-running the COUNT queries.
#[derive(Clone)]
struct Snapshot {
    current: Option<Uuid>,
    showing_trash: bool,
    showing_recent: bool,
    active_smart: Option<Uuid>,
    all_count: u64,
    trash_total: u64,
    recent_total: u64,
    fonts_total: u64,
    filter_kind: Option<AssetKind>,
    rows: Vec<CollectionRow>,
    smart_rows: Vec<SmartRow>,
}

impl Snapshot {
    fn take(ctl: &LibraryController) -> Self {
        let conn = ctl.library.store().conn();

        let mut rows: Vec<CollectionRow> = Vec::new();
        if let Ok(roots) = collections::roots(conn) {
            for root in roots {
                rows.push(CollectionRow {
                    id: root.id,
                    name: root.name.clone(),
                    is_root: true,
                    count: collections::count_assets(conn, root.id).unwrap_or(0),
                });
                if let Ok(children) = collections::children_of(conn, Some(root.id)) {
                    for child in children {
                        rows.push(CollectionRow {
                            id: child.id,
                            name: child.name.clone(),
                            is_root: false,
                            count: collections::count_assets(conn, child.id).unwrap_or(0),
                        });
                    }
                }
            }
        }

        let smart_rows = flat_smart_rows(ctl);

        Self {
            current: ctl.current_collection,
            showing_trash: ctl.showing_trash,
            showing_recent: ctl.showing_recent,
            active_smart: ctl.active_smart,
            all_count: live_count(ctl),
            trash_total: trash_count(ctl),
            recent_total: recent_count(ctl),
            fonts_total: fonts_count(ctl),
            filter_kind: ctl.filter_kind,
            rows,
            smart_rows,
        }
    }

    /// "All assets" highlights when nothing specific is selected.
    fn all_selected(&self) -> bool {
        self.current.is_none()
            && !self.showing_trash
            && !self.showing_recent
            && self.filter_kind != Some(AssetKind::Font)
    }

    /// The fonts row highlights only when the fonts kind filter is active.
    fn fonts_selected(&self) -> bool {
        self.current.is_none()
            && !self.showing_trash
            && !self.showing_recent
            && self.filter_kind == Some(AssetKind::Font)
    }
}

// ============================================================================
// Panel state
// ============================================================================

pub struct ExplorerPanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    /// Reused inline editor for add / rename.
    editor_input: Entity<InputState>,
    mode: EditorMode,
    /// Row/count snapshot keyed by the controller generation it was taken
    /// at, plus the kind filter the kind highlight reads. Filter changes
    /// that do not touch the kind (favorite, shape, rating, format) leave
    /// the snapshot valid — the counts behind it never read them.
    snapshot_cache: Option<(u64, Option<AssetKind>, Snapshot)>,
    /// The magnifier in the title bar. Each panel owns one, so the search
    /// entry point is never more than a glance away — wherever the user has
    /// dragged the dock to.
    search_box: Entity<SearchBox>,
}

impl ExplorerPanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let editor_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
        });
        let search_box = cx.new(|cx| SearchBox::new(window, cx, controller.clone(), "explorer"));
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            editor_input,
            mode: EditorMode::None,
            snapshot_cache: None,
            search_box,
        };
        observe_controller(cx, &this.controller);
        this.subscribe_enter(window, cx);
        this
    }

    fn subscribe_enter(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor_input.clone();
        cx.subscribe_in(&editor, window, |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_editor(cx);
            }
        })
        .detach();
    }

    /// Open the editor, cleared and focused, for a fresh collection.
    fn open_add(&mut self, parent: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.mode = EditorMode::Adding { parent };
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// "+" clicked: create at the end, under the browsed collection when any.
    fn begin_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.controller.read(cx).current_collection;
        self.open_add(parent, window, cx);
    }

    /// Right-click → "New collection inside": immediately editable.
    fn add_inside(&mut self, parent: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.open_add(Some(parent), window, cx);
    }

    /// Right-click → Rename on a smart collection: same inline editor.
    pub fn begin_rename_smart(
        &mut self,
        id: Uuid,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.mode = EditorMode::RenamingSmart(id);
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Right-click → Rename: the row becomes a prefilled, focused editor.
    fn begin_rename(
        &mut self,
        id: Uuid,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.mode = EditorMode::Renaming(id);
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor_input.clone();
        editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Esc in the inline add/rename editor: drop the editor without
    /// committing. The input's own Escape handler propagates the key, so
    /// this fires only while the editor holds focus inside this panel.
    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == EditorMode::None {
            return;
        }
        self.mode = EditorMode::None;
        self.editor_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Enter pressed in the editor: create (with its parent) or rename.
    fn submit_editor(&mut self, cx: &mut Context<Self>) {
        let name = self.editor_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }

        match self.mode {
            EditorMode::Adding { parent } => {
                let controller = self.controller.clone();
                controller.update(cx, |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    // Append at the end of the target level.
                    let position = match parent {
                        Some(pid) => collections::children_of(conn, Some(pid))
                            .map(|c| c.len() as i64)
                            .unwrap_or(0),
                        None => collections::roots(conn)
                            .map(|r| r.len() as i64)
                            .unwrap_or(0),
                    };
                    if let Ok(collection) = collections::create(
                        conn,
                        &NewCollection {
                            parent_id: parent,
                            name,
                            position,
                        },
                    ) {
                        ctl.select_collection(Some(collection.id));
                    }
                    cx.notify();
                });
            }
            EditorMode::Renaming(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let _ = ctl.library.rename_collection(id, &name);
                    cx.notify();
                });
            }
            EditorMode::RenamingSmart(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let _ = ctl.library.rename_smart_collection(id, &name);
                    cx.notify();
                });
            }
            EditorMode::None => return,
        }
        self.mode = EditorMode::None;
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — smart collection, collection (with its parent
    /// prefix when nested), trash, recently viewed, or the all-assets
    /// fallback.
    fn title_label(&self, cx: &Context<Self>) -> String {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if ctl.showing_recent {
            return rust_i18n::t!("app.recent_viewed").to_string();
        }
        if ctl.filter_kind == Some(AssetKind::Font) {
            return rust_i18n::t!("app.fonts_view").to_string();
        }
        if let Some(sid) = ctl.active_smart
            && let Ok(Some(sc)) = smart_collections::get(conn, sid)
        {
            return sc.name;
        }
        if let Some(cid) = ctl.current_collection
            && let Ok(Some(c)) = collections::get(conn, cid)
        {
            if let Some(pid) = c.parent_id
                && let Ok(Some(p)) = collections::get(conn, pid)
            {
                return format!("{} / {}", p.name, c.name);
            }
            return c.name;
        }
        // The favorites toggle turns the "all assets" view into the
        // favorites view; named views keep their names.
        if ctl.filter_favorite {
            return rust_i18n::t!("workspace.title_favorites").to_string();
        }
        rust_i18n::t!("app.all_assets").to_string()
    }
}

impl BasePanel for ExplorerPanel {
    fn panel_name(&self) -> &'static str {
        "ExplorerPanel"
    }
    fn closable(&self, _: &App) -> bool {
        false
    }
}

impl DockPanel for ExplorerPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .text_sm()
            .font_weight(FontWeight::BOLD)
            .text_color(cx.theme().foreground)
            .child(self.title_label(cx))
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    /// The magnifier plus the "+" pinned to the trailing edge of the title
    /// bar (where the removed ellipsis menu used to sit).
    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let entity = cx.entity();
        Some(
            h_flex()
                .items_center()
                .gap_1()
                .child(self.search_box.clone())
                .child(
                    plus_button(
                        "add-collection-title",
                        rust_i18n::t!("explorer.add_collection").to_string(),
                    )
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| this.begin_add(window, cx));
                    }),
                ),
        )
    }
}

impl EventEmitter<PanelEvent> for ExplorerPanel {}

impl Focusable for ExplorerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplorerPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let explorer = cx.entity();
        // Reuse the cached snapshot while the generation and the kind filter
        // are unchanged; the COUNT queries behind it only need to re-run
        // after a mutation (or a kind change, which the highlight reads).
        let ctl = self.controller.read(cx);
        let (generation, filter_kind) = (ctl.generation, ctl.filter_kind);
        let snapshot = match &self.snapshot_cache {
            Some((cached_gen, cached_kind, snap))
                if *cached_gen == generation && *cached_kind == filter_kind =>
            {
                snap.clone()
            }
            _ => {
                let snap = Snapshot::take(ctl);
                self.snapshot_cache = Some((generation, filter_kind, snap.clone()));
                snap
            }
        };
        let snap = snapshot;
        let mut items: Vec<AnyElement> = Vec::new();

        // --- Pseudo rows: All assets, Fonts, Recently viewed, Trash ---

        items.push(
            collection_row(
                cx,
                self.controller.clone(),
                None,
                rust_i18n::t!("app.all_assets").to_string(),
                snap.all_count,
                snap.all_selected(),
                true,
            )
            .into_any_element(),
        );

        // Fonts view: every live font asset (a kind-filtered all-assets
        // browse). Grid cells render live specimen cards.
        items.push(fonts_row(
            cx,
            self.controller.clone(),
            snap.fonts_total,
            snap.fonts_selected(),
        ));

        // Recently viewed: history count, click browses the view. Dropping
        // assets here sends them to the trash like any other view.
        items.push(recent_row(
            cx,
            self.controller.clone(),
            snap.recent_total,
            snap.showing_recent,
        ));

        items.push(trash_row(
            cx,
            self.controller.clone(),
            snap.trash_total,
            snap.showing_trash,
        ));

        // --- Managed collections ---

        let rename_target = match mode {
            EditorMode::Renaming(id) => Some(id),
            _ => None,
        };

        for row in &snap.rows {
            if rename_target == Some(row.id) {
                // The renamed row itself is replaced by the inline editor.
                items.push(editor_row(&self.editor_input, px(14.)));
                continue;
            }
            items.push(
                collection_row(
                    cx,
                    self.controller.clone(),
                    Some(row.id),
                    row.name.clone(),
                    row.count,
                    snap.current == Some(row.id) && !snap.showing_trash,
                    row.is_root,
                )
                .context_menu({
                    let explorer = explorer.clone();
                    let id = row.id;
                    let name = row.name.clone();
                    move |menu, window, cx| {
                        collection_menu(menu, window, cx, &explorer, id, name.clone())
                    }
                })
                .into_any_element(),
            );
        }

        if matches!(mode, EditorMode::Adding { .. }) {
            // Editor appears after the last collection row.
            items.push(editor_row(&self.editor_input, px(0.)));
        }

        // --- Smart collections ---

        items.push(smart_section_header(cx, self.controller.clone()));

        for row in &snap.smart_rows {
            let sid = row.id;
            let depth = row.depth;
            let nested = depth > 0;
            let controller = self.controller.clone();
            let drop_ctl = self.controller.clone();
            let menu_name = row.name.clone();
            let drag_name = row.name.clone();
            items.push(
                div()
                    .id(format!("smart-row-{sid}"))
                    .cursor_pointer()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    // The row stretches to the panel edge; the indent is
                    // padding, not a margin, so the count keeps its column.
                    .when(nested, |this| {
                        this.pl(px(ROW_PAD + ROW_INDENT * depth as f32))
                    })
                    .when(snap.active_smart == Some(sid), |this| {
                        this.bg(cx.theme().secondary)
                    })
                    .on_click(move |_ev: &ClickEvent, _window, cx| {
                        controller.update(cx, |ctl, _| ctl.select_smart(Some(sid)));
                    })
                    // Drag onto another row to nest under it; the store
                    // refuses cycles, so a self- or descendant-drop is
                    // reported rather than performed. Attached before the
                    // context menu: that wrapper only forwards children.
                    .on_drag(SmartDrag(sid), move |_, _, _, cx| {
                        let name = drag_name.clone();
                        cx.new(|_| SmartDragPreview { name })
                    })
                    .drag_over::<SmartDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                    .on_drop(move |payload: &SmartDrag, _window, cx| {
                        let dragged = payload.0;
                        if dragged != sid {
                            drop_ctl.update(cx, move |ctl, cx| {
                                reparent_smart(ctl, dragged, Some(sid), cx);
                            });
                        }
                    })
                    .context_menu({
                        let controller = self.controller.clone();
                        let explorer = explorer.clone();
                        let menu_name = menu_name.clone();
                        move |menu, window, cx| {
                            smart_menu(
                                menu,
                                window,
                                cx,
                                &controller,
                                &explorer,
                                SmartTarget {
                                    id: sid,
                                    name: menu_name.clone(),
                                    nested,
                                },
                            )
                        }
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .when_some(row.accent, |this, rgb| {
                                        this.text_color(gpui_kit::rgb(rgb))
                                    })
                                    .child(row.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(row.count.to_string()),
                            ),
                    )
                    .into_any_element(),
            );
        }

        v_flex()
            .size_full()
            .gap_1()
            .p_1()
            .key_context("Explorer")
            .on_action(
                cx.listener(|this, _: &crate::app::actions::CancelEditor, window, cx| {
                    this.cancel_editor(window, cx);
                }),
            )
            .child(
                div()
                    .flex_1()
                    .child(v_flex().gap_0p5().children(items).w_full()),
            )
    }
}

// ============================================================================
// Reusable row builders
// ============================================================================

/// "+" button used both in the title bar and on the Smart section header.
fn plus_button(id: &'static str, tooltip: String) -> Button {
    Button::new(id).ghost().xsmall().label("+").tooltip(tooltip)
}

/// A full-width inline editor (add or rename).
fn editor_row(editor: &Entity<InputState>, indent: Pixels) -> AnyElement {
    h_flex()
        .w_full()
        .pl(indent)
        .px_1()
        .child(Input::new(editor).small())
        .into_any_element()
}

/// Skeleton for the non-managed pseudo-rows (fonts, recent, trash):
/// full-width, name on the left, count on the right.
fn pseudo_row(
    cx: &mut Context<ExplorerPanel>,
    row_id: &'static str,
    label: String,
    count: u64,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(row_id)
        .cursor_pointer()
        .w_full()
        .px_2()
        .py_1()
        .rounded(cx.theme().radius)
        .when(selected, |this| this.bg(cx.theme().secondary))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(count.to_string()),
                ),
        )
}

/// Dropping assets on the recent / trash rows sends them to the trash.
fn attach_trash_drop(row: Stateful<Div>, controller: Entity<LibraryController>) -> Stateful<Div> {
    row.drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
        .on_drop(move |payload: &AssetsDrag, _window, cx| {
            controller.update(cx, move |ctl, cx| {
                let _ = ctl.library.trash_assets(&payload.0);
                ctl.deselect(&payload.0);
                cx.notify();
            });
        })
}

/// Fonts view row: click switches the library to the fonts kind filter.
fn fonts_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    count: u64,
    selected: bool,
) -> AnyElement {
    pseudo_row(
        cx,
        "collection-row-fonts",
        rust_i18n::t!("app.fonts_view").to_string(),
        count,
        selected,
    )
    .on_click(move |_ev: &ClickEvent, _window, cx| {
        controller.update(cx, |ctl, _| ctl.select_fonts());
    })
    .into_any_element()
}

/// Recently viewed row.
fn recent_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    count: u64,
    selected: bool,
) -> AnyElement {
    let click = controller.clone();
    let row = pseudo_row(
        cx,
        "collection-row-recent",
        rust_i18n::t!("app.recent_viewed").to_string(),
        count,
        selected,
    )
    .on_click(move |_ev: &ClickEvent, _window, cx| {
        click.update(cx, |ctl, _| ctl.select_recent());
    });
    attach_trash_drop(row, controller).into_any_element()
}

/// Trash row.
fn trash_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    count: u64,
    selected: bool,
) -> AnyElement {
    let click = controller.clone();
    let row = pseudo_row(
        cx,
        "collection-row-trash",
        rust_i18n::t!("app.trash").to_string(),
        count,
        selected,
    )
    .on_click(move |_ev: &ClickEvent, _window, cx| {
        click.update(cx, |ctl, _| ctl.select_trash());
    });
    attach_trash_drop(row, controller).into_any_element()
}

/// Smart-collections section header: its own "+" (new rule editor) and a
/// drop target that pulls a nested smart collection back to the top level.
fn smart_section_header(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
) -> AnyElement {
    // The "+" lands inside the browsed smart collection, which the button
    // itself cannot show — so the tooltip changes with it.
    let tooltip = match controller.read(cx).active_smart {
        Some(_) => rust_i18n::t!("explorer.new_smart_inside"),
        None => rust_i18n::t!("rules.title_new"),
    }
    .to_string();
    let add_ctl = controller.clone();
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        // Lines the "+" up with the one in the panel title bar.
        .pr(px(HEADER_ACTION_PAD))
        .drag_over::<SmartDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
        .on_drop(move |payload: &SmartDrag, _window, cx| {
            let dragged = payload.0;
            controller.update(cx, move |ctl, cx| {
                reparent_smart(ctl, dragged, None, cx);
            });
        })
        .child(separator_label(
            cx,
            rust_i18n::t!("panel.smart").to_string(),
        ))
        .child(
            plus_button("add-smart-title", tooltip).on_click(move |_, window, cx| {
                // Same rule as the collections "+": the new row lands inside
                // whatever is browsed, which for this section is a smart
                // collection (nothing browsed → the top level).
                let parent = add_ctl.read(cx).active_smart;
                crate::dialogs::rules::open_rule_editor(window, cx, add_ctl.clone(), None, parent);
            }),
        )
        .into_any_element()
}

/// A single row in the collections list. `id == None` renders the
/// non-managed "All assets" pseudo-row.
fn collection_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    id: Option<Uuid>,
    name: String,
    count: u64,
    selected: bool,
    is_root: bool,
) -> Stateful<Div> {
    let row_id = id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "all".to_string());

    let mut row = div()
        .id(format!("collection-row-{row_id}"))
        .cursor_pointer()
        .w_full()
        .px_2()
        .py_1()
        .rounded(cx.theme().radius)
        .on_click({
            let controller = controller.clone();
            move |_ev: &ClickEvent, _window, cx| {
                controller.update(cx, move |ctl, _| ctl.select_collection(id));
            }
        })
        .child(
            h_flex()
                .w_full()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(name),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(count.to_string()),
                ),
        );

    if selected {
        row = row.bg(cx.theme().secondary);
    }
    if !is_root {
        // Same step as the smart rows' nesting indent.
        row = row.pl(px(ROW_PAD + ROW_INDENT));
    }

    match id {
        // Managed rows are drop targets for assets, and both drag sources
        // (reparent) and drop targets for other collections.
        Some(cid) => {
            let drop_assets = controller.clone();
            row = row
                .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                .on_drop(move |payload: &AssetsDrag, _window, cx| {
                    drop_assets.update(cx, move |ctl, cx| {
                        let _ = ctl.library.add_assets_to_collection(cid, &payload.0);
                        ctl.generation += 1;
                        cx.notify();
                    });
                });

            let move_ctl = controller.clone();
            row = row
                .on_drag(CollectionDrag(cid), |_, _, _, cx| {
                    cx.new(|_| CollectionDragPreview)
                })
                .drag_over::<CollectionDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                .on_drop(move |payload: &CollectionDrag, _window, cx| {
                    move_ctl.update(cx, move |ctl, cx| {
                        move_collection(ctl, payload.0, Some(cid), cx);
                    });
                });
        }
        // "All assets" row: dropping a collection here moves it back to the
        // root level.
        None => {
            let move_ctl = controller.clone();
            row = row
                .drag_over::<CollectionDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                .on_drop(move |payload: &CollectionDrag, _window, cx| {
                    move_ctl.update(cx, move |ctl, cx| {
                        move_collection(ctl, payload.0, None, cx);
                    });
                });
        }
    }

    row
}

/// Reparent `dragged` under `target` (or to the root when `None`), appending
/// at the end of the target level. Failures surface via `ctl.notice`.
fn move_collection(
    ctl: &mut LibraryController,
    dragged: Uuid,
    target: Option<Uuid>,
    cx: &mut Context<LibraryController>,
) {
    let conn = ctl.library.store().conn();
    let position = match target {
        Some(pid) => collections::children_of(conn, Some(pid))
            .map(|c| c.len() as i64)
            .unwrap_or(0),
        None => collections::roots(conn)
            .map(|r| r.len() as i64)
            .unwrap_or(0),
    };
    if let Err(e) = ctl.library.move_collection(dragged, target, position) {
        ctl.notice = Some(rust_i18n::t!("explorer.move_failed", error = e.to_string()).to_string());
    }
    ctl.generation += 1;
    cx.notify();
}

/// Reparent `dragged` under `target`, or back to the top level when `None`.
/// Siblings share one ordering space, so appending after the target's
/// existing smart children is their count. Cycles are refused by the store
/// and surface via `ctl.notice`.
fn reparent_smart(
    ctl: &mut LibraryController,
    dragged: Uuid,
    target: Option<Uuid>,
    cx: &mut Context<LibraryController>,
) {
    let conn = ctl.library.store().conn();
    let position = smart_collections::list(conn)
        .map(|all| all.iter().filter(|sc| sc.parent_id == target).count() as i64)
        .unwrap_or(0);
    if let Err(e) = ctl.library.move_smart_collection(dragged, target, position) {
        ctl.notice = Some(rust_i18n::t!("explorer.move_failed", error = e.to_string()).to_string());
    }
    ctl.generation += 1;
    cx.notify();
}

/// Drag ghost for a dragged collection row.
struct CollectionDragPreview;

impl Render for CollectionDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().primary)
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().primary_foreground)
            .child(Icon::new(IconName::Folder).size_4())
            .child(rust_i18n::t!("explorer.drag_collection").to_string())
    }
}

/// Drag ghost for a dragged smart-collection row: the row's own name, so a
/// drag into a shallow nested list still says which search is moving.
struct SmartDragPreview {
    name: String,
}

impl Render for SmartDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().primary)
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().primary_foreground)
            .child(Icon::new(IconName::Search).size_4())
            .child(self.name.clone())
    }
}

/// The row a smart-collection context menu was opened on. Bundled because the
/// menu needs all three and each handler owns its own copy.
struct SmartTarget {
    id: Uuid,
    name: String,
    /// Nested rows also get "Move to top level": a child is only reachable
    /// under its parent, so it needs a way out once it stops being one.
    nested: bool,
}

/// Right-click menu for a smart collection.
fn smart_menu(
    menu: PopupMenu,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    explorer: &Entity<ExplorerPanel>,
    target: SmartTarget,
) -> PopupMenu {
    let SmartTarget { id, name, nested } = target;
    let ctl_rename = explorer.clone();
    let ctl_edit = controller.clone();
    let ctl_delete = controller.clone();
    let ctl_inside = controller.clone();
    let ctl_top = controller.clone();
    menu.min_w(px(180.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.new_smart_inside").to_string()).on_click(
                move |_, window, cx| {
                    crate::dialogs::rules::open_rule_editor(
                        window,
                        cx,
                        ctl_inside.clone(),
                        None,
                        Some(id),
                    );
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.rename").to_string()).on_click(
                move |_, window, cx| {
                    ctl_rename.update(cx, |this, cx| {
                        this.begin_rename_smart(id, name.clone(), window, cx);
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("rules.edit_rule").to_string()).on_click(
                move |_, window, cx| {
                    let editing = ctl_edit.read(cx).library.store().conn();
                    let editing = smart_collections::get(editing, id).ok().flatten();
                    crate::dialogs::rules::open_rule_editor(
                        window,
                        cx,
                        ctl_edit.clone(),
                        editing,
                        None,
                    );
                },
            ),
        )
        .when(nested, |menu| {
            menu.separator().item(
                PopupMenuItem::new(rust_i18n::t!("explorer.move_to_top").to_string()).on_click(
                    move |_, _, cx| {
                        ctl_top.update(cx, move |ctl, cx| {
                            reparent_smart(ctl, id, None, cx);
                        });
                    },
                ),
            )
        })
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(
                move |_, _, cx| {
                    ctl_delete.update(cx, move |ctl, cx| {
                        let _ = ctl.library.delete_smart_collection(id);
                        if ctl.active_smart == Some(id) {
                            ctl.select_smart(None);
                        }
                        ctl.generation += 1;
                        cx.notify();
                    });
                },
            ),
        )
}

/// Right-click menu for a managed collection.
fn collection_menu(
    menu: PopupMenu,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
    explorer: &Entity<ExplorerPanel>,
    id: Uuid,
    name: String,
) -> PopupMenu {
    let explorer_new = explorer.clone();
    let explorer_rename = explorer.clone();
    let explorer_delete = explorer.clone();

    menu.min_w(px(180.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.new_collection_inside").to_string())
                .on_click(move |_, window, cx| {
                    explorer_new.update(cx, |this, cx| this.add_inside(id, window, cx));
                }),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.rename").to_string()).on_click(
                move |_, window, cx| {
                    explorer_rename.update(cx, |this, cx| {
                        this.begin_rename(id, name.clone(), window, cx);
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(
                move |_, _, cx| {
                    explorer_delete.update(cx, |this, cx| {
                        let ctl = this.controller.clone();
                        ctl.update(cx, |ctl, cx| {
                            let conn = ctl.library.store().conn();
                            let _ = collections::delete(conn, id);
                            if ctl.current_collection == Some(id) {
                                ctl.select_collection(None);
                            }
                            ctl.generation += 1;
                            cx.notify();
                        });
                        cx.notify();
                    });
                },
            ),
        )
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags the gpui prelude's
    // `test` attribute macro in, which makes expanding `#[test]` recurse.
    use super::nest_order;
    use uuid::Uuid;

    #[test]
    fn nesting_follows_parents_and_keeps_sibling_order() {
        let root = Uuid::from_u128(1);
        let child = Uuid::from_u128(2);
        let leaf = Uuid::from_u128(3);
        let sibling = Uuid::from_u128(4);
        let under_collection = Uuid::from_u128(5);
        let entries = vec![
            (root, None),
            (child, Some(root)),
            (leaf, Some(child)),
            (sibling, None),
            // Parent is a regular collection, which this section never lists:
            // the row must still show up, at the top level of the section.
            (under_collection, Some(Uuid::from_u128(99))),
        ];

        assert_eq!(
            nest_order(&entries),
            vec![(0, 0), (1, 1), (2, 2), (3, 0), (4, 0)],
            "depth follows the chain, unrelated parents start their own root"
        );
    }

    #[test]
    fn a_damaged_parent_cycle_still_lists_every_row() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let root = Uuid::from_u128(3);
        let child = Uuid::from_u128(4);
        // a and b point at each other, so neither is reachable from a root —
        // the walk has to end anyway and the rows must not disappear.
        let entries = vec![
            (a, Some(b)),
            (b, Some(a)),
            (root, None),
            (child, Some(root)),
        ];

        let order = nest_order(&entries);
        let listed: Vec<usize> = order.iter().map(|(ix, _)| *ix).collect();
        assert_eq!(order.len(), entries.len(), "no row is dropped or doubled");
        for ix in 0..entries.len() {
            assert_eq!(
                listed.iter().filter(|seen| **seen == ix).count(),
                1,
                "entry {ix} appears exactly once: {listed:?}"
            );
        }
        assert!(
            order.contains(&(3, 1)),
            "the healthy branch keeps its nesting: {order:?}"
        );
    }
}
