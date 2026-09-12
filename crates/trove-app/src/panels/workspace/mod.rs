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
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

// The `gpui_kit::*` glob above re-exports everything from gpui, but the grid
// needs the virtualized `list` element under a distinct name: a local
// `Vec<Asset>` variable called `list` would otherwise shadow it.
use crate::panels::color_picker::{ColorPicked, ColorPickerState, picker_panel};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::list as list_element;
use gpui_kit::{Anchor, Bounds, ListOffset, Pixels};
use gpui_kit::{ListAlignment, ListState};

use crate::panels::search_box::SearchBox;

use serde_json::json;
use trove_core::layout::{
    GRID_GAP, MAX_ROW_HEIGHT, MIN_ASPECT, MIN_ROW_HEIGHT, RowLayout, justify_layout_with_target,
    target_row_height_for_scale,
};
use trove_core::model::{AssetKind, AssetQuery, AssetSort, NewSmartCollection};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::app::actions::{ClearSelection, MoveDown, MoveLeft, MoveRight, MoveUp, OpenPreview};
use crate::components::preview::{
    AssetPreviewEvent, AssetPreviewPanel, ModelViewport, ModelViewportEvent,
};
use crate::library::{GRID_PAGE_SIZE, LibraryController, ViewMode};

use crate::panels::workspace_context_menu::AssetsDragPreview;
use crate::panels::workspace_context_menu::asset_context_menu;

use crate::panels::common::{AssetsDrag, display_name, kind_icon, observe_controller};

mod cells;
mod data;
mod interactions;
mod rows;
mod toolbar;

use cells::{build_cell_element, build_list_row_element, model_source};
use data::{Cell, DataKey, Direction, Row, TIMELINE_HEADER_HEIGHT, ViewData, ViewKey};
use rows::{
    materialize_rows, next_cell_row, prev_cell_row, refill_rows, timeline_header, timeline_rows,
};
use toolbar::{kind_filter, kind_key, selection_toolbar, title_controls};

/// Fallback layout width before the container has been measured once
/// (assumes a ~1024px window minus the two side docks).
const FALLBACK_WIDTH: f32 = 1024.0 - 590.0;
/// Rows rendered beyond the viewport by the virtualized list, in px.
const LIST_OVERDRAW_PX: f32 = 400.0;
/// Fixed height of one row in list view mode.
const LIST_ROW_HEIGHT: f32 = 44.0;
/// How close (in rows) to the end of the list the next page is requested.
const PAGE_TRIGGER_ROWS: usize = 3;

// ======================== Center: thumbnail grid =============================

/// What fills the main area while a preview is open: the interactive 3D
/// viewport for models, or the full-size still / live player for every
/// other kind.
#[derive(Clone)]
enum MainPreview {
    Model(Entity<ModelViewport>),
    Asset(Entity<AssetPreviewPanel>),
}

impl MainPreview {
    /// Hand the preview's frames back to the window before it is dropped —
    /// gpui's sprite atlas never evicts on its own.
    fn release(&self, window: &mut Window, cx: &mut App) {
        match self {
            MainPreview::Model(viewport) => {
                viewport.update(cx, |viewport, _| viewport.release(window));
            }
            MainPreview::Asset(preview) => {
                preview.update(cx, |preview, cx| preview.release(window, cx));
            }
        }
    }
}

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    /// Self-contained floating search (trigger + popover + input).
    search_box: Entity<SearchBox>,
    /// Custom-colour picker state (popover in the toolbar row).
    color_picker: Entity<ColorPickerState>,
    /// Whether the picker popover is showing.
    color_picker_open: std::cell::Cell<bool>,
    /// A colour confirmed in the picker; consumed by the next render.
    pending_color_search: Option<String>,
    /// Measured available width of the scroll container, updated each
    /// prepaint so the layout tracks the real panel width.
    available_width: Entity<Pixels>,
    /// Frozen row layout of the grid (shared with the list render closures).
    rows: Rc<Vec<Row>>,
    /// Virtualized list state (row count + scroll position).
    list_state: ListState,
    /// Grid-zoom slider (title bar): pending scale before release; the
    /// committed value lives in [`LibraryController::row_height_scale`].
    zoom_slider: Entity<SliderState>,
    /// The view the current [`Self::rows`] were laid out for.
    view_key: Option<ViewKey>,
    /// Cached data pass (query + cells) for the current [`DataKey`].
    /// Rebuilt only when the key changes; resize re-renders reuse it.
    data: Option<ViewData>,
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
    /// Open main-area preview. While this is set the main area shows the
    /// previewed asset and the grid is not built at all — the other assets
    /// are hidden, which is the whole point of previewing one full-size.
    preview: Option<MainPreview>,
    /// Kept so the preview's close event stops arriving when it is dropped.
    preview_subscription: Option<Subscription>,
}

