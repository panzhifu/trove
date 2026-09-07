//! Asset grid: virtualized, paged thumbnail grid with selection, keyboard
//! navigation and a right-click management menu.
//!
//! Rendering uses gpui's `list` (variable-height virtualized list): one list
//! item per justified row. The row structure (the DP-optimized breaks from
//! [`trove_core::layout::justify_layout`]) is frozen in [`Self::rows`] and
//! only rebuilt when the view changes, the container is resized, or the
//! asset set grows/shrinks — so appending a page never reshuffles rows the
//! user has already scrolled past.

use std::cell::Cell as CellFlag;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::base::{ElementExt as _, h_flex, v_flex};
use gpui_kit::component::Sizable;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

// The `gpui_kit::*` glob above re-exports everything from gpui, but the grid
// needs the virtualized `list` element under a distinct name: a local
// `Vec<Asset>` variable called `list` would otherwise shadow it.
use gpui_kit::list as list_element;
use gpui_kit::{Anchor, Bounds, Pixels};
use gpui_kit::{ListAlignment, ListState};

use serde_json::json;
use trove_core::layout::{
    GRID_GAP, MAX_ROW_HEIGHT, MIN_ASPECT, MIN_ROW_HEIGHT, RowLayout, TARGET_ROW_HEIGHT,
    justify_layout,
};
use trove_core::model::{AssetKind, AssetQuery, AssetSort, NewSmartCollection};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::actions::{MoveDown, MoveLeft, MoveRight, MoveUp, OpenPreview};
use crate::state::{GRID_PAGE_SIZE, LibraryController, ViewMode};

use crate::panels::workspace_context_menu::AssetsDragPreview;
use crate::panels::workspace_context_menu::asset_context_menu;

use super::common::{AssetsDrag, display_name, kind_icon, observe_controller};

