//! Asset grid: virtualized, paged thumbnail grid with selection, keyboard
//! navigation and a right-click management menu.
//!
//! Rendering uses gpui's `list` (variable-height virtualized list): one list
//! item per justified row. The row structure (the DP-optimized breaks from
//! [`trove_core::layout::justify_layout`]) is frozen in [`Self::rows`] and
//! only rebuilt when the view changes, the container is resized, or the
//! asset set grows/shrinks — so appending a page never reshuffles rows the
//! user has already scrolled past.

use std::path::PathBuf;
use std::rc::Rc;
use std::cell::Cell as CellFlag;

use gpui_kit::base::{h_flex, v_flex, ElementExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;

// The `gpui_kit::*` glob above re-exports everything from gpui, but the grid
// needs the virtualized `list` element under a distinct name: a local
// `Vec<Asset>` variable called `list` would otherwise shadow it.
use gpui_kit::{ListAlignment, ListState};
use gpui_kit::list as list_element;
use gpui_kit::{Anchor, Bounds, Pixels};

use trove_core::layout::{
    GRID_GAP, MAX_ROW_HEIGHT, MIN_ASPECT, MIN_ROW_HEIGHT, TARGET_ROW_HEIGHT, RowLayout,
    justify_layout,
};
use trove_core::model::{AssetKind, AssetPatch, AssetQuery};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::actions::{MoveDown, MoveLeft, MoveRight, MoveUp, OpenPreview};
use crate::state::{GRID_PAGE_SIZE, LibraryController};

use super::common::{display_name, kind_icon, observe_controller, AssetsDrag};

/// Left+right padding of the grid container plus scrollbar allowance,
/// subtracted from the measured width before laying rows out.
const H_PADDING: f32 = 40.0;
/// Fallback layout width before the container has been measured once
/// (assumes a ~1024px window minus the two side docks and padding).
const FALLBACK_WIDTH: f32 = 1024.0 - 590.0 - H_PADDING;
/// Rows rendered beyond the viewport by the virtualized list, in px.
const LIST_OVERDRAW_PX: f32 = 400.0;
/// How close (in rows) to the end of the list the next page is requested.
const PAGE_TRIGGER_ROWS: usize = 3;

/// Data needed to paint one grid cell. Immutable per layout epoch;
/// `selected` is resolved live at paint time so selection changes never
/// rebuild rows.
#[derive(Debug, Clone)]
struct Cell {
    id: Uuid,
    kind: AssetKind,
    thumb: Option<PathBuf>,
    width: Option<u32>,
    height: Option<u32>,
    trashed: bool,
}

impl Cell {
    fn aspect(&self) -> f32 {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => w as f32 / h as f32,
            _ => 1.0,
        }
    }
}

/// One frozen justified row: uniform height, cell widths in display order.
#[derive(Debug, Clone)]
struct Row {
    height: f32,
    widths: Vec<f32>,
    cells: Vec<Cell>,
}

impl Row {
    /// Horizontal center of each cell (for up/down nearest-column moves).
    fn centers(&self) -> Vec<f32> {
        let mut x = GRID_GAP / 2.0;
        self.widths
            .iter()
            .map(|w| {
                let c = x + w / 2.0;
                x += w + GRID_GAP;
                c
            })
            .collect()
    }
}

/// Identifies the browsed view + container width. Any change re-runs the
/// global justify layout and resets list scrolling.
#[derive(Debug, Clone, PartialEq)]
struct ViewKey {
    collection: Option<Uuid>,
    in_trash: bool,
    smart: Option<Uuid>,
    tag: Option<Uuid>,
    search: String,
    content_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

// ======================== Center: thumbnail grid =============================

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    search_input: Entity<InputState>,
    /// Measured available width of the scroll container, updated each
    /// prepaint so the layout tracks the real panel width.
    available_width: Entity<Pixels>,
    /// Frozen row layout of the grid (shared with the list render closures).
    rows: Rc<Vec<Row>>,
    /// Virtualized list state (row count + scroll position).
    list_state: ListState,
    /// The view the current [`Self::rows`] were laid out for.
    view_key: Option<ViewKey>,
    /// How many cells the frozen rows cover (pagination / set-change cursor).
    covered: usize,
    /// Total asset count of the current view, captured by the last render and
    /// displayed next to the title-bar buttons.
    last_total: usize,
}