impl WorkspacePanel {
    /// Slider events: the label previews live; the actual row-height scale
    /// (and its config persistence) commits on release so a drag does not
    /// re-justify the grid on every tick.
    fn on_zoom_slider(
        &mut self,
        _: &Entity<SliderState>,
        event: &SliderEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SliderEvent::Release(value) = event {
            let scale = value.start().clamp(0.6, 1.8);
            self.controller.update(cx, |ctl, cx| {
                ctl.row_height_scale = scale;
                cx.notify();
            });
            let mut config = trove_core::config::AppConfig::load();
            config.grid_zoom = Some(scale);
            let _ = config.save();
        }
        cx.notify();
    }
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

    /// The panel title bar: item count, zoom, view/sort/favourites, search.
    fn title_suffix(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let ctl = self.controller.read(cx);
        let in_trash = ctl.showing_trash;
        let in_recent = ctl.showing_recent;
        let loaded = ctl.grid_loaded.min(self.last_total);
        let total = self.last_total;
        let controller = self.controller.clone();
        let slider_value = self.zoom_slider.read(cx).value().start();
        let zoom_label = format!("{:.0}%", (slider_value * 100.0).round());
        let count_label = if loaded < total {
            rust_i18n::t!("workspace.scroll_hint", loaded = loaded, total = total).to_string()
        } else if total == 1 {
            rust_i18n::t!("workspace.item_one").to_string()
        } else {
            rust_i18n::t!("workspace.items_many", count = total).to_string()
        };
        let mut row = h_flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(count_label),
            )
            .child(
                div()
                    .id("grid-zoom")
                    .flex_none()
                    .w(px(96.0))
                    .px_1()
                    .child(Slider::new(&self.zoom_slider)),
            )
            .child(
                div()
                    .text_xs()
                    .w(px(34.0))
                    .text_color(cx.theme().muted_foreground)
                    .child(zoom_label),
            )
            .child(title_controls(&controller, cx))
            .child(self.search_box.clone());
        if in_trash || in_recent {
            // Zoom has no effect in list view contexts of trash/recent? It
            // still does (grid layout), so keep everything; only these two
            // contextual actions differ.
            let action = if in_trash {
                Button::new("empty-trash")
                    .ghost()
                    .danger()
                    .xsmall()
                    .label(rust_i18n::t!("workspace.empty_all").to_string())
                    .tooltip(rust_i18n::t!("workspace.empty_all_tooltip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.empty_trash(cx)))
            } else {
                Button::new("clear-history")
                    .ghost()
                    .danger()
                    .xsmall()
                    .label(rust_i18n::t!("workspace.clear_history").to_string())
                    .tooltip(rust_i18n::t!("workspace.clear_history_tooltip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.clear_view_history(cx)))
            };
            row = row.child(action);
        }
        Some(row)
    }
}

impl WorkspacePanel {
    /// The in-panel toolbar row below the title bar: the kind filter, the
    /// colour picker (colour search), and the contextual actions.
    fn toolbar_row(&mut self, cx: &mut Context<Self>) -> Div {
        let ctl = self.controller.read(cx);
        let in_trash = ctl.showing_trash;
        let in_recent = ctl.showing_recent;
        let search_active = !in_trash && !in_recent && !ctl.search_text.trim().is_empty();
        let controller = self.controller.clone();
        let color_picker = self.color_picker.clone();
        let picker_open = self.color_picker_open.clone();
        h_flex()
            .w_full()
            .items_center()
            .gap_1()
            // Kind filter stays on the left of the row.
            .child(kind_filter(&controller, cx))
            .when(!in_trash && !in_recent, |row| {
                // Custom-colour picker: confirm opens a colour search.
                row.child(
                    div()
                        .id("color-picker-trigger")
                        .size_5()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().secondary)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .cursor_pointer()
                        .hover(|this| this.text_color(cx.theme().foreground))
                        .on_click({
                            let picker_open = picker_open.clone();
                            move |_, _, cx| {
                                picker_open.set(!picker_open.get());
                                cx.refresh_windows();
                            }
                        })
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(SharedString::from(
                                rust_i18n::t!("workspace.pick_color_search").to_string(),
                            ))
                            .build(window, cx)
                        })
                        .child(Icon::new(IconName::Palette).size_3()),
                )
                .when(self.color_picker_open.get(), |row| {
                    let color_picker = color_picker.clone();
                    let picker_open = picker_open.clone();
                    row.child({
                        let color_picker = color_picker.clone();
                        Popover::new("workspace-color-picker")
                            .anchor(Anchor::TopLeft)
                            .open(true)
                            .on_open_change({
                                let picker_open = picker_open.clone();
                                move |is_open: &bool, _, cx| {
                                    picker_open.set(*is_open);
                                    cx.refresh_windows();
                                }
                            })
                            .content(move |_, window, cx| picker_panel(&color_picker, window, cx))
                    })
                })
            })
            .when(in_trash, |row| {
                row.child(
                    Button::new("empty-trash")
                        .ghost()
                        .danger()
                        .xsmall()
                        .label(rust_i18n::t!("workspace.empty_all").to_string())
                        .tooltip(rust_i18n::t!("workspace.empty_all_tooltip").to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.empty_trash(cx))),
                )
            })
            .when(in_recent, |row| {
                row.child(
                    Button::new("clear-history")
                        .ghost()
                        .danger()
                        .xsmall()
                        .label(rust_i18n::t!("workspace.clear_history").to_string())
                        .tooltip(rust_i18n::t!("workspace.clear_history_tooltip").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_view_history(cx);
                        })),
                )
            })
            .when(search_active, |row| {
                row.child(
                    Button::new("save-smart")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip(rust_i18n::t!("workspace.save_as_smart").to_string())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_search_as_smart(window, cx);
                        })),
                )
            })
    }
}

