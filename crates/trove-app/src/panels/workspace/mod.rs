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
use gpui_kit::component::dock::{BasePanel, PanelEvent};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

// The `gpui_kit::*` glob above re-exports everything from gpui, but the grid
// needs the virtualized `list` element under a distinct name: a local
// `Vec<Asset>` variable called `list` would otherwise shadow it.
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::list as list_element;
use gpui_kit::{Bounds, ListOffset, Pixels};
use gpui_kit::{ListAlignment, ListState};

use crate::panels::search_box::SearchBox;

use serde_json::json;
use trove_core::layout::{
    GRID_GAP, MAX_ROW_HEIGHT, MIN_ASPECT, MIN_ROW_HEIGHT, RowLayout, justify_layout_with_target,
    target_row_height_for_scale,
};
use trove_core::model::{AssetKind, AssetSort, NewSmartCollection, Orientation};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::app::actions::{ClearSelection, MoveDown, MoveLeft, MoveRight, MoveUp, OpenPreview};
use crate::components::preview::{
    AssetPreviewData, AssetPreviewEvent, AssetPreviewPanel, ModelViewport, ModelViewportEvent,
};
use crate::library::{GRID_PAGE_SIZE, LibraryController, ViewMode};

mod context_menu;

use self::context_menu::{AssetsDragPreview, asset_context_menu};

use crate::panels::common::{AssetsDrag, display_name, kind_icon, observe_controller};

mod cells;
mod data;
mod interactions;
mod open_with_apps;
mod rows;
mod toolbar;

use cells::{build_cell_element, build_list_row_element, model_source};
use data::{
    Cell, DataKey, Direction, Row, TIMELINE_HEADER_HEIGHT, ViewData, ViewKey, hsla_to_hex,
    recent_picker_colors, total_identity,
};
use rows::{
    materialize_rows, next_cell_row, prev_cell_row, refill_rows, timeline_header, timeline_rows,
};
use toolbar::{
    add_filter_button, color_filter, format_filter, kind_filter, kind_key, rating_filter,
    selection_toolbar, shape_filter, tag_filter, title_controls,
};

/// Fallback layout width before the container has been measured once
/// (assumes a ~1024px window minus the two side docks).
const FALLBACK_WIDTH: f32 = 1024.0 - 590.0;
/// Rows rendered beyond the viewport by the virtualized list, in px.
const LIST_OVERDRAW_PX: f32 = 400.0;
/// Fixed height of one row in list view mode.
const LIST_ROW_HEIGHT: f32 = 44.0;
/// How close (in rows) to the end of the list the next page is requested.
const PAGE_TRIGGER_ROWS: usize = 3;
/// Width jump (in px) that counts as a discrete resize rather than a drag.
///
/// A drag reports a handful of px per frame; a fullscreen toggle, a
/// double-clicked titlebar or a snapped window reports the whole delta at
/// once. Only the drag needs the debounce — deferring a jump means the grid
/// sits stretched with its old column count for the whole debounce window,
/// which reads as a stall followed by a sudden fill-in.
const WIDTH_JUMP_PX: f32 = 120.0;
/// Minimum wall time between two exact COUNT queries for the grid total.
/// Between them the cached number is reused; the data pass fetches items
/// with a lower-bound total instead, keeping the COUNT off the hot path.
const TOTAL_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2000);
/// Delay after a refresh that reused the cached total before one exact
/// re-count runs, so the number always settles once churn (imports, edits)
/// ends instead of staying stale until the next view switch.
const TOTAL_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

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
    /// Framework colour picker state; the element owns its own popover, so
    /// the toolbar row just renders it.
    color_picker: Entity<gpui_kit::base::ColorPickerState>,
    /// A colour confirmed in the picker; consumed by the next render.
    pending_color_search: Option<String>,
    /// Set when a staged colour has been *committed* (the popover closed) and
    /// the next render should search for it. False while the picker is still
    /// open, which is what keeps a slider drag from searching every frame.
    color_search_armed: bool,
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
    /// Which renderer is painting an open model viewport, as the status bar
    /// shows it. `None` when no model preview is open. Carried here — rather
    /// than read on demand by the app view, which cannot reach through the
    /// dock to the panel — and refreshed by the viewport observer, which
    /// only forwards actual changes, so a drag that notifies every frame
    /// costs one string comparison.
    viewport_backend: Option<String>,
    /// The live viewport watcher behind [`WorkspacePanel::viewport_backend`];
    /// dropped with the preview.
    viewport_observer: Option<Subscription>,
    /// In-flight system-font scan for the fonts view; `None` once started
    /// and finished (the result lives on the controller).
    fonts_scan_task: Option<gpui::Task<()>>,
    /// View identity + time of the last exact COUNT for the grid total
    /// (see `TOTAL_REFRESH_INTERVAL`).
    total_refresh: Option<(std::time::Instant, DataKey)>,
    /// Set by the settle timer: the next data pass must re-run the exact
    /// COUNT even though the cached total is otherwise still fresh.
    count_recheck: bool,
    /// Pending settle timer for the re-check above.
    count_settle: Option<gpui::Task<()>>,
    /// Distinct live file extensions for the format filter, keyed by the
    /// controller generation they were read at. The read is a full scan of
    /// the live rows and the toolbar is rebuilt every frame, so it may only
    /// run when the generation moves.
    filter_exts: Option<(u64, Vec<String>)>,
    /// Title-bar label, keyed by the controller generation it was resolved
    /// at. The dock asks for the title outside this panel's own render, so
    /// the collection / smart-collection name lookup behind it would
    /// otherwise run once per frame.
    title_cache: Option<(u64, String)>,
    /// Infinite-scroll guard: the `grid_loaded` cursor the last page request
    /// was issued for. Held across frames on purpose — see the paging trigger
    /// in `render` for why a per-frame flag pages eagerly. `usize::MAX` means
    /// "no request yet", which no real cursor can equal.
    page_guard: Rc<CellFlag<usize>>,
}