/// Left+right padding of the grid container plus scrollbar allowance,
/// subtracted from the measured width before laying rows out.
const H_PADDING: f32 = 40.0;
/// Fallback layout width before the container has been measured once
/// (assumes a ~1024px window minus the two side docks and padding).
const FALLBACK_WIDTH: f32 = 1024.0 - 590.0 - H_PADDING;
/// Rows rendered beyond the viewport by the virtualized list, in px.
const LIST_OVERDRAW_PX: f32 = 400.0;
/// Fixed height of one row in list view mode.
const LIST_ROW_HEIGHT: f32 = 44.0;
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
    /// Display name + facts for the list view rows.
    name: String,
    size_bytes: u64,
    added: String,
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
    filter_kind: Option<AssetKind>,
    filter_favorite: bool,
    view_mode: ViewMode,
    sort: AssetSort,
    sort_desc: bool,
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
    /// Whether the search popover is shown. Shared as `Rc<Cell<bool>>`
    /// because the ✕ button inside the popover content (which runs with a
    /// popover context) must be able to close it too.
    search_open: Rc<CellFlag<bool>>,
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
    /// Set by the debounce timer: the next render should apply the newest
    /// width (layout recompute) even though only the width changed.
    relayout_pending: bool,
    /// Debounce timer handle.
    debounce_timer: Option<gpui::Task<()>>,
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
        _window: &mut Window,
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
                    this.child(filter_controls(&controller, cx)).child({
                        // Floating search: click the magnifier to open the
                        // popover; ✕ appears inside once there is text and
                        // closes the popover when clicked. The open flag is
                        // shared so the ✕ (running with a popover context)
                        // can dismiss it.
                        let search_open = self.search_open.clone();
                        Popover::new("search-popover")
                            .anchor(Anchor::TopRight)
                            .open(search_open.get())
                            .on_open_change({
                                let search_open = search_open.clone();
                                move |open: &bool, _, cx| {
                                    search_open.set(*open);
                                    cx.refresh_windows();
                                }
                            })
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
                                    .when(search_active, |b| b.primary())
                                    .tooltip(
                                        rust_i18n::t!("workspace.search").to_string(),
                                    ),
                            )
                            .content({
                                let input = input.clone();
                                let ctl = controller.clone();
                                let search_open = search_open.clone();
                                move |_, _, cx| {
                                    let has_text =
                                        !input.read(cx).value().trim().is_empty();
                                    h_flex()
                                        .w(px(260.))
                                        .h_7()
                                        .items_center()
                                        .rounded_full()
                                        .border_1()
                                        .border_color(cx.theme().input)
                                        .bg(cx.theme().background)
                                        .px_3()
                                        .gap_1()
                                        .shadow_sm()
                                        .child(
                                            Input::new(&input)
                                                .appearance(false)
                                                .small()
                                                .w_full(),
                                        )
                                        .when(has_text, |row| {
                                            row.child(
                                                Button::new("clear-search")
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Close)
                                                    .tooltip(rust_i18n::t!(
                                                        "workspace.clear_search"
                                                    )
                                                    .to_string())
                                                    .on_click({
                                                        let input = input.clone();
                                                        let ctl = ctl.clone();
                                                        let search_open =
                                                            search_open.clone();
                                                        move |_, window, cx| {
                                                            input.update(cx, |state, cx| {
                                                                state.set_value(
                                                                    "", window, cx,
                                                                )
                                                            });
                                                            ctl.update(cx, |ctl, _| {
                                                                ctl.set_search(String::new())
                                                            });
                                                            search_open.set(false);
                                                            cx.refresh_windows();
                                                        }
                                                    }),
                                            )
                                        })
                                        .into_any_element()
                                }
                            })
                            .into_any_element()
                    })
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
                        Button::new("save-smart")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip(rust_i18n::t!("workspace.save_as_smart").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_search_as_smart(window, cx);
                            })),
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
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("workspace.search_placeholder").to_string())
        });
        let available_width = cx.new(|_| px(0.));
        let list_state = ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW_PX));
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            search_input,
            search_open: Rc::new(CellFlag::new(false)),
            available_width,
            rows: Rc::new(Vec::new()),
            list_state,
            view_key: None,
            covered: 0,
            last_total: 0,
            relayout_pending: false,
            debounce_timer: None,
        };
        observe_controller(cx, &this.controller);

        let input = this.search_input.clone();
        let ctl = this.controller.clone();
        cx.subscribe_in(&input, window, move |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let text: String = this.search_input.read(cx).value().to_string();
                ctl.update(cx, |ctl, _| ctl.set_search(text));
            } else if matches!(event, InputEvent::Change) {
                // Re-render so the ✕ inside the popover tracks the text.
                cx.notify();
            }
        })
        .detach();
        this
    }

    /// Surface a view/query failure in the status bar. `report_error`
    /// dedupes identical messages so a re-render cannot re-notify in a
    /// loop when the same query keeps failing.
    fn report_view_error(&mut self, cx: &mut Context<Self>, error: impl std::fmt::Display) {
        let msg = rust_i18n::t!("workspace.query_failed", error = error.to_string()).to_string();
        self.controller.update(cx, |ctl, cx| {
            if ctl.report_error(msg) {
                cx.notify();
            }
        });
    }

    /// Save the active full-text search as a smart collection. The stored
    /// query tree uses the same `fts_query` the live search runs, so the
    /// saved results match 1:1 and track future imports.
    fn save_search_as_smart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.controller.read(cx).search_text.trim().to_string();
        if search.is_empty() {
            return;
        }
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
        });
        let ctl = self.controller.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(rust_i18n::t!("workspace.save_as_smart").to_string())
                .width(px(380.))
                .child(Input::new(&name_input).small().appearance(true))
                .on_ok({
                    let name_input = name_input.clone();
                    let ctl = ctl.clone();
                    let search = search.clone();
                    move |_, _, cx| {
                        let name: String = name_input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            ctl.update(cx, |ctl, cx| {
                                let input = NewSmartCollection {
                                    name,
                                    query: json!({
                                        "op": "match",
                                        "field": "text",
                                        "value": search,
                                    }),
                                    color: None,
                                    position: 0,
                                };
                                match ctl.library.create_smart_collection(&input) {
                                    Ok(_) => {
                                        ctl.generation += 1;
                                        cx.notify();
                                    }
                                    Err(e) => {
                                        ctl.notice = Some(
                                            rust_i18n::t!(
                                                "workspace.smart_create_failed",
                                                error = e.to_string()
                                            )
                                            .to_string(),
                                        );
                                        cx.notify();
                                    }
                                }
                            });
                        }
                        true
                    }
                })
        });
    }

    fn empty_trash(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            ctl.notice = match ctl.library.empty_trash() {
                Ok(n) => Some(rust_i18n::t!("workspace.trash_emptied", count = n).to_string()),
                Err(e) => Some(
                    rust_i18n::t!("workspace.trash_empty_failed", error = e.to_string())
                        .to_string(),
                ),
            };
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
        // The favorites toggle turns the unfiltered "all assets" view into
        // the favorites view; named views keep their names.
        if ctl.filter_favorite {
            return rust_i18n::t!("workspace.title_favorites").to_string();
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
            let Some(asset) = assets::get(conn, id).ok().flatten() else {
                return;
            };

            let thumb = asset
                .sha256
                .as_deref()
                .map(|sha| trove_core::media::thumb::abs_path(&library_root, sha))
                .filter(|p| p.is_file());
            (display_name(&asset), thumb, asset.kind)
        };

        window.open_dialog(cx, move |dialog, _window, cx| {
            let min_zoom: f32 = 100.0;
            let max_zoom: f32 = 800.0;
            let track_width = px(240.0);

            // Shared zoom state (entity persists across dialog refreshes).
            let zoom_state = cx.new(|_cx| ZoomSliderState {
                value: 100,
                drag: DragData::default(),
            });

            let zoom_val = zoom_state.read(cx).value;
            let scale = zoom_val as f32 / 100.0;
            let max_h = 520.0_f32 * scale;

            let preview: AnyElement = match &thumb {
                Some(path) => img(path.clone())
                    .max_h(px(max_h))
                    .max_w(px(720.0 * scale))
                    .object_fit(gpui_kit::ObjectFit::Contain)
                    .into_any_element(),
                None => v_flex()
                    .h_64()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(kind_icon(kind)).size_8())
                    .into_any_element(),
            };

            let handle_pos = ((zoom_val as f32 - min_zoom) / (max_zoom - min_zoom)).clamp(0.0, 1.0);
            let handle_x = handle_pos * 240.0_f32;
            let zs = zoom_state.clone();
            let slider = div()
                .id("zoom-slider-track")
                .relative()
                .h(px(28.))
                .w(track_width)
                .cursor_pointer()
                // Mouse down on track: jump to position and begin drag.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    move |ev: &gpui::MouseDownEvent, _, cx| {
                        let ratio = (ev.position.x / track_width).clamp(0.0, 1.0);
                        let new_val = (min_zoom + ratio * (max_zoom - min_zoom)).round() as u32;
                        zs.update(cx, |state, cx| {
                            state.value = new_val;
                            state.drag.dragging = true;
                            state.drag.start_x = ev.position.x.into();
                            state.drag.start_value = new_val;
                            cx.refresh_windows();
                        });
                    },
                )
                // Mouse move: update zoom while dragging.
                .on_mouse_move({
                    let zs = zoom_state.clone();
                    move |ev: &gpui::MouseMoveEvent, _, cx| {
                        let dragging = zs.read(cx).drag.dragging;
                        if dragging {
                            let start_x: gpui_kit::Pixels = zs.read(cx).drag.start_x.into();
                            let start_val = zs.read(cx).drag.start_value;
                            let dx: f32 = (ev.position.x - start_x).into();
                            let ratio = (dx / 240.0_f32).clamp(-1.0, 1.0);
                            let new_val = (start_val as f32 + ratio * (max_zoom - min_zoom))
                                .clamp(min_zoom, max_zoom)
                                as u32;
                            zs.update(cx, |state, cx| {
                                state.value = new_val;
                                cx.refresh_windows();
                            });
                        }
                    }
                })
                // Track background.
                .child(
                    div()
                        .absolute()
                        .top(px(12.))
                        .left(px(0.))
                        .h(px(4.))
                        .w(track_width)
                        .bg(cx.theme().secondary)
                        .rounded(px(2.)),
                )
                // Filled portion.
                .child(
                    div()
                        .absolute()
                        .top(px(12.))
                        .left(px(0.))
                        .h(px(4.))
                        .w(px(handle_x))
                        .bg(cx.theme().primary)
                        .rounded(px(2.)),
                )
                // Draggable handle.
                .child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .left(px(handle_x - 8.))
                        .size(px(16.))
                        .rounded_full()
                        .bg(cx.theme().primary)
                        .border_2()
                        .border_color(cx.theme().background)
                        .shadow_sm()
                        .cursor_grab(),
                );

            dialog.title(name.clone()).width(px(780.)).child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    // Zoom slider bar.
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .child(slider)
                            .child(
                                div()
                                    .text_xs()
                                    .w(px(40.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}%", zoom_val)),
                            ),
                    )
                    // Preview image.
                    .child(
                        div()
                            .w_full()
                            .max_h(px(600.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .child(preview),
                    ),
            )
        });
    }
}