impl BasePanel for WorkspacePanel {
    fn panel_name(&self) -> &'static str {
        "WorkspacePanel"
    }
    fn closable(&self, _: &App) -> bool {
        false
    }
}
impl DockPanel for WorkspacePanel {
    /// Title text: follows the browsed view (collection name, smart
    /// collection, trash, or the all-assets fallback). The interactive
    /// buttons live in [`title_suffix`] which renders outside the title's
    /// clipping container, so they stay visible.
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

    /// Trailing edge of the title bar: a search icon in normal mode, an
    /// "Empty all" button in trash mode, and the clear-search button (×)
    /// when a search is active. This hook renders outside the title's
    /// `overflow_hidden` container, so the buttons are always visible.
    fn title_suffix(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let ctl = self.controller.read(cx);
        let in_trash = ctl.showing_trash;
        let search_active = !in_trash && !ctl.search_text.trim().is_empty();
        let loaded = ctl.grid_loaded.min(self.last_total);
        let total = self.last_total;
        let _ = ctl;
        // Item count for the browsed view; the heading text itself lives in
        // the dock title, so only the counter sits beside the buttons here.
        let count_label = div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(if loaded < total {
                rust_i18n::t!("workspace.scroll_hint", loaded = loaded, total = total).to_string()
            } else if total == 1 {
                rust_i18n::t!("workspace.item_one").to_string()
            } else {
                rust_i18n::t!("workspace.items_many", count = total).to_string()
            });
        let input = self.search_input.clone();
        let controller = self.controller.clone();
        Some(
            h_flex()
                .items_center()
                .gap_1()
                .child(count_label)
                .when(!in_trash, |this| {
                    this.child(
                        Popover::new("search-popover")
                            .anchor(Anchor::TopRight)
                            // The pill-shaped search input IS the surface:
                            // strip the popover's own bg/border/shadow/padding
                            // (our style is refined after the default) while
                            // keeping the default overlay-click-to-close.
                            .bg(gpui::transparent_black())
                            .border_0()
                            .shadow_none()
                            .p_0()
                            .trigger(
                                Button::new("search")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Search)
                                    .tooltip(rust_i18n::t!("workspace.search").to_string()),
                            )
                            .content({
                                let input = input.clone();
                                move |_, _, cx| {
                                    h_flex()
                                        .w(px(260.))
                                        .h_7()
                                        .items_center()
                                        .rounded_full()
                                        .border_1()
                                        .border_color(cx.theme().input)
                                        .bg(cx.theme().background)
                                        .px_3()
                                        .shadow_sm()
                                        .child(
                                            Input::new(&input)
                                                .appearance(false)
                                                .small()
                                                .w_full(),
                                        )
                                        .into_any_element()
                                }
                            }),
                    )
                })
                .when(in_trash, |this| {
                    this.child(
                        Button::new("empty-trash")
                            .ghost()
                            .danger()
                            .xsmall()
                            .label(rust_i18n::t!("workspace.empty_all").to_string())
                            .tooltip(rust_i18n::t!("workspace.empty_all_tooltip").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.empty_trash(cx))),
                    )
                })
                .when(search_active, |this| {
                    this.child(
                        Button::new("clear-search")
                            .ghost()
                            .xsmall()
                            .label("×")
                            .tooltip(rust_i18n::t!("workspace.clear_search").to_string())
                            .on_click(move |_, window, cx| {
                                controller.update(cx, |ctl, _| ctl.set_search(String::new()));
                                input.update(cx, |state, cx| state.set_value("", window, cx));
                            }),
                    )
                }),
        )
    }
}
impl EventEmitter<PanelEvent> for WorkspacePanel {}
impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl WorkspacePanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder(rust_i18n::t!("workspace.search_placeholder").to_string()));
        let available_width = cx.new(|_| px(0.));
        let list_state = ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW_PX));
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            search_input,
            available_width,
            rows: Rc::new(Vec::new()),
            list_state,
            view_key: None,
            covered: 0,
            last_total: 0,
        };
        observe_controller(cx, &this.controller);

        let input = this.search_input.clone();
        let ctl = this.controller.clone();
        cx.subscribe_in(&input, window, move |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let text: String = this.search_input.read(cx).value().to_string();
                ctl.update(cx, |ctl, _| ctl.set_search(text));
            }
        })
        .detach();
        this
    }

    fn empty_trash(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            match ctl.library.empty_trash() {
                Ok(n) => eprintln!("emptied trash: {n}"),
                Err(e) => eprintln!("empty trash failed: {e}"),
            }
            ctl.selected_assets.clear();
            ctl.generation += 1;
            cx.notify();
        });
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — smart collection, collection (with its parent
    /// prefix when nested), trash, or the all-assets fallback.
    fn title_label(&self, cx: &Context<Self>) -> String {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if let Some(sid) = ctl.active_smart {
            if let Ok(Some(sc)) = smart_collections::get(conn, sid) {
                return sc.name;
            }
        }
        if let Some(cid) = ctl.current_collection {
            if let Ok(Some(c)) = collections::get(conn, cid) {
                if let Some(pid) = c.parent_id {
                    if let Ok(Some(p)) = collections::get(conn, pid) {
                        return format!("{} / {}", p.name, c.name);
                    }
                }
                return c.name;
            }
        }
        rust_i18n::t!("app.all_assets").to_string()
    }

    // -- keyboard navigation ---------------------------------------------------

    /// Move the selection one step in `toward`, following the frozen row
    /// geometry: left/right step through the flat order (wrapping across row
    /// edges); up/down lands on the nearest column center of the adjacent
    /// row. The moved-to row is revealed in the virtualized list.
    fn move_selection(&mut self, toward: Direction, cx: &mut Context<Self>) {
        let rows = self.rows.clone();
        if rows.is_empty() {
            return;
        }

        // Locate the primary selection within the frozen rows.
        let primary = self.controller.read(cx).primary();
        let locate = |id: Option<Uuid>| -> Option<(usize, usize)> {
            rows.iter().enumerate().find_map(|(r, row)| {
                row.cells
                    .iter()
                    .position(|cell| Some(cell.id) == id)
                    .map(|c| (r, c))
            })
        };

        let target: (usize, usize) = match locate(primary) {
            // Nothing selected: start from the grid edge in the move's
            // direction.
            None => match toward {
                Direction::Left | Direction::Up => {
                    let last = rows.len() - 1;
                    (last, rows[last].cells.len().saturating_sub(1))
                }
                Direction::Right | Direction::Down => (0, 0),
            },
            Some((r, c)) => match toward {
                Direction::Left => {
                    if c > 0 {
                        (r, c - 1)
                    } else if r > 0 {
                        (r - 1, rows[r - 1].cells.len() - 1)
                    } else {
                        (r, c)
                    }
                }
                Direction::Right => {
                    if c + 1 < rows[r].cells.len() {
                        (r, c + 1)
                    } else if r + 1 < rows.len() {
                        (r + 1, 0)
                    } else {
                        (r, c)
                    }
                }
                Direction::Up | Direction::Down => {
                    let step: i64 = if toward == Direction::Up { -1 } else { 1 };
                    let nr = r as i64 + step;
                    if nr < 0 || nr as usize >= rows.len() {
                        (r, c)
                    } else {
                        let nr = nr as usize;
                        let centers = rows[r].centers();
                        let x = centers.get(c).copied().unwrap_or(0.0);
                        let best = rows[nr]
                            .centers()
                            .iter()
                            .enumerate()
                            .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        (nr, best)
                    }
                }
            },
        };

        let Some(cell) = rows.get(target.0).and_then(|row| row.cells.get(target.1)) else {
            return;
        };
        let (id, row_ix) = (cell.id, target.0);
        self.controller.update(cx, |ctl, cx| {
            ctl.select_asset(Some(id));
            cx.notify();
        });
        self.list_state.scroll_to_reveal_item(row_ix);
    }

    /// Enter: open a large preview of the primary selected asset.
    fn open_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Extract everything needed while borrowing the controller, so no
        // borrow crosses into the dialog-capture path below.
        let (name, thumb, kind) = {
            let ctl = self.controller.read(cx);
            let Some(id) = ctl.primary() else { return };
            let library_root = ctl.library.root().to_path_buf();
            let conn = ctl.library.store().conn();
            let Some(asset) = assets::get(conn, id).ok().flatten() else { return };

            let thumb = asset
                .sha256
                .as_deref()
                .map(|sha| trove_core::media::thumb::abs_path(&library_root, sha))
                .filter(|p| p.is_file());
            (display_name(&asset), thumb, asset.kind)
        };

        window.open_dialog(cx, move |dialog, _, _| {
            let preview: AnyElement = match &thumb {
                Some(path) => img(path.clone())
                    .max_h(px(520.))
                    .object_fit(gpui_kit::ObjectFit::Contain)
                    .into_any_element(),
                None => v_flex()
                    .h_64()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(kind_icon(kind)).size_8())
                    .into_any_element(),
            };
            dialog
                .title(name.clone())
                .width(px(760.))
                .child(v_flex().p_2().items_center().child(preview))
        });
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // --- context snapshot (drop the controller borrow early) -----------
        let (collection, active_tag, in_trash, search, smart, grid_loaded) = {
            let ctl = self.controller.read(cx);
            (
                ctl.current_collection,
                ctl.active_tag,
                ctl.showing_trash,
                ctl.search_text.trim().to_string(),
                ctl.active_smart,
                ctl.grid_loaded,
            )
        };
        let library_root = self.controller.read(cx).library.root().to_path_buf();

        // --- paged query (three mutually-exclusive view drivers) -----------
        let limit = Some(grid_loaded as u32);
        let search_active = !in_trash && !search.is_empty();
        let (total, list): (usize, Vec<trove_core::model::Asset>) = if search_active {
            let q = AssetQuery {
                collection_id: collection,
                tag_ids: active_tag.map(|t| vec![t]).unwrap_or_default(),
                is_trashed: false,
                text: None,
                limit,
                ..Default::default()
            };
            match assets::search(self.controller.read(cx).library.store().conn(), &search, &q) {
                Ok((t, a)) => (t as usize, a),
                Err(_) => (0, Vec::new()),
            }
        } else if let Some(sid) = smart {
            match self
                .controller
                .read(cx)
                .library
                .evaluate_smart_collection(sid, limit, 0)
            {
                Ok((t, a)) => (t as usize, a),
                Err(_) => (0, Vec::new()),
            }
        } else {
            match assets::query(
                self.controller.read(cx).library.store().conn(),
                &AssetQuery {
                    collection_id: if in_trash { None } else { collection },
                    tag_ids: if in_trash {
                        Vec::new()
                    } else {
                        active_tag.map(|t| vec![t]).unwrap_or_default()
                    },
                    is_trashed: in_trash,
                    limit,
                    ..Default::default()
                },
            ) {
                Ok((t, a)) => (t as usize, a),
                Err(_) => (0, Vec::new()),
            }
        };

        let cells: Vec<Cell> = list
            .iter()
            .filter(|a| in_trash || a.trashed_at.is_none())
            .map(|a| {
                let thumb = a
                    .sha256
                    .as_deref()
                    .map(|sha| trove_core::media::thumb::abs_path(&library_root, sha))
                    .filter(|p| p.is_file());
                Cell {
                    id: a.id,
                    kind: a.kind,
                    thumb,
                    width: a.width,
                    height: a.height,
                    trashed: a.trashed_at.is_some(),
                }
            })
            .collect();

        // --- measured width --------------------------------------------------
        let measured = f32::from(*self.available_width.read(cx));
        let content_width = if measured > 0. {
            (measured - H_PADDING).max(160.)
        } else {
            FALLBACK_WIDTH
        };

        // --- row (re)layout ----------------------------------------------------
        let key = ViewKey {
            collection,
            in_trash,
            smart,
            tag: active_tag,
            search: search.clone(),
            content_width,
        };
        if self.view_key.as_ref() != Some(&key) {
            // View or width changed: full DP layout, scroll resets to top.
            let aspects: Vec<f32> = cells.iter().map(|c| c.aspect()).collect();
            let layouts = justify_layout(&aspects, content_width);
            self.rows = Rc::new(materialize_rows(cells, &layouts));
            self.view_key = Some(key);
            self.covered = self.rows.iter().map(|r| r.cells.len()).sum();
            self.list_state.reset(self.rows.len());
        } else if self.covered != cells.len() {
            // Assets were added or removed: keep the frozen row *shapes*
            // (cells per row) and refill them, so scrolling stays stable
            // across unrelated mutations.
            let counts: Vec<usize> = self.rows.iter().map(|r| r.cells.len()).collect();
            let old_rows = self.rows.len();
            self.rows = Rc::new(refill_rows(cells, &counts, content_width));
            self.covered = self.rows.iter().map(|r| r.cells.len()).sum();
            self.list_state.splice(old_rows..self.rows.len(), self.rows.len());
        }
        let rows = self.rows.clone();
        self.last_total = total;

        // Publish the visible ids for Edit ▸ Select-all (no notify needed:
        // the panels re-render through the regular observation channels).
        let visible: Vec<Uuid> = rows
            .iter()
            .flat_map(|r| r.cells.iter().map(|c| c.id))
            .collect();
        self.controller.update(cx, |ctl, _| {
            ctl.visible_assets = visible;
        });

        // --- virtualized list -----------------------------------------------------
        let list_state = self.list_state.clone();
        let controller = self.controller.clone();
        let focus_handle = self.focus_handle.clone();
        let rows_for_render = rows.clone();
        let rows_len = rows.len();
        // One page request per frame at most (several visible rows can all
        // sit within the trigger window of the end).
        let page_guard = Rc::new(CellFlag::new(false));
        let total_for_trigger = total;

        let grid = list_element(
            list_state,
            move |ix, _window, cx: &mut App| {
                // Infinite scroll: near the end, request the next page.
                if ix + PAGE_TRIGGER_ROWS >= rows_len && !page_guard.get() {
                    page_guard.set(true);
                    controller.update(cx, |ctl, cx| {
                        if ctl.grid_loaded < total_for_trigger {
                            ctl.grid_loaded =
                                (ctl.grid_loaded + GRID_PAGE_SIZE).min(total_for_trigger);
                            cx.notify();
                        }
                    });
                }
                let Some(row) = rows_for_render.get(ix) else {
                    return v_flex().into_any_element();
                };
                let widths = row.widths.clone();
                let height = row.height;
                let cells = row.cells.clone();
                h_flex()
                    .w_full()
                    .gap(px(GRID_GAP))
                    .children(
                        cells
                            .iter()
                            .zip(widths)
                            .map(|(cell, w)| {
                                build_cell_element(
                                    cx, &controller, &focus_handle, cell, w, height,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()
            },
        )
        // The list's default sizing behavior is `Auto` (no content-based
        // height): without an explicit size it measures 0px tall and paints
        // nothing. Fill the grid area instead.
        .size_full();

        v_flex()
            .size_full()
            .gap_1()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                this.move_selection(Direction::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| {
                this.move_selection(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_selection(Direction::Up, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_selection(Direction::Down, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenPreview, window, cx| {
                this.open_preview(window, cx);
            }))
            .child(
                div()
                    .id("assets-grid-area")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .on_prepaint({
                        let available_width = self.available_width.clone();
                        let controller = self.controller.clone();
                        move |bounds: Bounds<Pixels>, _, cx| {
                            let w = bounds.size.width;
                            available_width.update(cx, |width, cx| {
                                if *width != w {
                                    *width = w;
                                    // Width changes re-layout the grid; wake
                                    // the controller so panels re-render.
                                    controller.update(cx, |_, cx| cx.notify());
                                }
                            });
                        }
                    })
                    .child(grid),
            )
    }
}

// ============================ row construction ===============================

/// Pair a cell list with DP row layouts into frozen [`Row`]s.
fn materialize_rows(cells: Vec<Cell>, layouts: &[RowLayout]) -> Vec<Row> {
    let mut cells = cells.into_iter();
    layouts
        .iter()
        .map(|layout| Row {
            height: layout.height,
            widths: layout.item_widths.clone(),
            cells: (&mut cells).take(layout.item_widths.len()).collect(),
        })
        .collect()
}

/// Refill frozen row *shapes* (cells per row) with a new cell list.
///
/// Used when assets are added or removed: rows keep their positions, only
/// content and recomputed heights change. Cells that overflow the frozen
/// shapes (the set grew) become greedily-fitted tail rows.
fn refill_rows(cells: Vec<Cell>, counts: &[usize], content_width: f32) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut iter = cells.into_iter();
    for count in counts {
        let chunk: Vec<Cell> = iter.by_ref().take(*count).collect();
        if chunk.is_empty() {
            break;
        }
        let aspects: Vec<f32> = chunk.iter().map(|c| c.aspect()).collect();
        let layout = fit_row(&aspects, content_width);
        rows.push(Row {
            height: layout.height,
            widths: layout.item_widths,
            cells: chunk,
        });
    }
    let rest: Vec<Cell> = iter.collect();
    if !rest.is_empty() {
        let aspects: Vec<f32> = rest.iter().map(|c| c.aspect()).collect();
        let layouts = justify_layout(&aspects, content_width);
        let mut rest = rest.into_iter();
        for layout in layouts {
            rows.push(Row {
                height: layout.height,
                widths: layout.item_widths.clone(),
                cells: (&mut rest).take(layout.item_widths.len()).collect(),
            });
        }
    }
    rows
}

/// Lay out a single row: uniform height, aspect-preserving widths, spanning
/// the container exactly (same formula as the DP's per-row scoring).
fn fit_row(aspects: &[f32], content_width: f32) -> RowLayout {
    let aspects: Vec<f32> = aspects.iter().map(|a| a.max(MIN_ASPECT)).collect();
    let k = aspects.len() as f32;
    let natural: f32 = aspects.iter().sum::<f32>() * TARGET_ROW_HEIGHT;
    let content = (content_width - GRID_GAP * (k - 1.0)).max(1.0);
    let h_raw = TARGET_ROW_HEIGHT * content / natural.max(1e-3);
    // Raising a too-short row to MIN would push it past the container width;
    // keep the exact fit there, so MIN stays a soft bound and rows never
    // overflow.
    let height = if h_raw < MIN_ROW_HEIGHT {
        h_raw
    } else {
        h_raw.min(MAX_ROW_HEIGHT)
    };
    RowLayout {
        height,
        item_widths: aspects.iter().map(|a| a * height).collect(),
    }
}

// ============================ cell rendering =================================

/// One cell thumbnail with click / drag / context-menu behavior, rendered at
/// the exact pixel size the row layout assigned to it. Selection is read live
/// from the controller. Clicking focuses the panel so grid keyboard
/// navigation (arrows / Delete / Enter) applies.
fn build_cell_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    cell: &Cell,
    w: f32,
    h: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let is_sel = controller.read(cx).selected_assets.contains(&id);

    let preview: AnyElement = match &thumb {
        Some(path) => img(path.clone())
            .size_full()
            .object_fit(gpui_kit::ObjectFit::Contain)
            .into_any_element(),
        None => v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(Icon::new(kind_icon(kind)).size_8())
            .into_any_element(),
    };
    let base = div()
        .id(format!("cell-{id}"))
        .cursor_pointer()
        .flex_none()
        .w(px(w))
        .h(px(h))
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(if is_sel {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .overflow_hidden()
        .child(preview);

    let ctl_click = controller.clone();
    let id_click = id;
    let focus = focus_handle.clone();
    let base = base.on_click(move |event: &ClickEvent, window, _cx| {
        // Focus the grid so keyboard navigation applies right away.
        window.focus(&focus, _cx);
        let m = event.modifiers();
        let multi = m.control || m.platform;
        ctl_click.update(_cx, move |ctl, _| {
            if multi {
                ctl.toggle_asset(id_click);
            } else {
                ctl.select_asset(Some(id_click));
            }
        });
    });

    // Drag source: drags the clicked asset, or the whole selection when it
    // includes this one.
    let selected_now = controller.read(cx).selected_assets.clone();
    let ids_for_drag = if selected_now.contains(&id) {
        selected_now
    } else {
        vec![id]
    };
    let base = base.on_drag(
        AssetsDrag(ids_for_drag),
        move |payload, _offset, _, cx| {
            cx.new(|_cx| AssetsDragPreview {
                count: payload.0.len(),
            })
        },
    );

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
}

/// Right-click menu on an asset thumbnail.
fn asset_context_menu(
    menu: PopupMenu,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    trashed: bool,
) -> PopupMenu {
    if trashed {
        let ctl_restore = controller.clone();
        let ctl_purge = controller.clone();
        return menu
            .min_w(px(180.))
            .item(
                PopupMenuItem::new(rust_i18n::t!("workspace.restore").to_string()).on_click(move |_, _, cx| {
                    ctl_restore.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        let _ = ctl.library.restore_assets(&ids);
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(rust_i18n::t!("workspace.delete_forever").to_string()).on_click(move |_, _, cx| {
                    ctl_purge.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        if let Err(e) = ctl.library.purge_assets(&ids) {
                            eprintln!("purge failed: {e}");
                        }
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                }),
            );
    }

    let conn = controller.read(cx).library.store().conn();
    let favorite = assets::get(conn, asset_id)
        .ok()
        .flatten()
        .map(|a| a.is_favorite)
        .unwrap_or(false);

    let ctl_build = controller.clone();
    let c_fav = controller.clone();
    let c_trash = controller.clone();

    let add_submenu = PopupMenu::build(_window, cx, move |menu, _window, cx| {
        let controller = ctl_build;
        let conn = controller.read(cx).library.store().conn();
        let mut items: Vec<(Uuid, String)> = Vec::new();
        if let Ok(roots) = collections::roots(conn) {
            for root in roots {
                items.push((root.id, root.name.clone()));
                if let Ok(children) = collections::children_of(conn, Some(root.id)) {
                    for child in children {
                        items.push((child.id, child.name.clone()));
                    }
                }
            }
        }
        let mut menu = menu;
        if items.is_empty() {
            menu = menu.item(PopupMenuItem::label(rust_i18n::t!("workspace.no_collections").to_string()));
        }
        for (cid, cname) in items {
            let controller = controller.clone();
            menu = menu.item(
                PopupMenuItem::new(cname).on_click(move |_, _, cx| {
                    controller.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        let _ = ctl.library.add_assets_to_collection(cid, &ids);
                        ctl.generation += 1;
                        cx.notify();
                    });
                }),
            );
        }
        menu
    });

    menu.min_w(px(200.))
        .item(
            PopupMenuItem::new(if favorite {
                rust_i18n::t!("workspace.remove_from_favorites").to_string()
            } else {
                rust_i18n::t!("workspace.add_to_favorites").to_string()
            })
            .checked(favorite)
            .on_click(move |_, _, cx| {
                c_fav.update(cx, move |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    let _ = assets::update(
                        conn,
                        asset_id,
                        &AssetPatch {
                            is_favorite: Some(!favorite),
                            ..Default::default()
                        },
                    );
                    ctl.generation += 1;
                    cx.notify();
                });
            }),
        )
        .separator()
        .item(PopupMenuItem::submenu(rust_i18n::t!("workspace.add_to_collection").to_string(), add_submenu))
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("app.move_to_trash").to_string()).on_click(move |_, _, cx| {
                c_trash.update(cx, move |ctl, cx| {
                    let ids = ctl.action_targets(asset_id);
                    let _ = ctl.library.trash_assets(&ids);
                    ctl.deselect(&ids);
                    cx.notify();
                });
            }),
        )
}

/// Drag preview shown while dragging assets.
struct AssetsDragPreview {
    count: usize,
}
impl Render for AssetsDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().primary)
            .text_sm()
            .text_color(cx.theme().primary_foreground)
            .child(if self.count == 1 {
                rust_i18n::t!("workspace.drag_one").to_string()
            } else {
                rust_i18n::t!("workspace.drag_many", count = self.count).to_string()
            })
    }
}