impl WorkspacePanel {
    /// Which renderer is painting an open model viewport, for the status
    /// bar. `None` when no model preview is open.
    pub(crate) fn viewport_backend(&self) -> Option<&str> {
        self.viewport_backend.as_deref()
    }

    /// Schedule one exact re-count of the grid total after a refresh that
    /// reused the cached number, so the displayed count catches up once the
    /// churn (import burst, one-off edit) settles.
    fn arm_total_settle(&mut self, cx: &mut Context<Self>) {
        if self.count_settle.is_some() {
            return;
        }
        let panel = cx.entity();
        self.count_settle = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(TOTAL_SETTLE_DELAY).await;
            panel.update(cx, |this, cx| {
                this.count_settle = None;
                this.count_recheck = true;
                cx.notify();
            });
        }));
    }

    /// Kick off the system-font scan the first time the fonts view is
    /// browsed. The scan parses the name table of every font file on the
    /// machine, so it must run on the background executor; when it lands
    /// the generation bump lets the data pass merge the virtual entries.
    fn ensure_system_fonts_scan(&mut self, cx: &mut Context<Self>) {
        let already_scanned = self.controller.read(cx).system_fonts.is_some();
        if already_scanned || self.fonts_scan_task.is_some() {
            return;
        }
        let controller = self.controller.clone();
        self.fonts_scan_task = Some(cx.spawn(async move |_, cx| {
            let scanned = cx
                .background_executor()
                .spawn(async move { trove_core::services::font_manager::scan_system_fonts() })
                .await;
            controller.update(cx, |ctl, cx| {
                ctl.system_fonts = Some(std::sync::Arc::new(scanned));
                ctl.generation += 1;
                cx.notify();
            });
        }));
    }

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

    /// A colour chosen in the picker: stage it, and let the close commit it.
    ///
    /// The picker emits `Change` from two places that mean different things:
    ///
    /// - a palette swatch commits and closes the popover (`select_color`),
    /// - a slider drag streams `Change` on every tick and deliberately keeps
    ///   the popover open (`update_value_from_slider`, "Commits a color
    ///   without changing the open state, as a slider drag does").
    ///
    /// Both arrive as the same event with the same payload, so the drag
    /// cannot be told apart from the commit here. Searching on each `Change`
    /// would therefore replace the grid on every frame of a drag — the "it
    /// confirms before I let go" bug — so this handler only stages the value.
    /// [`Self::on_picker_closed`] does the committing, which also covers
    /// dismissing the popover after a drag (a path that emits no `Change`).
    fn on_color_picked(
        &mut self,
        _: Entity<gpui_kit::base::ColorPickerState>,
        event: &gpui_kit::base::ColorPickerEvent,
        cx: &mut Context<Self>,
    ) {
        // `ColorPickerEvent` has a single variant, so this only has to pick
        // the payload apart. A cleared picker (`None`) means "no colour",
        // which is not a search — leave the grid as it is rather than
        // filtering to nothing.
        let gpui_kit::base::ColorPickerEvent::Change(color) = event;
        let Some(color) = color else {
            return;
        };
        self.pending_color_search = Some(hsla_to_hex(*color));
        // While the popover is open the user is still editing, so nothing is
        // committed yet; the close is what commits.
        self.color_search_armed = color_change_commits(self.color_picker.read(cx).is_open());
        cx.notify();
    }
    /// The picker popover closed: commit whatever colour is staged.
    ///
    /// This is the single commit point. It catches both the palette click
    /// (which closes *and* emits `Change`) and dismissing the popover after
    /// nudging a slider, where the last `Change` landed mid-drag and no
    /// further event ever came.
    fn on_picker_closed(&mut self, cx: &mut Context<Self>) {
        if close_commits(self.pending_color_search.is_some()) {
            self.color_search_armed = true;
            cx.notify();
        }
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

impl WorkspacePanel {
    /// The in-panel toolbar row below the title bar. While a visual search
    /// is active it collapses to the result-mode chip and an exit button;
    /// otherwise it shows the colour picker (colour search), the user's
    /// enabled filter tools (kind / tag / shape / rating / format), and the
    /// "+" button that toggles the set.
    fn toolbar_row(&mut self, cx: &mut Context<Self>) -> Div {
        let ctl = self.controller.read(cx);
        if let Some(visual) = ctl.visual_results.clone() {
            let controller = self.controller.clone();
            let count = visual.hits.len();
            return h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(
                            rust_i18n::t!("workspace.visual_title").to_string()
                                + " · "
                                + &visual.label,
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!("workspace.visual_count", count = count).to_string()),
                )
                .child(
                    Button::new("exit-visual-search")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(rust_i18n::t!("workspace.exit_visual").to_string())
                        .on_click(move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                ctl.close_visual_search();
                                cx.notify();
                            });
                        }),
                );
        }
        let in_trash = ctl.showing_trash;
        let in_recent = ctl.showing_recent;
        let search_active = !in_trash && !in_recent && !ctl.search_text.trim().is_empty();
        let controller = self.controller.clone();
        let color_picker = self.color_picker.clone();
        // The extension list the format filter offers. Reading it means a full
        // scan of the live rows (32 ms on a 100k library) and this row is built
        // every frame, so it is refilled only when the generation moves.
        let generation = self.controller.read(cx).generation;
        if self.filter_exts.as_ref().map(|(cached, _)| *cached) != Some(generation) {
            let conn = self.controller.read(cx).library.store().conn();
            let exts = trove_core::store::assets::distinct_exts(conn).unwrap_or_default();
            self.filter_exts = Some((generation, exts));
        }
        let exts: &[String] = match &self.filter_exts {
            Some((_, exts)) => exts.as_slice(),
            None => &[],
        };
        // Which filter tools the user enabled; recomputed per render so a
        // toggle in the "+" menu applies immediately.
        let enabled_tools = trove_core::config::AppConfig::load().filter_tools();
        let tool_enabled =
            |tool: &str| !in_trash && !in_recent && enabled_tools.iter().any(|t| t == tool);
        h_flex()
            .w_full()
            .items_center()
            .gap_1()
            // Colour filter sits at the far left, then the kind filter.
            .when(!in_trash && !in_recent, |row| {
                // Trigger and popover both live in `toolbar::color_filter`;
                // recent colours ride along as the featured row, so the colours
                // the user actually reaches for stay one click away.
                row.child(color_filter(&color_picker, recent_picker_colors(cx), cx))
            })
            .when(tool_enabled("kind"), |row| {
                row.child(kind_filter(&controller, cx))
            })
            .when(tool_enabled("tag"), |row| {
                row.child(tag_filter(&controller, cx))
            })
            .when(tool_enabled("shape"), |row| {
                row.child(shape_filter(&controller, cx))
            })
            .when(tool_enabled("rating"), |row| {
                row.child(rating_filter(&controller, cx))
            })
            .when(tool_enabled("format"), |row| {
                row.child(format_filter(exts, &controller, cx))
            })
            .when(!in_trash && !in_recent, |row| {
                row.child(add_filter_button(&controller))
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

/// Ask the platform for the frame that draws a relayout armed this frame.
///
/// Marking state dirty is not a frame request, and on Wayland that difference
/// is fatal: frames arrive only from `schedule_frame()` and the compositor's
/// `wl_surface.frame` callbacks, and nothing else can produce one — Linux
/// leaves `PlatformWindow::frame_waker()` at its `None` default
/// (`gpui-pre/src/platform.rs:854`), so the waker `Window::refresh` and
/// `Window::invalidate_view` would call (`window.rs:167-191`, `2179`) does not
/// exist. X11 hides that: its client invokes the frame callback every vblank
/// (`x11/client.rs:1990` -> `x11/window.rs:1177`), so a dirty window is drawn
/// within a frame no matter what. Wayland has no such poll, so an armed
/// relayout waits for the next input event to drag a frame out of the window —
/// "toggle fullscreen, then click, and only then does the grid fill in".
///
/// `Window::on_next_frame` is the primitive that cannot be dropped: it calls
/// `platform_window.schedule_frame()` synchronously (`window.rs:2507-2510`)
/// while the current frame is still ticking, which Wayland turns into a
/// reschedule, i.e. a one-vblank retry (`wayland/window.rs:1013-1026`,
/// `976-981`; `FRAME_RETRY_INTERVAL` = 16.7 ms). The callback then marks the
/// window dirty again (`App::refresh_windows`, `app.rs:1808-1814`), so the
/// frame it produces always draws a relayout that was armed this frame —
/// `Context::notify` on its own only sets a dirty bit, and a relayout must not
/// depend on which of the two survived.
fn arm_relayout_frame(window: &mut Window) {
    window.on_next_frame(|_window, cx| cx.refresh_windows());
}

/// Whether a `Change` that arrived while the picker was in state `open` is a
/// commit worth searching for.
///
/// The picker emits `Change` for both a palette click and every tick of a
/// slider drag, and the two are indistinguishable by payload. Only the open
/// state tells them apart: `select_color` shuts the popover *before* it
/// emits, whereas a drag leaves it open ("Commits a color without changing
/// the open state, as a slider drag does"). So a change that finds the
/// popover shut is the commit, and one that finds it open is mid-drag.
fn color_change_commits(picker_is_open: bool) -> bool {
    !picker_is_open
}

/// Whether closing the picker should commit the staged colour.
///
/// A close is always a commit *if* something was staged. Dismissing the
/// popover after a drag emits no `Change`, so without this the colour the
/// user just dialled in would be silently dropped.
fn close_commits(staged: bool) -> bool {
    staged
}

/// Whether an `is_open` observation is the true→false edge.
///
/// `set_open` only calls `notify`, so the close is detected by watching the
/// flag. The opening edge must not count: it is always followed by another
/// choice, and firing there would search for a colour the user had not
/// finished picking.
fn picker_just_closed(previously_open: bool, now_open: bool) -> bool {
    previously_open && !now_open
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
            filter_orientation,
            filter_aspect,
            filter_min_rating,
            filter_ext,
            view_mode,
            sort,
            sort_desc,
            active_folder,
            visual_ids,
            visual_label,
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
                ctl.filter_orientation,
                ctl.filter_aspect,
                ctl.filter_min_rating,
                ctl.filter_ext.clone(),
                ctl.view_mode,
                ctl.sort,
                ctl.sort_desc,
                ctl.active_folder.clone(),
                ctl.visual_results.as_ref().map(|r| r.ids.clone()),
                ctl.visual_results.as_ref().map(|r| r.label.clone()),
            )
        };
        let library_root = self.controller.read(cx).library.root().to_path_buf();

        // Fonts view: make sure the system-font scan is on its way so the
        // virtual entries can merge in once it lands.
        if !in_trash && !in_recent && filter_kind == Some(AssetKind::Font) && visual_ids.is_none() {
            self.ensure_system_fonts_scan(cx);
        }

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
            filter_orientation,
            filter_aspect,
            filter_min_rating,
            filter_ext,
            sort,
            sort_desc,
            grid_loaded,
            library_root: library_root.clone(),
            generation: self.controller.read(cx).generation,
            visual: visual_ids.clone(),
        };
        if self.data.as_ref().is_none_or(|d| d.key != data_key) {
            // Decide whether this refresh pays for the exact COUNT. Rapid
            // refreshes (import ticks, one-off edits) reuse the cached
            // total; the settle timer guarantees one exact pass once the
            // churn ends, so the number never settles stale.
            let identity = total_identity(&data_key);
            let now = std::time::Instant::now();
            let need_count = self.total_refresh.as_ref().is_none_or(|(at, key)| {
                *key != identity || now.duration_since(*at) >= TOTAL_REFRESH_INTERVAL
            }) || self.count_recheck;
            self.count_recheck = false;
            if need_count {
                self.total_refresh = Some((now, identity));
            }
            let (pass_total, cells) = self.run_data_pass(cx, &data_key, need_count);
            let total = if need_count {
                pass_total
            } else {
                self.arm_total_settle(cx);
                self.data.as_ref().map(|d| d.total).unwrap_or(pass_total)
            };
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
        // A large one-shot width change is a discrete resize (fullscreen
        // button, double-clicked titlebar, snapping), not a drag: it lands in
        // a single frame with the whole delta. Deferring that one leaves the
        // grid drawn at the old column count inside a stretched viewport until
        // the debounce fires, which is exactly the "it pauses, then clicking
        // fills it in" stall. Drags stay debounced — they arrive a few px per
        // frame, so they never clear this threshold.
        let jumped_width = self
            .view_key
            .as_ref()
            .is_some_and(|k| (k.content_width - content_width).abs() > WIDTH_JUMP_PX);
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
                || k.visual != visual_ids
        });

        // Structural changes (view / filter / asset set) always relayout now;
        // a size-only change (width or zoom) is deferred until the resize /
        // slider drag settles, at which point the timer sets
        // `relayout_pending` and the next render applies it.
        let structural_changed = other_changed || self.covered != cells.len();
        let defer_layout =
            !structural_changed && width_changed && !jumped_width && !self.relayout_pending;

        if defer_layout && self.debounce_timer.is_none() {
            self.debounce_timer = Some(cx.spawn_in(window, async move |panel, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                // The pending flag is state, not a request for a frame, and
                // once a resize settles nothing else is necessarily in
                // flight, so the timer has to ask for the frame itself.
                panel
                    .update_in(cx, |this, window, cx| {
                        this.relayout_pending = true;
                        this.debounce_timer = None;
                        cx.notify();
                        arm_relayout_frame(window);
                    })
                    .ok();
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
            visual: visual_ids.clone(),
        };
        // The debounce settled on a width that already equals the applied one
        // (resize flickered back): clear the pending flag so future resizes
        // debounce again instead of jumping immediately.
        if self.relayout_pending && self.view_key.as_ref() == Some(&key) {
            self.relayout_pending = false;
        }
        // Whether either branch below rebuilt the frozen rows. The published
        // visible-id list has to follow the rows, but only when they move.
        let mut layout_changed = false;
        // A view change resets `grid_loaded` back to the first page, which can
        // land on a cursor the guard already served (the tail of the previous
        // view). Forget that cursor so the new view's first page can still be
        // requested when its end row comes into view.
        if other_changed {
            self.page_guard.set(usize::MAX);
        }
        if self.view_key.as_ref() != Some(&key) && !defer_layout {
            // View, width or zoom changed: full layout. Structural changes
            // reset scrolling (a new view starts at the top), but a
            // width/zoom-only change merely re-justifies the same assets —
            // remember what is on screen and restore it below, so a resize
            // or a slider drag does not throw the user back to the top.
            layout_changed = true;
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
            layout_changed = true;
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

        // Publish the visible ids for Edit ▸ Select-all / Shift-range. Only
        // when the rows actually moved: flattening them is O(assets), and
        // both consumers are user gestures rather than per-frame reads.
        if layout_changed {
            let visible: Vec<Uuid> = rows
                .iter()
                .flat_map(|r| r.cells.iter().map(|c| c.id))
                .collect();
            self.controller.update(cx, |ctl, _| {
                ctl.set_visible_assets(visible);
            });
        }

        // --- virtualized list -----------------------------------------------------
        let list_state = self.list_state.clone();
        let controller = self.controller.clone();
        let toolbar_controller = controller.clone();
        let focus_handle = self.focus_handle.clone();
        let rows_for_render = rows.clone();
        let rows_len = rows.len();
        let list_mode = view_mode == ViewMode::List;
        // One page request per `grid_loaded` value at most. The guard lives on
        // the panel and remembers the cursor the last request was issued for,
        // rather than being rebuilt (and thus cleared) every frame: with a
        // per-frame flag the trigger re-arms on every re-render while the end
        // row is still on screen, so anything that re-renders repeatedly —
        // import churn, a resize, the frames the request itself schedules —
        // paged the grid forward on its own instead of following the user's
        // scrolling. Keying on the cursor also means it re-arms exactly when a
        // page lands, which is the one moment another request is legitimate.
        let page_guard = self.page_guard.clone();
        let total_for_trigger = total;

        let grid = list_element(list_state, move |ix, _window, cx: &mut App| {
            // Infinite scroll: near the end, request the next page.
            if ix + PAGE_TRIGGER_ROWS >= rows_len {
                let loaded = controller.read(cx).grid_loaded;
                if loaded < total_for_trigger && page_guard.get() != loaded {
                    page_guard.set(loaded);
                    controller.update(cx, |ctl, cx| {
                        ctl.grid_loaded = (ctl.grid_loaded + GRID_PAGE_SIZE).min(total_for_trigger);
                        cx.notify();
                    });
                }
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
            if let Some(label) = &visual_label {
                rust_i18n::t!("workspace.no_results", query = label).to_string()
            } else if search_active {
                rust_i18n::t!("workspace.no_results", query = search).to_string()
            } else {
                rust_i18n::t!("workspace.no_assets_hint").to_string()
            }
        } else {
            String::new()
        };

        // A colour confirmed in the picker opens a colour search; render has
        // the window the dialog needs. Only a committed pick runs — a slider
        // drag keeps the popover open and must not replace the grid until the
        // user lets go (see `on_color_picked`).
        if self.color_search_armed
            && let Some(hex) = self.pending_color_search.take()
        {
            self.color_search_armed = false;
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
                        move |bounds: Bounds<Pixels>, window, cx| {
                            let w = bounds.size.width;
                            let mut measured_changed = false;
                            available_width.update(cx, |width, _| {
                                if *width != w {
                                    *width = w;
                                    measured_changed = true;
                                }
                            });
                            if measured_changed {
                                // Width changes re-layout the grid; wake the
                                // controller so panels re-render, and ask for
                                // the frame that applies the new measurement —
                                // `render` reads `available_width` a frame
                                // *ahead* of it landing here, so the relayout
                                // always needs one more frame.
                                controller.update(cx, |_, cx| cx.notify());
                                arm_relayout_frame(window);
                            }
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
    use super::{
        Cell, Row, close_commits, color_change_commits, next_cell_row, picker_just_closed,
        prev_cell_row, timeline_rows,
    };
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
            system_font: false,
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

    /// The bug this pins: `Change` fires on every slider tick, so treating
    /// each one as a commit replaced the grid mid-drag.
    #[test]
    fn a_slider_drag_does_not_commit_but_a_palette_click_does() {
        // Mid-drag: the popover is still open, so nothing is committed.
        assert!(!color_change_commits(true));
        // `select_color` closes the popover before emitting.
        assert!(color_change_commits(false));
    }

    /// The other half: after a drag the last `Change` arrived while the
    /// popover was open, and dismissing it emits no further event — so the
    /// staged colour has to be committed on the close instead of dropped.
    #[test]
    fn a_close_commits_the_colour_staged_by_a_drag() {
        assert!(close_commits(true));
        // Nothing staged means the user opened the picker and left; the grid
        // must not change.
        assert!(!close_commits(false));
    }

    /// Only the true→false edge is a close. Observing an open would search
    /// for a colour the user had not chosen yet.
    #[test]
    fn only_the_closing_edge_counts() {
        assert!(picker_just_closed(true, false));
        assert!(!picker_just_closed(false, true), "opening is not closing");
        assert!(!picker_just_closed(false, false));
        assert!(!picker_just_closed(true, true), "still open is not closing");
    }
}