/// Zoom slider state for the preview dialog.
struct ZoomSliderState {
    /// Current zoom value (100–800).
    value: u32,
    /// Drag tracking data.
    drag: DragData,
}

#[derive(Clone, Copy, Default)]
struct DragData {
    dragging: bool,
    start_x: f32,
    start_value: u32,
}

impl Render for WorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // --- context snapshot (drop the controller borrow early) -----------
        let (
            collection,
            active_tag,
            in_trash,
            search,
            smart,
            grid_loaded,
            selected,
            filter_kind,
            filter_favorite,
            view_mode,
            sort,
            sort_desc,
        ) = {
            let ctl = self.controller.read(cx);
            (
                ctl.current_collection,
                ctl.active_tag,
                ctl.showing_trash,
                ctl.search_text.trim().to_string(),
                ctl.active_smart,
                ctl.grid_loaded,
                ctl.selected_assets.clone(),
                ctl.filter_kind,
                ctl.filter_favorite,
                ctl.view_mode,
                ctl.sort,
                ctl.sort_desc,
            )
        };
        let library_root = self.controller.read(cx).library.root().to_path_buf();

        // --- paged query (four mutually-exclusive view drivers) -----------
        let limit = Some(grid_loaded as u32);
        let search_active = !in_trash && !search.is_empty();
        let (mut total, mut list): (usize, Vec<trove_core::model::Asset>) = if search_active {
            let q = AssetQuery {
                collection_id: collection,
                tag_ids: active_tag.map(|t| vec![t]).unwrap_or_default(),
                kind: filter_kind,
                is_favorite: filter_favorite.then_some(true),
                is_trashed: false,
                text: None,
                limit,
                ..Default::default()
            };
            let result =
                assets::search(self.controller.read(cx).library.store().conn(), &search, &q);
            match result {
                Ok((t, a)) => (t as usize, a),
                Err(e) => {
                    self.report_view_error(cx, e);
                    (0, Vec::new())
                }
            }
        } else if let Some(sid) = smart {
            let result = self.controller.read(cx).library.evaluate_smart_collection(
                sid,
                filter_kind,
                filter_favorite.then_some(true),
                limit,
                0,
            );
            match result {
                Ok((t, a)) => (t as usize, a),
                Err(e) => {
                    self.report_view_error(cx, e);
                    (0, Vec::new())
                }
            }
        } else {
            let result = assets::query(
                self.controller.read(cx).library.store().conn(),
                &AssetQuery {
                    collection_id: if in_trash { None } else { collection },
                    tag_ids: if in_trash {
                        Vec::new()
                    } else {
                        active_tag.map(|t| vec![t]).unwrap_or_default()
                    },
                    // The trash view hides the filter controls, so it also
                    // ignores the grid filters entirely.
                    kind: if in_trash { None } else { filter_kind },
                    is_favorite: (!in_trash && filter_favorite).then_some(true),
                    is_trashed: in_trash,
                    sort,
                    sort_desc,
                    limit,
                    ..Default::default()
                },
            );
            match result {
                Ok((t, a)) => (t as usize, a),
                Err(e) => {
                    self.report_view_error(cx, e);
                    (0, Vec::new())
                }
            }
        };

        // One search box, two engines: append CLIP semantic hits (image
        // content) that keyword FTS cannot see, skipping duplicates. Runs
        // only when the engine is configured; failures surface in the
        // status bar but keep the FTS result set visible.
        if search_active && trove_core::media::clip::semantic_ready() {
            let threshold = trove_core::config::AppConfig::load().semantic_min_similarity();
            let semantic_result = self
                .controller
                .read(cx)
                .library
                .semantic_text_search(&search, threshold, limit);
            if let Ok(hits) = semantic_result {
                let have: std::collections::HashSet<Uuid> = list.iter().map(|a| a.id).collect();
                let before = list.len();
                list.extend(hits.into_iter().filter(|a| !have.contains(&a.id)));
                total += list.len() - before;
            } else {
                self.report_view_error(cx, semantic_result.unwrap_err());
            }
        }

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
                    name: display_name(a),
                    size_bytes: a.size_bytes,
                    added: a.created_at.format("%Y-%m-%d %H:%M").to_string(),
                }
            })
            .collect();
        let cells_empty = cells.is_empty();

        // --- measured width --------------------------------------------------
        let measured = f32::from(*self.available_width.read(cx));
        let raw_width = if measured > 0. {
            (measured - H_PADDING).max(160.)
        } else {
            FALLBACK_WIDTH
        };
        // Round to 10px to avoid recalculating on sub-pixel changes.
        let content_width = (raw_width / 10.0).round() * 10.0;

        // --- debounce width changes -----------------------------------------
        // Only recalculate layout when width changes, and debounce to avoid
        // recalculating on every frame during a live resize.
        let width_changed = self
            .view_key
            .as_ref()
            .is_some_and(|k| k.content_width != content_width);
        let other_changed = self.view_key.as_ref().is_none_or(|k| {
            k.collection != collection
                || k.in_trash != in_trash
                || k.smart != smart
                || k.tag != active_tag
                || k.search != search
                || k.filter_kind != filter_kind
                || k.filter_favorite != filter_favorite
                || k.view_mode != view_mode
                || k.sort != sort
                || k.sort_desc != sort_desc
        });

        // Structural changes (view / filter / asset set) always relayout now;
        // a width-only change is deferred until resize settles, at which point
        // the timer sets `relayout_pending` and the next render applies it.
        let structural_changed = other_changed || self.covered != cells.len();
        let defer_layout = !structural_changed && width_changed && !self.relayout_pending;

        if defer_layout && self.debounce_timer.is_none() {
            let panel = cx.entity();
            self.debounce_timer = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let _ = panel.update(cx, |this, cx| {
                    this.relayout_pending = true;
                    this.debounce_timer = None;
                    cx.notify();
                });
            }));
        }

        // --- row (re)layout ----------------------------------------------------
        let key = ViewKey {
            collection,
            in_trash,
            smart,
            tag: active_tag,
            search: search.clone(),
            filter_kind,
            filter_favorite,
            view_mode,
            sort,
            sort_desc,
            content_width,
        };
        // The debounce settled on a width that already equals the applied one
        // (resize flickered back): clear the pending flag so future resizes
        // debounce again instead of jumping immediately.
        if self.relayout_pending && self.view_key.as_ref() == Some(&key) {
            self.relayout_pending = false;
        }
        if self.view_key.as_ref() != Some(&key) && !defer_layout {
            // View or width changed: full layout, scroll resets to top.
            self.relayout_pending = false;
            let rows = if view_mode == ViewMode::List {
                // List mode: one full-width row per asset, no justification.
                cells
                    .into_iter()
                    .map(|c| Row {
                        height: LIST_ROW_HEIGHT,
                        widths: vec![content_width],
                        cells: vec![c],
                    })
                    .collect()
            } else {
                let aspects: Vec<f32> = cells.iter().map(|c| c.aspect()).collect();
                let layouts = justify_layout(&aspects, content_width);
                materialize_rows(cells, &layouts)
            };
            self.rows = Rc::new(rows);
            self.view_key = Some(key);
            self.covered = self.rows.iter().map(|r| r.cells.len()).sum();
            self.list_state.reset(self.rows.len());
        } else if !defer_layout && self.covered != cells.len() {
            // Assets were added or removed: keep the frozen row *shapes*
            // (cells per row) and refill them, so scrolling stays stable
            // across unrelated mutations. List mode just rebuilds its
            // trivial one-cell rows.
            let new_rows: Vec<Row> = if view_mode == ViewMode::List {
                cells
                    .into_iter()
                    .map(|c| Row {
                        height: LIST_ROW_HEIGHT,
                        widths: vec![content_width],
                        cells: vec![c],
                    })
                    .collect()
            } else {
                let counts: Vec<usize> = self.rows.iter().map(|r| r.cells.len()).collect();
                refill_rows(cells, &counts, content_width)
            };
            let old_rows = self.rows.len();
            self.rows = Rc::new(new_rows);
            self.covered = self.rows.iter().map(|r| r.cells.len()).sum();
            // The frozen head rows are untouched; only the tail changed size.
            // splice(start..end, count) replaces [start, end) with `count`
            // items, so "append N rows" must replace an empty tail range with
            // exactly N items — the old call double-counted the tail and
            // corrupted the list state (sum_tree seek panic on the next
            // layout).
            let new_count = self.rows.len();
            if new_count >= old_rows {
                self.list_state
                    .splice(old_rows..old_rows, new_count - old_rows);
            } else {
                self.list_state.splice(new_count..old_rows, 0);
            }
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
        let toolbar_controller = controller.clone();
        let focus_handle = self.focus_handle.clone();
        let rows_for_render = rows.clone();
        let rows_len = rows.len();
        let list_mode = view_mode == ViewMode::List;
        // One page request per frame at most (several visible rows can all
        // sit within the trigger window of the end).
        let page_guard = Rc::new(CellFlag::new(false));
        let total_for_trigger = total;

        let grid = list_element(list_state, move |ix, _window, cx: &mut App| {
            // Infinite scroll: near the end, request the next page.
            if ix + PAGE_TRIGGER_ROWS >= rows_len && !page_guard.get() {
                page_guard.set(true);
                controller.update(cx, |ctl, cx| {
                    if ctl.grid_loaded < total_for_trigger {
                        ctl.grid_loaded = (ctl.grid_loaded + GRID_PAGE_SIZE).min(total_for_trigger);
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
            if list_mode {
                // One full-width info row per asset.
                return build_list_row_element(
                    cx,
                    &controller,
                    &focus_handle,
                    &cells[0],
                    widths[0],
                );
            }
            h_flex()
                .w_full()
                .gap(px(GRID_GAP))
                .children(
                    cells
                        .iter()
                        .zip(widths)
                        .map(|(cell, w)| {
                            build_cell_element(cx, &controller, &focus_handle, cell, w, height)
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        })
        // The list's default sizing behavior is `Auto` (no content-based
        // height): without an explicit size it measures 0px tall and paints
        // nothing. Fill the grid area instead.
        .size_full();

        // --- empty-state hint -------------------------------------------------
        // An empty grid currently paints nothing at all; tell the user why.
        // While searching, distinguish "nothing matches" from the actionable
        // "no embeddings yet" case (text search silently returns nothing).
        let empty_message = if cells_empty {
            let (embedded, total) = self
                .controller
                .read(cx)
                .library
                .embedding_status()
                .unwrap_or((0, 0));
            if search_active && total > 0 && embedded == 0 {
                rust_i18n::t!("workspace.no_embeddings_hint").to_string()
            } else if search_active {
                rust_i18n::t!("workspace.no_results", query = search).to_string()
            } else {
                rust_i18n::t!("workspace.no_assets_hint").to_string()
            }
        } else {
            String::new()
        };

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
                    .relative()
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
                    // Empty-state hint sits UNDER the grid so the grid keeps
                    // all mouse handling (deselect on click, etc.).
                    .when(!empty_message.is_empty(), |area| {
                        area.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(empty_message),
                                ),
                        )
                    })
                    // The toolbar must come AFTER the grid: later siblings
                    // paint on top, and the bar has to float over the cells.
                    .child(grid)
                    .when(selected.len() >= 2, |area| {
                        let ids = selected.clone();
                        area.child(selection_toolbar(&toolbar_controller, in_trash, ids, cx))
                    }),
            )
    }
}

// ============================ filter controls ================================

/// Type + favorites grid filters for the title bar: a kind dropdown, a
/// heart toggle and a clear button when anything is active. The filters
/// compose with every view (collection, search, smart collection) and are
/// also how the favorites view is entered.
fn filter_controls(controller: &Entity<LibraryController>, cx: &App) -> Div {
    let (kind, favorite, view_mode, sort, sort_desc) = {
        let ctl = controller.read(cx);
        (
            ctl.filter_kind,
            ctl.filter_favorite,
            ctl.view_mode,
            ctl.sort,
            ctl.sort_desc,
        )
    };
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let mut bar = h_flex().items_center().gap_1();

    // View toggle: grid ⇄ list presentation.
    let (next_mode, toggle_icon, toggle_tip) = match view_mode {
        ViewMode::Grid => (ViewMode::List, IconName::Menu, "workspace.view_list"),
        ViewMode::List => (
            ViewMode::Grid,
            IconName::GalleryVerticalEnd,
            "workspace.view_grid",
        ),
    };
    bar = bar.child(
        Button::new("view-toggle")
            .xsmall()
            .ghost()
            .icon(toggle_icon)
            .tooltip(t(toggle_tip))
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    controller.update(cx, |ctl, cx| {
                        ctl.set_view_mode(next_mode);
                        cx.notify();
                    });
                }
            }),
    );

    // Sort dropdown: key + direction pairs.
    let sort_options: Vec<(AssetSort, bool, String)> = vec![
        (AssetSort::CreatedAt, true, t("workspace.sort_newest")),
        (AssetSort::CreatedAt, false, t("workspace.sort_oldest")),
        (AssetSort::Name, false, t("workspace.sort_name_asc")),
        (AssetSort::Name, true, t("workspace.sort_name_desc")),
        (AssetSort::SizeBytes, true, t("workspace.sort_size_desc")),
        (AssetSort::SizeBytes, false, t("workspace.sort_size_asc")),
        (AssetSort::Rating, true, t("workspace.sort_rating_desc")),
    ];
    bar = bar.child(
        Button::new("sort-menu")
            .xsmall()
            .ghost()
            .icon(if sort_desc {
                IconName::SortDescending
            } else {
                IconName::SortAscending
            })
            .tooltip(t("workspace.sort"))
            .dropdown_menu_with_anchor(Anchor::TopLeft, {
                let controller = controller.clone();
                move |menu, _, _| {
                    let mut menu = menu.min_w(px(170.));
                    for (value, desc, label) in &sort_options {
                        let checked = *value == sort && *desc == sort_desc;
                        let (value, desc) = (*value, *desc);
                        let controller = controller.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    controller.update(cx, |ctl, cx| {
                                        ctl.set_sort(value, desc);
                                        cx.notify();
                                    });
                                },
                            ));
                    }
                    menu
                }
            }),
    );

    // Kind dropdown: label shows the active kind, "all" when unset.
    let kind_label = match kind {
        Some(k) => t(kind_key(k)),
        None => t("workspace.filter_all_kinds"),
    };
    let options: Vec<(Option<AssetKind>, String)> =
        std::iter::once((None, t("workspace.filter_all_kinds")))
            .chain(
                [
                    AssetKind::Image,
                    AssetKind::Video,
                    AssetKind::Audio,
                    AssetKind::Document,
                    AssetKind::Archive,
                    AssetKind::Font,
                    AssetKind::Other,
                ]
                .into_iter()
                .map(|k| (Some(k), t(kind_key(k)))),
            )
            .collect();
    bar = bar.child(
        Button::new("filter-kind")
            .xsmall()
            .outline()
            .label(kind_label)
            .dropdown_menu_with_anchor(Anchor::TopLeft, {
                let controller = controller.clone();
                move |menu, _, _| {
                    let mut menu = menu.min_w(px(150.));
                    for (value, label) in &options {
                        let checked = *value == kind;
                        let value = *value;
                        let controller = controller.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    controller.update(cx, |ctl, cx| {
                                        ctl.set_filter_kind(value);
                                        cx.notify();
                                    });
                                },
                            ));
                    }
                    menu
                }
            }),
    );

    // Favorites toggle: the primary (filled) state marks the active filter.
    bar = bar.child(
        Button::new("filter-favorite")
            .xsmall()
            .when(favorite, |b| b.primary())
            .when(!favorite, |b| b.ghost())
            .icon(IconName::Heart)
            .tooltip(t("workspace.filter_favorite"))
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    controller.update(cx, |ctl, _| ctl.set_filter_favorite(!favorite));
                }
            }),
    );

    // Reset when anything is active.
    if kind.is_some() || favorite {
        bar = bar.child(
            Button::new("clear-filters")
                .xsmall()
                .ghost()
                .label("×")
                .tooltip(t("workspace.clear_filters"))
                .on_click({
                    let controller = controller.clone();
                    move |_, _, cx| controller.update(cx, |ctl, _| ctl.clear_filters())
                }),
        );
    }
    bar
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

