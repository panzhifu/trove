//! Grid data model and the paged query pass.
//!
//! [`Cell`]/[`Row`] are the frozen per-layout-epoch structures the
//! virtualized list renders; `ViewKey`/`DataKey` decide when the row
//! structure and the cached query result are rebuilt.

use super::*;

#[derive(Debug, Clone)]
/// Data needed to paint one grid cell. Immutable per layout epoch;
/// `selected` is resolved live at paint time so selection changes never
/// rebuild rows.
pub(super) struct Cell {
    pub(super) id: Uuid,
    pub(super) kind: AssetKind,
    pub(super) thumb: Option<PathBuf>,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
    pub(super) trashed: bool,
    /// Display name + facts for the list view rows.
    pub(super) name: String,
    pub(super) size_bytes: u64,
    pub(super) added: String,
    /// Timeline bucket: the day this asset belongs to as `YYYY-MM-DD`.
    /// Capture date when the file carries one, import date otherwise.
    pub(super) day: String,
    /// Live font preview: family + the font file to register, set only for
    /// Font assets so cells can render the sample text in the actual font.
    pub(super) font_family: Option<String>,
    pub(super) font_blob: Option<PathBuf>,
}

impl Cell {
    pub(super) fn aspect(&self) -> f32 {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => w as f32 / h as f32,
            _ => 1.0,
        }
    }
}

/// One frozen justified row: uniform height, cell widths in display order.
///
/// A row may also carry `header` instead of cells: the timeline view inserts
/// one such row per day section.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub(super) height: f32,
    pub(super) widths: Vec<f32>,
    pub(super) cells: Vec<Cell>,
    pub(super) header: Option<String>,
}

/// Height of a timeline day header.
pub(super) const TIMELINE_HEADER_HEIGHT: f32 = 34.0;

