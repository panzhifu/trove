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
use std::ops::Range;

use gpui_kit::assets;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::NewCollection;
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::appearance;
use trove_core::model::Appearance;

use super::common::{
    AssetsDrag, CollectionDrag, SmartDrag, live_count, observe_controller, separator_label,
    trash_count,
};
use crate::components::controls::muted_label;
use crate::components::scrollbar::ScrollableElement as _;

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
    ctl.library.viewed_count().unwrap_or(0)
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
    appearance: Appearance,
}

/// A smart collection row, flattened for rendering.
#[derive(Clone)]
struct SmartRow {
    id: Uuid,
    name: String,
    count: u64,
    appearance: Appearance,
    /// Nesting level below the section's own top level (0 = top).
    depth: usize,
    /// Whether this row has nested children (so it gets a fold chevron).
    has_children: bool,
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
    let all = ctl.library.list_smart_collections().unwrap_or_default();
    let entries: Vec<(Uuid, Option<Uuid>)> = all.iter().map(|sc| (sc.id, sc.parent_id)).collect();
    // Which ids are some other row's parent — i.e. get a fold chevron.
    let parents: HashSet<Uuid> = all.iter().filter_map(|sc| sc.parent_id).collect();

    nest_order(&entries)
        .into_iter()
        .map(|(ix, depth)| {
            let sc = &all[ix];
            let count = sc
                .query
                .node()
                .and_then(|node| ctl.library.count_smart_rule(node).ok())
                .unwrap_or(0);
            SmartRow {
                id: sc.id,
                name: sc.name.clone(),
                count,
                appearance: sc.appearance.clone(),
                depth,
                has_children: parents.contains(&sc.id),
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
    rows: Vec<CollectionRow>,
    smart_rows: Vec<SmartRow>,
}

impl Snapshot {
    fn take(ctl: &LibraryController) -> Self {
        let mut rows: Vec<CollectionRow> = Vec::new();
        if let Ok(roots) = ctl.library.collection_roots() {
            for root in roots {
                rows.push(CollectionRow {
                    id: root.id,
                    name: root.name.clone(),
                    is_root: true,
                    count: ctl.library.count_collection_assets(root.id).unwrap_or(0),
                    appearance: root.appearance.clone(),
                });
                if let Ok(children) = ctl.library.collection_children(Some(root.id)) {
                    for child in children {
                        rows.push(CollectionRow {
                            id: child.id,
                            name: child.name.clone(),
                            is_root: false,
                            count: ctl.library.count_collection_assets(child.id).unwrap_or(0),
                            appearance: child.appearance.clone(),
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
            rows,
            smart_rows,
        }
    }

    /// "All assets" highlights when nothing specific is selected.
    fn all_selected(&self) -> bool {
        self.current.is_none() && !self.showing_trash && !self.showing_recent
    }
}

/// One row of the collections panel, flattened for the virtualized list.
enum ExplorerRow {
    All {
        count: u64,
        selected: bool,
    },
    Recent {
        count: u64,
        selected: bool,
    },
    Trash {
        count: u64,
        selected: bool,
    },
    Collection {
        id: Uuid,
        name: String,
        count: u64,
        selected: bool,
        is_root: bool,
        appearance: Appearance,
    },
    /// The inline add / rename editor, at a raw left indent in px.
    Editor {
        indent: f32,
    },
    SmartHeader,
    Smart {
        id: Uuid,
        name: String,
        count: u64,
        appearance: Appearance,
        depth: usize,
        selected: bool,
        has_children: bool,
    },
}

/// Fixed height every row in the virtualized list shares (`uniform_list`
/// measures the first row and positions the rest at that height).
const EXPLORER_ROW_H: f32 = 28.;

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
    /// at. That is the whole key: the counts move only when the library
    /// does, and no row highlight reads a filter any more.
    snapshot_cache: Option<(u64, Snapshot)>,
    /// Smart collections whose nested children are folded away. UI state, not
    /// part of the generation-keyed snapshot, so it survives across renders.
    collapsed_smart: HashSet<Uuid>,
    /// Scroll position of the virtualized row list.
    scroll_handle: UniformListScrollHandle,
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
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            editor_input,
            mode: EditorMode::None,
            snapshot_cache: None,
            collapsed_smart: HashSet::new(),
            scroll_handle: UniformListScrollHandle::default(),
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
                    // Append at the end of the target level.
                    let position = match parent {
                        Some(pid) => ctl
                            .library
                            .collection_children(Some(pid))
                            .map(|c| c.len() as i64)
                            .unwrap_or(0),
                        None => ctl
                            .library
                            .collection_roots()
                            .map(|r| r.len() as i64)
                            .unwrap_or(0),
                    };
                    if let Ok(collection) = ctl.library.create_collection(&NewCollection {
                        parent_id: parent,
                        name,
                        position,
                    }) {
                        ctl.select_collection(Some(collection.id));
                    }
                    cx.notify();
                });
            }
            EditorMode::Renaming(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let outcome = ctl.library.rename_collection(id, &name);
                    ctl.report_failed("renaming a folder", outcome);
                    cx.notify();
                });
            }
            EditorMode::RenamingSmart(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let outcome = ctl.library.rename_smart_collection(id, &name);
                    ctl.report_failed("renaming a smart folder", outcome);
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
        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if ctl.showing_recent {
            return rust_i18n::t!("app.recent_viewed").to_string();
        }
        if let Some(sid) = ctl.active_smart
            && let Ok(Some(sc)) = ctl.library.get_smart_collection(sid)
        {
            return sc.name;
        }
        if let Some(cid) = ctl.current_collection
            && let Ok(Some(c)) = ctl.library.collection(cid)
        {
            if let Some(pid) = c.parent_id
                && let Ok(Some(p)) = ctl.library.collection(pid)
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

    /// Fold or unfold a smart collection's nested children.
    fn toggle_fold_smart(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed_smart.remove(&id) {
            self.collapsed_smart.insert(id);
        }
        cx.notify();
    }

    /// Render one row of the virtualized list. Every arm must produce a row
    /// of exactly [`EXPLORER_ROW_H`], or the uniform list misplaces the rows
    /// below it.
    fn render_row(
        &mut self,
        row: &ExplorerRow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let inner: AnyElement = match row {
            ExplorerRow::All { count, selected } => {
                let folder = Appearance::default();
                collection_row(
                    cx,
                    self.controller.clone(),
                    None,
                    rust_i18n::t!("app.all_assets").to_string(),
                    RowView {
                        count: *count,
                        selected: *selected,
                        is_root: true,
                        folder: &folder,
                    },
                )
                .h_full()
                .into_any_element()
            }
            ExplorerRow::Recent { count, selected } => {
                recent_row(cx, self.controller.clone(), *count, *selected)
            }
            ExplorerRow::Trash { count, selected } => {
                trash_row(cx, self.controller.clone(), *count, *selected)
            }
            ExplorerRow::Collection {
                id,
                name,
                count,
                selected,
                is_root,
                appearance,
            } => {
                let id = *id;
                let explorer = cx.entity();
                let menu_name = name.clone();
                collection_row(
                    cx,
                    self.controller.clone(),
                    Some(id),
                    name.clone(),
                    RowView {
                        count: *count,
                        selected: *selected,
                        is_root: *is_root,
                        folder: appearance,
                    },
                )
                .context_menu(move |menu, window, cx| {
                    collection_menu(menu, window, cx, &explorer, id, menu_name.clone())
                })
                .h_full()
                .into_any_element()
            }
            ExplorerRow::Editor { indent } => editor_row(&self.editor_input, px(*indent)),
            ExplorerRow::SmartHeader => smart_section_header(cx, self.controller.clone()),
            ExplorerRow::Smart {
                id,
                name,
                count,
                appearance,
                depth,
                selected,
                has_children,
            } => self.render_smart_row(
                *id,
                name,
                *count,
                appearance,
                *depth,
                *selected,
                *has_children,
                cx,
            ),
        };
        div()
            .h(px(EXPLORER_ROW_H))
            .w_full()
            .child(inner)
            .into_any_element()
    }

    /// One smart-collection row, under the app-wide tree contract: a single
    /// click selects it, a double click folds or unfolds its children.
    #[allow(clippy::too_many_arguments)]
    fn render_smart_row(
        &mut self,
        sid: Uuid,
        name: &str,
        count: u64,
        appearance: &Appearance,
        depth: usize,
        selected: bool,
        has_children: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let nested = depth > 0;
        let controller = self.controller.clone();
        let drop_ctl = self.controller.clone();
        let menu_name = name.to_string();
        let drag_name = name.to_string();

        div()
            .id(format!("smart-row-{sid}"))
            .h_full()
            .w_full()
            .cursor_pointer()
            .px_2()
            .rounded(cx.theme().radius)
            // The row stretches to the panel edge; the indent is padding, not
            // a margin, so the count keeps its column.
            .when(nested, |this| {
                this.pl(px(ROW_PAD + ROW_INDENT * depth as f32))
            })
            .when(selected, |this| this.bg(cx.theme().secondary))
            .on_click(cx.listener(move |this, ev: &ClickEvent, _window, cx| {
                if has_children && ev.click_count() >= 2 {
                    this.toggle_fold_smart(sid, cx);
                } else {
                    this.controller
                        .update(cx, |ctl, _| ctl.select_smart(Some(sid)));
                }
            }))
            // Drag onto another row to nest under it; the store refuses
            // cycles, so a self- or descendant-drop is reported rather than
            // performed. Attached before the context menu: that wrapper only
            // forwards children.
            .on_drag(SmartDrag(sid), move |_, _, _, cx| {
                let name = drag_name.clone();
                cx.new(|_| SmartDragPreview { name })
            })
            .drag_over::<SmartDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
            .on_drop(move |payload: &SmartDrag, _window, cx| {
                let dragged = payload.0;
                if dragged != sid {
                    drop_ctl.update(cx, move |ctl, cx| {
                        let target_parent = ctl
                            .library
                            .get_smart_collection(sid)
                            .ok()
                            .flatten()
                            .and_then(|s| s.parent_id);
                        let target_pos = ctl
                            .library
                            .list_smart_collections()
                            .map(|all| {
                                all.iter()
                                    .filter(|sc| sc.parent_id == target_parent)
                                    .position(|sc| sc.id == sid)
                                    .unwrap_or(0) as i64
                            })
                            .unwrap_or(0);
                        if let Err(e) = ctl.library.reorder_smart_collection(dragged, target_pos) {
                            ctl.notice = Some(
                                rust_i18n::t!("explorer.move_failed", error = e.to_string())
                                    .to_string(),
                            );
                        }
                        ctl.generation += 1;
                        cx.notify();
                    });
                }
            })
            .context_menu({
                let explorer = cx.entity();
                let controller = controller.clone();
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
                    .h_full()
                    .w_full()
                    .items_center()
                    .gap_2()
                    // A saved search is found by searching, so that is its
                    // default mark; a glyph the user chose replaces it the
                    // same way it does on a folder row.
                    .child(appearance::glyph(
                        Some(appearance),
                        assets::IconName::Search,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(appearance::label_color(appearance, cx))
                            .child(name.to_string()),
                    )
                    .child(muted_label(count.to_string(), cx)),
            )
            .into_any_element()
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

    /// "+" pinned to the trailing edge of the title bar (where the removed
    /// ellipsis menu used to sit).
    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let entity = cx.entity();
        Some(
            plus_button(
                "add-collection-title",
                rust_i18n::t!("explorer.add_collection").to_string(),
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.begin_add(window, cx));
            }),
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
        // Reuse the cached snapshot while the generation is unchanged: the
        // COUNT queries behind it re-run only after a mutation.
        let ctl = self.controller.read(cx);
        let generation = ctl.generation;
        let snapshot = match &self.snapshot_cache {
            Some((cached_gen, snap)) if *cached_gen == generation => snap.clone(),
            _ => {
                let snap = Snapshot::take(ctl);
                self.snapshot_cache = Some((generation, snap.clone()));
                snap
            }
        };
        let snap = snapshot;

        // Flatten the whole panel — the pseudo rows, the managed
        // collections, the smart section header and its (fold-aware) rows,
        // and any inline editor — into one row stream for the virtualized
        // list. Every row shares `EXPLORER_ROW_H`.
        let rename_target = match mode {
            EditorMode::Renaming(id) => Some(id),
            _ => None,
        };
        let mut rows: Vec<ExplorerRow> = Vec::new();

        // --- Pseudo rows: All assets, Recently viewed, Trash ---
        rows.push(ExplorerRow::All {
            count: snap.all_count,
            selected: snap.all_selected(),
        });
        rows.push(ExplorerRow::Recent {
            count: snap.recent_total,
            selected: snap.showing_recent,
        });
        rows.push(ExplorerRow::Trash {
            count: snap.trash_total,
            selected: snap.showing_trash,
        });

        // --- Managed collections ---
        for row in &snap.rows {
            if rename_target == Some(row.id) {
                // The renamed row itself is replaced by the inline editor.
                rows.push(ExplorerRow::Editor { indent: 14. });
                continue;
            }
            rows.push(ExplorerRow::Collection {
                id: row.id,
                name: row.name.clone(),
                count: row.count,
                selected: snap.current == Some(row.id) && !snap.showing_trash,
                is_root: row.is_root,
                appearance: row.appearance.clone(),
            });
        }
        if matches!(mode, EditorMode::Adding { .. }) {
            // Editor appears after the last collection row.
            rows.push(ExplorerRow::Editor { indent: 0. });
        }

        // --- Smart collections ---
        rows.push(ExplorerRow::SmartHeader);

        // Rows are in preorder, so once a folded row is met every deeper row
        // is hidden until a row at the same or shallower depth appears.
        let mut hidden_below: Option<usize> = None;
        for row in &snap.smart_rows {
            if let Some(depth) = hidden_below {
                if row.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            let folded = self.collapsed_smart.contains(&row.id);
            rows.push(ExplorerRow::Smart {
                id: row.id,
                name: row.name.clone(),
                count: row.count,
                appearance: row.appearance.clone(),
                depth: row.depth,
                selected: snap.active_smart == Some(row.id),
                has_children: row.has_children,
            });
            if folded && row.has_children {
                hidden_below = Some(row.depth);
            }
        }

        let row_count = rows.len();
        let scroll_handle = self.scroll_handle.clone();
        let list = uniform_list(
            "explorer-rows",
            row_count,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                range
                    .map(|ix| this.render_row(&rows[ix], window, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&scroll_handle)
        .size_full();

        v_flex()
            .size_full()
            .gap_1()
            .p_1()
            .key_context("Explorer")
            .on_action(
                cx.listener(|this, _: &crate::app::actions::Cancel, window, cx| {
                    this.cancel_editor(window, cx);
                }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .vertical_scrollbar(&scroll_handle),
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
        .h_full()
        .w_full()
        .items_center()
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
    icon: assets::IconName,
) -> Stateful<Div> {
    div()
        .id(row_id)
        .cursor_pointer()
        .w_full()
        .h_full()
        .px_2()
        .rounded(cx.theme().radius)
        .when(selected, |this| this.bg(cx.theme().secondary))
        .child(
            h_flex()
                .h_full()
                .w_full()
                .items_center()
                .gap_2()
                // The same leading slot a folder row carries, so the names
                // line up whether or not the folder has a glyph of its own.
                .child(appearance::glyph(None, icon, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(label),
                )
                .child(muted_label(count.to_string(), cx)),
        )
}

/// Dropping assets on the recent / trash rows sends them to the trash.
fn attach_trash_drop(row: Stateful<Div>, controller: Entity<LibraryController>) -> Stateful<Div> {
    row.drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
        .on_drop(move |payload: &AssetsDrag, _window, cx| {
            controller.update(cx, move |ctl, cx| {
                let outcome = ctl.library.trash_assets(&payload.0);
                ctl.report_failed("trashing dropped assets", outcome);
                ctl.deselect(&payload.0);
                cx.notify();
            });
        })
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
        assets::IconName::Clock,
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
        assets::IconName::Trash,
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
        .h_full()
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

/// What a row draws that is not its identity: its count, whether it is the
/// browsed one, how deep it sits, and the look its folder asks for.
#[derive(Clone, Copy)]
struct RowView<'a> {
    count: u64,
    selected: bool,
    is_root: bool,
    folder: &'a Appearance,
}

/// A single row in the collections list. `id == None` renders the
/// non-managed "All assets" pseudo-row.
fn collection_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    id: Option<Uuid>,
    name: String,
    view: RowView<'_>,
) -> Stateful<Div> {
    let RowView {
        count,
        selected,
        is_root,
        folder,
    } = view;
    let row_id = id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "all".to_string());
    let folder = folder.clone();

    let mut row = div()
        .id(format!("collection-row-{row_id}"))
        .cursor_pointer()
        .w_full()
        .h_full()
        .px_2()
        .rounded(cx.theme().radius)
        .on_click({
            let controller = controller.clone();
            move |_ev: &ClickEvent, _window, cx| {
                controller.update(cx, move |ctl, _| ctl.select_collection(id));
            }
        })
        .child(
            h_flex()
                .h_full()
                .w_full()
                .items_center()
                .gap_2()
                .child(appearance::glyph(
                    Some(&folder),
                    match id {
                        Some(_) => assets::IconName::Folder,
                        // The whole library, not a folder in it.
                        None => assets::IconName::Library,
                    },
                    cx,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(appearance::label_color(&folder, cx))
                        .child(name),
                )
                .child(muted_label(count.to_string(), cx)),
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
                        let outcome = ctl.library.add_assets_to_collection(cid, &payload.0);
                        ctl.report_failed("adding dropped assets to a folder", outcome);
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
    let position = match target {
        Some(pid) => ctl
            .library
            .collection_children(Some(pid))
            .map(|c| c.len() as i64)
            .unwrap_or(0),
        None => ctl
            .library
            .collection_roots()
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
    let position = ctl
        .library
        .list_smart_collections()
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
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
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
    let ctl_look = controller.clone();
    let look_name = name.clone();
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
                    let editing = ctl_edit
                        .read(cx)
                        .library
                        .get_smart_collection(id)
                        .ok()
                        .flatten();
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
        .item(appearance::submenu_item(
            window,
            cx,
            ctl_look,
            appearance::Target::Smart(id),
            look_name,
        ))
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
                        // The same gate as a managed collection's delete: the
                        // active smart set is only left once the row is gone.
                        match ctl.library.delete_smart_collection(id) {
                            Ok(()) => {
                                if ctl.active_smart == Some(id) {
                                    ctl.select_smart(None);
                                }
                                ctl.generation += 1;
                            }
                            Err(error) => {
                                ctl.report_error(
                                    rust_i18n::t!(
                                        "explorer.delete_failed",
                                        error = error.to_string()
                                    )
                                    .to_string(),
                                );
                            }
                        }
                        cx.notify();
                    });
                },
            ),
        )
}

/// Right-click menu for a managed collection.
fn collection_menu(
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    explorer: &Entity<ExplorerPanel>,
    id: Uuid,
    name: String,
) -> PopupMenu {
    let explorer_new = explorer.clone();
    let explorer_rename = explorer.clone();
    let explorer_delete = explorer.clone();
    // Read out front: the submenu wants `cx` mutably to build itself with.
    let look_controller = explorer.read(cx).controller.clone();
    let look_name = name.clone();

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
        .item(appearance::submenu_item(
            window,
            cx,
            look_controller,
            appearance::Target::Collection(id),
            look_name,
        ))
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(
                move |_, _, cx| {
                    explorer_delete.update(cx, |this, cx| {
                        let ctl = this.controller.clone();
                        ctl.update(cx, |ctl, cx| {
                            // The row leaves the sidebar only when the delete
                            // landed. The error used to be discarded and
                            // `generation` bumped anyway, so the list re-read the
                            // database, found the collection still there, and the
                            // user had already been told by an empty slot that it
                            // was gone.
                            match ctl.library.delete_collection(id) {
                                Ok(()) => {
                                    if ctl.current_collection == Some(id) {
                                        ctl.select_collection(None);
                                    }
                                    ctl.generation += 1;
                                }
                                Err(error) => {
                                    ctl.report_error(
                                        rust_i18n::t!(
                                            "explorer.delete_failed",
                                            error = error.to_string()
                                        )
                                        .to_string(),
                                    );
                                }
                            }
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