/// Floating batch-action bar over the grid while two or more assets are
/// selected. Every action hits the existing batch APIs, then deselects.
fn selection_toolbar(
    controller: &Entity<LibraryController>,
    in_trash: bool,
    ids: Vec<Uuid>,
    cx: &App,
) -> Div {
    let count = ids.len();
    let all_favorite = if in_trash {
        false
    } else {
        let conn = controller.read(cx).library.store().conn();
        assets::by_ids(conn, &ids)
            .map(|list| list.iter().all(|a| a.is_favorite))
            .unwrap_or(false)
    };
    let ctl_fav = controller.clone();
    let ctl_trash = controller.clone();
    let ctl_restore = controller.clone();
    let ctl_purge = controller.clone();
    let ctl_add = controller.clone();
    let ctl_clear = controller.clone();

    let mut bar = h_flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_lg()
        .child(
            div()
                .px_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("workspace.selected_many", count = count).to_string()),
        );

    if in_trash {
        bar = bar
            .child(
                Button::new("sel-restore")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Undo)
                    .tooltip(rust_i18n::t!("workspace.restore").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_restore.update(cx, |ctl, cx| {
                            let ids = std::mem::take(&mut ctl.selected_assets);
                            let _ = ctl.library.restore_assets(&ids);
                            ctl.selection_anchor = None;
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("sel-purge")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip(rust_i18n::t!("workspace.delete_forever").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_purge.update(cx, |ctl, cx| {
                            let ids = std::mem::take(&mut ctl.selected_assets);
                            if let Err(e) = ctl.library.purge_assets(&ids) {
                                ctl.notice = Some(
                                    rust_i18n::t!("workspace.purge_failed", error = e.to_string())
                                        .to_string(),
                                );
                            }
                            ctl.selection_anchor = None;
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            );
    } else {
        bar = bar
            .child(
                Button::new("sel-fav")
                    .xsmall()
                    .ghost()
                    .icon(if all_favorite {
                        IconName::HeartOff
                    } else {
                        IconName::Heart
                    })
                    .tooltip(
                        rust_i18n::t!(if all_favorite {
                            "workspace.remove_from_favorites"
                        } else {
                            "workspace.add_to_favorites"
                        })
                        .to_string(),
                    )
                    .on_click(move |_, _, cx| {
                        ctl_fav.update(cx, |ctl, cx| {
                            let ids = ctl.selected_assets.clone();
                            let _ = ctl.library.set_assets_favorite(&ids, !all_favorite);
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("sel-add")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip(rust_i18n::t!("workspace.add_to_collection").to_string())
                    .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, cx| {
                        let conn = ctl_add.read(cx).library.store().conn();
                        let mut items: Vec<(Uuid, String)> = Vec::new();
                        if let Ok(roots) = collections::roots(conn) {
                            for root in roots {
                                items.push((root.id, root.name.clone()));
                                if let Ok(children) = collections::children_of(conn, Some(root.id))
                                {
                                    for child in children {
                                        items.push((child.id, child.name.clone()));
                                    }
                                }
                            }
                        }
                        let mut menu = menu.min_w(px(180.));
                        if items.is_empty() {
                            menu = menu.item(PopupMenuItem::label(
                                rust_i18n::t!("workspace.no_collections").to_string(),
                            ));
                        }
                        for (cid, cname) in items {
                            let ctl = ctl_add.clone();
                            menu =
                                menu.item(PopupMenuItem::new(cname).on_click(move |_, _, cx| {
                                    ctl.update(cx, |ctl, cx| {
                                        let ids = ctl.selected_assets.clone();
                                        let _ = ctl.library.add_assets_to_collection(cid, &ids);
                                        ctl.generation += 1;
                                        cx.notify();
                                    });
                                }));
                        }
                        menu
                    }),
            )
            .child(
                Button::new("sel-trash")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip(rust_i18n::t!("app.move_to_trash").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_trash.update(cx, |ctl, cx| {
                            ctl.trash_or_purge_selection();
                            ctl.selection_anchor = None;
                            cx.notify();
                        });
                    }),
            );
    }

    let bar = bar.child(
        Button::new("sel-clear")
            .xsmall()
            .ghost()
            .label("×")
            .tooltip(rust_i18n::t!("app.clear_selection").to_string())
            .on_click(move |_, _, cx| {
                ctl_clear.update(cx, |ctl, cx| {
                    ctl.clear_selection();
                    cx.notify();
                });
            }),
    );

    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_3()
        .flex()
        .justify_center()
        .child(bar)
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
            if m.shift {
                // Range select: anchor (last plain click) to this cell in
                // display order, replacing the selection.
                ctl.select_range_to(id_click);
            } else if multi {
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
    let base = base.on_drag(AssetsDrag(ids_for_drag), move |payload, _offset, _, cx| {
        cx.new(|_cx| AssetsDragPreview {
            count: payload.0.len(),
        })
    });

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
}

/// One full-width info row for list view: small thumbnail (or kind icon),
/// name, kind label, size and import date, with the same click / drag /
/// context-menu behavior as the grid cells.
fn build_list_row_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    cell: &Cell,
    w: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let (name, size, added) = (cell.name.clone(), cell.size_bytes, cell.added.clone());
    let is_sel = controller.read(cx).selected_assets.contains(&id);

    let lead: AnyElement = match &thumb {
        Some(path) => img(path.clone())
            .w(px(60.))
            .h(px(36.))
            .object_fit(gpui_kit::ObjectFit::Cover)
            .rounded(cx.theme().radius)
            .into_any_element(),
        None => div()
            .w(px(60.))
            .h(px(36.))
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .bg(cx.theme().secondary)
            .child(Icon::new(kind_icon(kind)).size_5())
            .into_any_element(),
    };

    let base = div()
        .id(format!("row-{id}"))
        .cursor_pointer()
        .w(px(w))
        .h(px(LIST_ROW_HEIGHT))
        .px_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(if is_sel {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .child(
            h_flex()
                .w_full()
                .h_full()
                .items_center()
                .gap_3()
                .child(lead)
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
                        .w(px(64.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!(kind_key(kind)).to_string()),
                )
                .child(
                    div()
                        .w(px(80.))
                        .text_right()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(super::common::human_bytes(size)),
                )
                .child(
                    div()
                        .w(px(110.))
                        .text_right()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(added),
                ),
        );

    let ctl_click = controller.clone();
    let focus = focus_handle.clone();
    let base = base.on_click(move |event: &ClickEvent, window, _cx| {
        window.focus(&focus, _cx);
        let m = event.modifiers();
        let multi = m.control || m.platform;
        ctl_click.update(_cx, move |ctl, _| {
            if m.shift {
                ctl.select_range_to(id);
            } else if multi {
                ctl.toggle_asset(id);
            } else {
                ctl.select_asset(Some(id));
            }
        });
    });

    let selected_now = controller.read(cx).selected_assets.clone();
    let ids_for_drag = if selected_now.contains(&id) {
        selected_now
    } else {
        vec![id]
    };
    let base = base.on_drag(AssetsDrag(ids_for_drag), move |payload, _offset, _, cx| {
        cx.new(|_cx| AssetsDragPreview {
            count: payload.0.len(),
        })
    });

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
}

// asset_context_menu, open_image_search and AssetsDragPreview moved to
// workspace_context_menu.rs / workspace_search.rs