impl Row {
    /// A timeline section header: no cells, just the label.
    pub(super) fn section(label: String) -> Self {
        Self {
            height: TIMELINE_HEADER_HEIGHT,
            widths: Vec::new(),
            cells: Vec::new(),
            header: Some(label),
        }
    }
    /// Horizontal center of each cell (for up/down nearest-column moves).
    pub(super) fn centers(&self) -> Vec<f32> {
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
pub(super) struct ViewKey {
    pub(super) collection: Option<Uuid>,
    pub(super) in_trash: bool,
    pub(super) in_recent: bool,
    pub(super) smart: Option<Uuid>,
    pub(super) tag: Option<Uuid>,
    pub(super) folder: Option<String>,
    pub(super) search: String,
    pub(super) filter_kind: Option<AssetKind>,
    pub(super) filter_favorite: bool,
    pub(super) view_mode: ViewMode,
    pub(super) sort: AssetSort,
    pub(super) sort_desc: bool,
    pub(super) content_width: f32,
    /// Grid zoom (multiplier on the ideal row height); part of the key so a
    /// slider change re-justifies through the same debounced path as a
    /// resize.
    pub(super) row_height_scale: f32,
}

/// Inputs that decide *which* assets are listed. This is the expensive
/// part — one SQL query plus one file stat per asset — so it is cached in
/// [`ViewData`]; a resize re-renders every frame without touching any of
/// these. `generation` is the library mutation counter, so imports, edits
/// and trash operations invalidate the cache.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DataKey {
    pub(super) collection: Option<Uuid>,
    pub(super) in_trash: bool,
    pub(super) in_recent: bool,
    pub(super) smart: Option<Uuid>,
    pub(super) tag: Option<Uuid>,
    pub(super) folder: Option<String>,
    pub(super) search: String,
    pub(super) filter_kind: Option<AssetKind>,
    pub(super) filter_favorite: bool,
    pub(super) sort: AssetSort,
    pub(super) sort_desc: bool,
    pub(super) grid_loaded: usize,
    pub(super) library_root: PathBuf,
    pub(super) generation: u64,
}

/// Cached data pass: the query result materialized into cells, shared with
/// the row builders through an `Rc`.
#[derive(Debug, Clone)]
pub(super) struct ViewData {
    pub(super) key: DataKey,
    pub(super) cells: Rc<Vec<Cell>>,
    pub(super) total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl WorkspacePanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let search_box = cx.new(|cx| SearchBox::new(window, cx, controller.clone()));
        let available_width = cx.new(|_| px(0.));
        let list_state = ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW_PX));
        // Grid-zoom slider: live label while dragging, the row layout
        // commits once on release (the debounced relayout path applies it).
        let initial_scale = controller.read(cx).row_height_scale;
        let zoom_slider = cx.new(|_| {
            SliderState::new()
                .min(0.6)
                .max(1.8)
                .step(0.05)
                .default_value(initial_scale)
        });
        cx.subscribe_in(&zoom_slider, window, Self::on_zoom_slider)
            .detach();
        let color_picker = cx.new(|cx| ColorPickerState::new(None, window, cx));
        cx.subscribe(
            &color_picker.clone(),
            |this, _, picked: &ColorPicked, cx| {
                this.pending_color_search = Some(picked.0.clone());
                cx.notify();
            },
        )
        .detach();
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            search_box,
            color_picker,
            color_picker_open: std::cell::Cell::new(false),
            pending_color_search: None,
            available_width,
            rows: Rc::new(Vec::new()),
            list_state,
            zoom_slider,
            view_key: None,
            data: None,
            covered: 0,
            last_total: 0,
            relayout_pending: false,
            debounce_timer: None,
            preview: None,
            preview_subscription: None,
        };
        observe_controller(cx, &this.controller);
        this
    }

    /// Run the paged query for `key` and materialize the assets into cells.
    /// The five mutually-exclusive view drivers (recent, FTS search, smart
    /// collection, plain query) live here. Called only when the [`DataKey`]
    /// changes — never on the per-frame path.
    pub(super) fn run_data_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
    ) -> (usize, Vec<Cell>) {
        let limit = Some(key.grid_loaded as u32);
        let search_active = !key.in_trash && !key.in_recent && !key.search.is_empty();
        let (total, list): (usize, Vec<trove_core::model::Asset>) = if key.in_recent {
            // Recently viewed: ids ordered by last view time, materialised
            // in that order (missing / trashed ids are dropped by the
            // query). History is capped at 200, so one page covers it all.
            let conn = self.controller.read(cx).library.store().conn();
            match trove_core::store::view_history::recent_ids(conn, key.grid_loaded).and_then(
                |ids| {
                    let n = ids.len();
                    assets::by_ids(conn, &ids).map(|a| (n, a))
                },
            ) {
                Ok((t, a)) => (t, a),
                Err(e) => {
                    self.report_view_error(cx, e);
                    (0, Vec::new())
                }
            }
        } else if search_active {
            let q = AssetQuery {
                collection_id: key.collection,
                tag_ids: key.tag.map(|t| vec![t]).unwrap_or_default(),
                kind: key.filter_kind,
                is_favorite: key.filter_favorite.then_some(true),
                source_path_prefix: key.folder.clone(),
                is_trashed: false,
                text: None,
                limit,
                ..Default::default()
            };
            let result = assets::search(
                self.controller.read(cx).library.store().conn(),
                &key.search,
                &q,
            );
            match result {
                Ok((t, a)) => (t as usize, a),
                Err(e) => {
                    self.report_view_error(cx, e);
                    (0, Vec::new())
                }
            }
        } else if let Some(sid) = key.smart {
            let result = self.controller.read(cx).library.evaluate_smart_collection(
                sid,
                key.filter_kind,
                key.filter_favorite.then_some(true),
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
                    collection_id: if key.in_trash { None } else { key.collection },
                    tag_ids: if key.in_trash {
                        Vec::new()
                    } else {
                        key.tag.map(|t| vec![t]).unwrap_or_default()
                    },
                    // The trash view hides the filter controls, so it also
                    // ignores the grid filters entirely.
                    kind: if key.in_trash { None } else { key.filter_kind },
                    is_favorite: (!key.in_trash && key.filter_favorite).then_some(true),
                    source_path_prefix: if key.in_trash {
                        None
                    } else {
                        key.folder.clone()
                    },
                    is_trashed: key.in_trash,
                    sort: key.sort,
                    sort_desc: key.sort_desc,
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

        let cells: Vec<Cell> = list
            .iter()
            .filter(|a| key.in_trash || a.trashed_at.is_none())
            .map(|a| {
                let thumb = a
                    .sha256
                    .as_deref()
                    .map(|sha| trove_core::media::thumb::abs_path(&key.library_root, sha))
                    .filter(|p| p.is_file());
                // Live font preview inputs: family (probed at import) plus
                // the font file to register (blob or linked source).
                let (font_family, font_blob) = if a.kind == AssetKind::Font {
                    let blob = if a.origin == trove_core::model::Origin::Linked {
                        a.extra
                            .get("source_path")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from)
                    } else {
                        a.rel_path.as_ref().map(|rel| key.library_root.join(rel))
                    }
                    .filter(|p| p.is_file());
                    (
                        a.extra
                            .get("font_family")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        blob,
                    )
                } else {
                    (None, None)
                };
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
                    // Capture date is what a timeline is about; files without
                    // EXIF fall back to the import date so nothing is lost.
                    day: a
                        .captured_at
                        .unwrap_or(a.created_at)
                        .format("%Y-%m-%d")
                        .to_string(),
                    font_family,
                    font_blob,
                }
            })
            .collect();
        (total, cells)
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
}