impl EventEmitter<PanelEvent> for WorkspacePanel {}
impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The action handlers are shared by both modes, so the shell is built
        // before the branch below picks what goes inside it.
        let shell = v_flex()
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
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                // Escape backs out of the innermost thing: out of the
                // main-area preview when one is open, otherwise it falls
                // through to the app root, which clears the grid selection
                // as before.
                if this.preview.is_some() {
                    this.dismiss_preview(window, cx);
                    cx.stop_propagation();
                }
            }));

        // The preview replaces the grid outright: the query and the row
        // layout are skipped entirely, so hiding the other assets also costs
        // nothing to keep hidden.
        if let Some(preview) = self.preview.clone() {
            return shell
                .child(match preview {
                    MainPreview::Model(viewport) => viewport.into_any_element(),
                    MainPreview::Asset(preview) => preview.into_any_element(),
                })
                .into_any_element();
        }

        // A reveal request (click in the similar-images dialog) lands here:
        // consume the flag first so handling cannot loop the render.
        if let Some(reveal) = self.controller.read(cx).pending_reveal {
            self.controller
                .update(cx, |ctl, _| ctl.pending_reveal = None);
            self.reveal_asset(reveal, cx);
        }

        // --- context snapshot (drop the controller borrow early) -----------
        let (
            collection,
            active_tag,
            in_trash,
            in_recent,
            search,
            smart,
            grid_loaded,
            selected,
            filter_kind,
            filter_favorite,
            view_mode,
            sort,
            sort_desc,
            active_folder,
        ) = {
            let ctl = self.controller.read(cx);
            (
                ctl.current_collection,
                ctl.active_tag,
                ctl.showing_trash,
                ctl.showing_recent,
                ctl.search_text.trim().to_string(),
                ctl.active_smart,
                ctl.grid_loaded,
                ctl.selected_assets.clone(),
                ctl.filter_kind,
                ctl.filter_favorite,
                ctl.view_mode,
                ctl.sort,
                ctl.sort_desc,
                ctl.active_folder.clone(),
            )
        };
        let library_root = self.controller.read(cx).library.root().to_path_buf();

        // --- data pass (cached) ---------------------------------------------
        // The query is the most expensive step in this function and the cell
        // build stats one file per asset; neither may run per frame. A resize
        // re-renders continuously without touching any data input, so the
        // cache absorbs it; mutations bump the generation, which is part of
        // the key.
        let search_active = !in_trash && !in_recent && !search.is_empty();
        let data_key = DataKey {
            collection,
            in_trash,
            in_recent,
            smart,
            tag: active_tag,
            folder: active_folder.clone(),
            search: search.clone(),
            filter_kind,
            filter_favorite,
            sort,
            sort_desc,
            grid_loaded,
            library_root: library_root.clone(),
            generation: self.controller.read(cx).generation,
        };
        if self.data.as_ref().is_none_or(|d| d.key != data_key) {
            let (total, cells) = self.run_data_pass(cx, &data_key);
            self.data = Some(ViewData {
                key: data_key,
                total,
                cells: Rc::new(cells),
            });
        }
        let data = self.data.as_ref().unwrap();
        let total = data.total;
        let cells = data.cells.clone();
        let cells_empty = cells.is_empty();

        // --- measured width --------------------------------------------------
        let measured = f32::from(*self.available_width.read(cx));
        let raw_width = if measured > 0. {
            measured.max(160.)
        } else {
            FALLBACK_WIDTH
        };
        // Round to 10px to avoid recalculating on sub-pixel changes.
        let content_width = (raw_width / 10.0).round() * 10.0;
        // Grid zoom (row-height scale from the title-bar slider). Like the
        // width it only reshapes the same rows, so it goes through the same
        // debounced, scroll-preserving relayout path.
        let row_height_scale = self.controller.read(cx).row_height_scale;
        let target = target_row_height_for_scale(row_height_scale);

        // --- debounce size changes ------------------------------------------
        // Only recalculate layout when the width or zoom changes, and
        // debounce to avoid recalculating on every frame during a live
        // resize / slider drag.
        let width_changed = self.view_key.as_ref().is_some_and(|k| {
            k.content_width != content_width || k.row_height_scale != row_height_scale
        });
        let other_changed = self.view_key.as_ref().is_none_or(|k| {
            k.collection != collection
                || k.in_trash != in_trash
                || k.in_recent != in_recent
                || k.smart != smart
                || k.tag != active_tag
                || k.folder != active_folder
                || k.search != search
                || k.filter_kind != filter_kind
                || k.filter_favorite != filter_favorite
                || k.view_mode != view_mode
                || k.sort != sort
                || k.sort_desc != sort_desc
        });

        // Structural changes (view / filter / asset set) always relayout now;
        // a size-only change (width or zoom) is deferred until the resize /
        // slider drag settles, at which point the timer sets
        // `relayout_pending` and the next render applies it.
        let structural_changed = other_changed || self.covered != cells.len();
        let defer_layout = !structural_changed && width_changed && !self.relayout_pending;

        if defer_layout && self.debounce_timer.is_none() {
            let panel = cx.entity();
            self.debounce_timer = Some(cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                panel.update(cx, |this, cx| {
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
            in_recent,
            smart,
            tag: active_tag,
            folder: active_folder.clone(),
            search: search.clone(),
            filter_kind,
            filter_favorite,
            view_mode,
            sort,
            sort_desc,
            content_width,
            row_height_scale,
        };
        // The debounce settled on a width that already equals the applied one
        // (resize flickered back): clear the pending flag so future resizes
        // debounce again instead of jumping immediately.
        if self.relayout_pending && self.view_key.as_ref() == Some(&key) {
            self.relayout_pending = false;
        }
        if self.view_key.as_ref() != Some(&key) && !defer_layout {
            // View, width or zoom changed: full layout. Structural changes
            // reset scrolling (a new view starts at the top), but a
            // width/zoom-only change merely re-justifies the same assets —
            // remember what is on screen and restore it below, so a resize
            // or a slider drag does not throw the user back to the top.
            let width_only = width_changed && !other_changed;
            let anchor = if width_only {
                let top = self.list_state.logical_scroll_top();
                self.rows
                    .iter()
                    .skip(top.item_ix)
                    .find_map(|row| row.cells.first().map(|cell| cell.id))
            } else {
                None
            };
            let old_rows = self.rows.len();
            self.relayout_pending = false;
            let rows = if view_mode == ViewMode::Timeline {
                timeline_rows((*cells).clone(), content_width, target)
            } else if view_mode == ViewMode::List {
                // List mode: one full-width info row per asset, no justification.
                (*cells)
                    .clone()
                    .into_iter()
                    .map(|c| Row {
                        height: LIST_ROW_HEIGHT,
                        widths: vec![content_width],
                        cells: vec![c],
                        header: None,
                    })
                    .collect()
            } else {
                let aspects: Vec<f32> = cells.iter().map(|c| c.aspect()).collect();
                let layouts = justify_layout_with_target(&aspects, content_width, target);
                materialize_rows((*cells).clone(), &layouts)
            };
            self.rows = Rc::new(rows);
            self.view_key = Some(key);
            self.covered = self.rows.iter().map(|r| r.cells.len()).sum();
            if width_only {
                self.list_state.splice(0..old_rows, self.rows.len());
                // Put the asset that topped the viewport back at the top:
                // splice alone would clamp an overlapped scroll offset to
                // the first row.
                if let Some(id) = anchor
                    && let Some(new_ix) = self
                        .rows
                        .iter()
                        .position(|row| row.cells.iter().any(|cell| cell.id == id))
                {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: new_ix,
                        offset_in_item: px(0.),
                    });
                }
            } else {
                self.list_state.reset(self.rows.len());
            }
        } else if !defer_layout && self.covered != cells.len() {
            // Assets were added or removed: keep the frozen row *shapes*
            // (cells per row) and refill them, so scrolling stays stable
            // across unrelated mutations. List mode just rebuilds its
            // trivial one-cell rows.
            let new_rows: Vec<Row> = if view_mode == ViewMode::Timeline {
                // Sections move whenever the set does, so there is no frozen
                // shape worth preserving here.
                timeline_rows((*cells).clone(), content_width, target)
            } else if view_mode == ViewMode::List {
                (*cells)
                    .clone()
                    .into_iter()
                    .map(|c| Row {
                        height: LIST_ROW_HEIGHT,
                        widths: vec![content_width],
                        cells: vec![c],
                        header: None,
                    })
                    .collect()
            } else {
                let counts: Vec<usize> = self.rows.iter().map(|r| r.cells.len()).collect();
                refill_rows((*cells).clone(), &counts, content_width, target)
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
            // Timeline day headers are rows without cells.
            if let Some(label) = row.header.clone() {
                return timeline_header(label, cx);
            }
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
        let empty_message = if cells_empty {
            if search_active {
                rust_i18n::t!("workspace.no_results", query = search).to_string()
            } else {
                rust_i18n::t!("workspace.no_assets_hint").to_string()
            }
        } else {
            String::new()
        };

        // A colour confirmed in the picker opens a colour search; render has
        // the window the dialog needs.
        if let Some(hex) = self.pending_color_search.take() {
            super::workspace_search::open_color_search(&hex, &self.controller, window, cx);
        }
        let shell = shell.child(self.toolbar_row(cx));
        shell
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
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags in a `test`
    // attribute macro from the gpui prelude, which makes expanding `#[test]`
    // below recurse.
    use super::{Cell, Row, next_cell_row, prev_cell_row, timeline_rows};
    use trove_core::layout::target_row_height_for_scale;
    use trove_core::model::AssetKind;
    use uuid::Uuid;

    /// A square image cell on `day`, enough for the layout to work with.
    fn cell(seed: u8, day: &str) -> Cell {
        Cell {
            id: Uuid::from_u128(seed as u128),
            kind: AssetKind::Image,
            thumb: None,
            width: Some(100),
            height: Some(100),
            trashed: false,
            name: String::new(),
            size_bytes: 0,
            added: String::new(),
            day: day.to_string(),
            font_family: None,
            font_blob: None,
        }
    }

    #[test]
    fn timeline_splits_days_into_sections_newest_first() {
        let cells = vec![
            cell(1, "2026-01-02"),
            cell(2, "2026-09-10"),
            cell(3, "2026-01-02"),
        ];
        let rows = timeline_rows(cells, 800.0, target_row_height_for_scale(1.0));

        // Header, its rows, next header, its rows.
        assert!(rows[0].header.is_some(), "first row is a header");
        assert!(
            rows[0]
                .header
                .as_deref()
                .is_some_and(|h| h.contains("2026-09-10")),
            "newest day first: {:?}",
            rows[0].header
        );
        assert_eq!(rows[1].cells.len(), 1, "one asset on the newest day");
        assert!(rows[2].header.is_some());
        assert!(
            rows[2]
                .header
                .as_deref()
                .is_some_and(|h| h.contains("2026-01-02")),
            "older day second: {:?}",
            rows[2].header
        );
        // Every asset survives the regrouping.
        assert_eq!(rows.iter().map(|r| r.cells.len()).sum::<usize>(), 3);
    }

    #[test]
    fn navigation_hops_over_timeline_headers() {
        let rows = vec![
            Row::section("newest".into()),
            Row {
                height: 100.0,
                widths: vec![100.0, 100.0],
                cells: vec![cell(1, "2026-09-10"), cell(2, "2026-09-10")],
                header: None,
            },
            Row::section("older".into()),
            Row {
                height: 100.0,
                widths: vec![100.0],
                cells: vec![cell(3, "2026-09-09")],
                header: None,
            },
        ];
        assert_eq!(next_cell_row(&rows, 0), Some(1));
        assert_eq!(next_cell_row(&rows, 2), Some(3));
        assert_eq!(next_cell_row(&rows, 4), None);
        assert_eq!(prev_cell_row(&rows, 2), Some(1));
        assert_eq!(prev_cell_row(&rows, 0), None, "a leading header is skipped");
    }
}
