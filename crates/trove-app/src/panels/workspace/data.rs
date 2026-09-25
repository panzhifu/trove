//! Grid data model and the paged query pass.
//!
//! [`Cell`]/[`Row`] are the frozen per-layout-epoch structures the
//! virtualized list renders; `ViewKey`/`DataKey` decide when the row
//! structure and the cached query result are rebuilt.

use super::*;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use trove_core::model::Asset;
use trove_core::search::{expression::Target, highlight::Lexicon};
use trove_core::store::{BrowseContext, BrowseSession};

/// The surfaces a listing row shows: one string, the title when the asset has
/// one and the file name when it does not. A term qualified against any other
/// surface matched somewhere the row does not display, so it stays unmarked.
pub(super) const ROW_TARGETS: &[Target] = &[Target::All, Target::Name, Target::Title];

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
    /// Byte ranges of `name` holding a word the active search asked for.
    /// Empty unless a search is running, which is also the only time the list
    /// view's names are read as anything but a label.
    pub(super) name_marks: Vec<Range<usize>>,
    /// How close this asset was to what a visual search asked for, 0.0–1.0.
    /// `None` for every ordinary listing: a browse has no score to show, and a
    /// column that is empty for the whole view is worse than no column.
    pub(super) score: Option<f32>,
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
///
/// The two lists are reference-counted slices because a row is cloned twice
/// for reasons that have nothing to do with its content: every visible row's
/// cells are cloned into the element closure each frame, and appending a page
/// clones the head rows to keep them (see `rows::append_rows`). A `Vec` here
/// would re-allocate the whole listing on both paths.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub(super) height: f32,
    pub(super) widths: Rc<[f32]>,
    pub(super) cells: Rc<[Cell]>,
    pub(super) header: Option<String>,
}

/// Height of a timeline day header.
pub(super) const TIMELINE_HEADER_HEIGHT: f32 = 34.0;

impl Row {
    /// A timeline section header: no cells, just the label.
    pub(super) fn section(label: String) -> Self {
        Self {
            height: TIMELINE_HEADER_HEIGHT,
            widths: Rc::from(Vec::new()),
            cells: Rc::from(Vec::new()),
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
    /// Active visual search (mirrors [`DataKey::visual`]): a structural
    /// change of the hit set relayouts and resets scrolling. Shared with the
    /// controller's cached id list: both keys are rebuilt every frame, so a
    /// plain `Vec` here would deep-copy up to CANDIDATE_CAP ids per frame.
    pub(super) visual: Option<Rc<Vec<Uuid>>>,
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
    pub(super) filter_orientation: Option<Orientation>,
    pub(super) filter_aspect: Option<trove_core::model::AspectPreset>,
    pub(super) filter_resolution: Option<trove_core::model::ResolutionBand>,
    pub(super) filter_min_rating: Option<u8>,
    pub(super) filter_ext: Option<String>,
    pub(super) sort: AssetSort,
    pub(super) sort_desc: bool,
    /// The library's data root: where stored blobs live.
    pub(super) library_root: PathBuf,
    /// Its cache root: where thumbnails live. Separate because the two move
    /// independently — `rm -rf` of the cache costs a rebuild and nothing else.
    pub(super) cache_root: PathBuf,
    pub(super) generation: u64,
    /// Active visual search: the grid shows exactly these asset ids (in
    /// rank order) instead of running the browse query.
    pub(super) visual: Option<Rc<Vec<Uuid>>>,
}

/// Cached data pass: the query result materialized into cells, shared with
/// the row builders through an `Rc`.
#[derive(Debug, Clone)]
pub(super) struct ViewData {
    pub(super) key: DataKey,
    pub(super) cells: Rc<Vec<Cell>>,
    pub(super) total: usize,
    /// `total` is a floor: the query ran out of candidates before it had seen
    /// everything matching. The title bar says so.
    pub(super) truncated: bool,
    /// The listing the next page is cut from. `None` for a visual search, whose
    /// id list is frozen on the controller instead.
    pub(super) session: Option<BrowseSession>,
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
        // The one live card. A frame arriving from its decode loop is a
        // `notify` on that entity, and `observe` is what turns it into a
        // repaint of the grid.
        let quick_look = cx.new(|_| LiveCard::new());
        cx.observe(&quick_look, |_, _, cx| cx.notify()).detach();
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
        // The colour filter's similarity rail. Like the zoom slider it commits
        // on release: the value is a question width, and re-running the colour
        // scan on every tick of a drag would replace the grid mid-gesture.
        let initial_similarity = controller.read(cx).colour_similarity;
        let colour_similarity_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value(initial_similarity)
        });
        cx.subscribe_in(
            &colour_similarity_slider,
            window,
            Self::on_colour_similarity_slider,
        )
        .detach();
        // The framework picker: its palette tab carries the whole nine-family
        // ramp (plus whatever recent colours we feed it), which the local
        // hand-rolled panel never had.
        let color_picker = cx.new(|cx| gpui_kit::base::ColorPickerState::new(window, cx));
        cx.subscribe(&color_picker, Self::on_color_picked).detach();
        // `set_open` only calls `notify`, so the close has to be observed
        // rather than subscribed to. Only the true→false edge commits: an
        // open is always followed by another choice, and committing on it
        // would fire a search the user never finished making.
        let was_open = Rc::new(CellFlag::new(false));
        cx.observe(&color_picker, move |this, picker, cx| {
            let open = picker.read(cx).is_open();
            let previously_open = was_open.replace(open);
            if super::picker_just_closed(previously_open, open) {
                this.on_picker_closed(cx);
            }
        })
        .detach();
        let this = Self {
            focus_handle: cx.focus_handle(),
            grid_focus: cx.focus_handle(),
            controller,
            quick_look,
            search_box,
            color_picker,
            pending_color_search: None,
            color_search_armed: false,
            available_width,
            rows: Rc::new(Vec::new()),
            list_state,
            zoom_slider,
            colour_similarity_slider,
            view_key: None,
            data: None,
            covered: 0,
            last_total: 0,
            last_truncated: false,
            page_finished: false,
            relayout_pending: false,
            debounce_timer: None,
            preview: None,
            preview_subscription: None,
            preview_asset_ids: Vec::new(),
            preview_index: 0,
            viewport_backend: None,
            viewport_observer: None,
            total_refresh: None,
            count_recheck: false,
            count_settle: None,
            filter_exts: None,
            title_cache: None,
            page_guard: Rc::new(CellFlag::new(usize::MAX)),
        };
        observe_controller(cx, &this.controller);
        // Quick look follows the card. Once the space bar has lit one, moving the
        // selection — an arrow key or a click, both of which arrive here as a
        // controller notification — moves the light with it, so a run of clips
        // plays through without a key press per tile. The alternative is a card
        // still playing two rows up while the selection has long since moved on.
        cx.observe(&this.controller, |this, _, cx| {
            if !this.quick_look.read(cx).is_on() {
                return;
            }
            let quick_look = this.quick_look.clone();
            let controller = this.controller.clone();
            // Nothing selected is nothing to look at, and the card goes out.
            let Some(id) = this.controller.read(cx).primary() else {
                quick_look.update(cx, |cards, cx| {
                    cards.off(cx);
                });
                return;
            };
            let Some(kind) = this.cell_kind(id) else {
                return;
            };
            quick_look.update(cx, |cards, cx| {
                if !cards.is_live(id) {
                    cards.point_to(id, kind, &controller, cx);
                }
            });
        })
        .detach();
        this
    }

    /// Freeze this view's listing and materialize its first window.
    ///
    /// The [`BrowseSession`] that comes back is what every later page comes
    /// from: a ranked listing would otherwise run the text leg and its
    /// `id IN (…)` intersection again for each screenful the user scrolls into,
    /// for an answer that cannot change inside one listing. The view dispatch
    /// itself lives in `trove_core::store::BrowseContext`; this only maps the
    /// render-side [`DataKey`] onto it and surfaces query errors, and runs only
    /// when the key changes — never on the per-frame path.
    ///
    /// `count_total = false` skips the exact COUNT, so the total is a lower
    /// bound and the caller overlays its cached number.
    pub(super) fn run_data_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
        count_total: bool,
        window: usize,
    ) -> (usize, Vec<Cell>, bool, Option<BrowseSession>) {
        if let Some(ids) = &key.visual {
            let (total, cells, truncated) = self.run_visual_pass(cx, key, ids, 0, window);
            return (total, cells, truncated, None);
        }

        let ctl = self.controller.read(cx);
        let ctx = BrowseContext {
            collection: key.collection,
            in_trash: key.in_trash,
            in_recent: key.in_recent,
            smart: key.smart,
            tag: key.tag,
            folder: key.folder.clone(),
            search: key.search.clone(),
            // The AI plan for this term, when the planner tier is on and it
            // has answered. `browse` consults it only when `tiers.ai` is set.
            ai_plan: ctl.ai_plan.clone(),
            kind: key.filter_kind,
            is_favorite: key.filter_favorite,
            orientation: key.filter_orientation,
            aspect: key.filter_aspect,
            resolution: key.filter_resolution,
            min_rating: key.filter_min_rating,
            ext: key.filter_ext.clone(),
            sort: key.sort,
            sort_desc: key.sort_desc,
            // The window is taken from the session this call freezes, not from
            // the browse itself — `offset` is for the one-shot `run` paths.
            offset: 0,
            // The vector leg of a hybrid search, when the app holds one for
            // the current term. `None` — no endpoint configured, not fetched
            // yet, or a vector for a term the user has typed past — is the
            // ordinary text-only search; the query checks the term itself.
            vector: ctl.query_vector.clone(),
            // Which search legs may run, resolved from the config onto the
            // controller.
            tiers: ctl.search_tiers,
        };
        let conn = ctl.library.store().conn();
        // Flush pending outbox rows first so a just-finished write (import,
        // edit) is reflected in the same refresh. A failed drain (a writer
        // holding the store's write lock past the busy timeout) still renders
        // the page — a stale browse beats no browse — but the staleness must
        // not be silent: the log records it and the notice below names it;
        // the resident drain loop and the next refresh both retry.
        let drained = ctl.library.drain_search_queue();
        let text_index = ctl.library.text_index();
        // The index the vector leg scores against — the same cached one the
        // semantic search uses, fetched only when this pass has a vector and
        // the semantic tier is actually on.
        let vector_index = ctx
            .vector
            .as_ref()
            .filter(|_| ctx.tiers.semantic)
            .map(|query| ctl.library.cached_vector_index(&query.model, query.space));
        let frozen = ctx
            .snapshot(conn, text_index, vector_index.as_ref(), count_total)
            .and_then(|session| {
                session
                    .page(conn, text_index, 0, Some(window))
                    .map(|page| (session, page))
            });
        if let Err(error) = drained {
            tracing::warn!(%error, "search outbox drain failed before a browse refresh");
            let msg =
                rust_i18n::t!("workspace.index_sync_failed", error = error.to_string()).to_string();
            self.report_notice(cx, msg);
        }
        match frozen {
            Ok((session, page)) => {
                let (total, truncated) = (page.total as usize, page.truncated);
                (total, cells_for(key, &page.items), truncated, Some(session))
            }
            Err(error) => {
                self.report_view_error(cx, error);
                (0, Vec::new(), false, None)
            }
        }
    }

    /// One more window of a listing that is already frozen.
    ///
    /// No query, no drain, no re-ranking: the session holds the answer, so a
    /// page costs the slice of it plus the rows that slice needs. A visual
    /// search has no session — its id list is already frozen on the controller —
    /// and windows through the same slice. Returns the cells and whether the
    /// window came back empty, which is the caller's sign to stop paging.
    pub(super) fn run_page_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
        session: Option<&BrowseSession>,
        offset: usize,
        window: usize,
    ) -> (Vec<Cell>, bool) {
        let Some(session) = session else {
            let Some(ids) = &key.visual else {
                // Nothing to page and nothing frozen: the caller stops asking.
                return (Vec::new(), true);
            };
            let (_, cells, _) = self.run_visual_pass(cx, key, ids, offset, window);
            let empty = cells.is_empty();
            return (cells, empty);
        };
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();
        let page = session.page(conn, ctl.library.text_index(), offset, Some(window));
        match page {
            Ok(page) => {
                let empty = page.items.is_empty();
                (cells_for(key, &page.items), empty)
            }
            Err(error) => {
                self.report_view_error(cx, error);
                (Vec::new(), true)
            }
        }
    }

    /// Materialize one window of the visual-search hits into cells, preserving
    /// rank order. Missing records (deleted since the scan) drop out, which is
    /// why a window can come back shorter than asked without being the end of
    /// the list — the caller stops on an *empty* window, not a short one.
    fn run_visual_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
        ids: &[Uuid],
        offset: usize,
        window: usize,
    ) -> (usize, Vec<Cell>, bool) {
        let conn = self.controller.read(cx).library.store().conn();
        let slice = &ids[offset.min(ids.len())..ids.len().min(offset + window)];
        let by_id: HashMap<Uuid, _> = assets::by_ids(conn, slice)
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
        let marks = Lexicon::from_query(&key.search);
        // The score each hit came back with. A visual search is a ranked answer
        // and "how close was this one" is the one thing about it the grid can
        // say — without it the list looks sorted by nothing in particular.
        let scores: HashMap<Uuid, f32> = self
            .controller
            .read(cx)
            .visual_results
            .as_ref()
            .map(|results| results.hits.iter().cloned().collect())
            .unwrap_or_default();
        let cells: Vec<Cell> = slice
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|a| {
                let mut cell = cell_from_asset(&key.library_root, &key.cache_root, a, &marks);
                cell.score = scores.get(&a.id).copied();
                cell
            })
            .collect();
        // The hit list is the whole truth of a visual search — it is the
        // ranking that was asked for, and it has no count behind it.
        (ids.len(), cells, false)
    }

    /// Surface a view/query failure in the status bar.
    fn report_view_error(&mut self, cx: &mut Context<Self>, error: impl std::fmt::Display) {
        let msg = rust_i18n::t!("workspace.query_failed", error = error.to_string()).to_string();
        self.report_notice(cx, msg);
    }

    /// Push a message into the controller's status-bar notice; `report_error`
    /// dedupes identical messages so a render-path caller can notify safely
    /// without re-render loops when the same message keeps failing.
    fn report_notice(&mut self, cx: &mut Context<Self>, msg: String) {
        self.controller.update(cx, |ctl, cx| {
            if ctl.report_error(msg) {
                cx.notify();
            }
        });
    }
}

/// The view-identity part of a [`DataKey`] with the churn counter dropped, so
/// two keys compare equal when only library mutations (imports, edits)
/// separate them. The cached exact total is keyed on this: a mutation should
/// not by itself buy another COUNT, the settle timer guarantees one.
pub(super) fn total_identity(key: &DataKey) -> DataKey {
    let mut identity = key.clone();
    identity.generation = 0;
    identity
}

/// One listing window → the cells that paint it. Trash is already decided by
/// the query, so the filter here is a belt on top of braces: a row that came
/// back trashed while the view is live would otherwise be listed.
fn cells_for(key: &DataKey, list: &[Asset]) -> Vec<Cell> {
    // Parsed once for the window, not once per row: the same grammar the
    // ranking ran on is what says which bytes to mark, so the two cannot
    // disagree about what the user asked for.
    let marks = Lexicon::from_query(&key.search);
    list.iter()
        .filter(|a| key.in_trash || a.trashed_at.is_none())
        .map(|a| cell_from_asset(&key.library_root, &key.cache_root, a, &marks))
        .collect()
}

/// One store record → one paintable cell. Shared by the browse query pass
/// and the visual-search pass so both grids render identically.
fn cell_from_asset(library_root: &Path, cache_root: &Path, a: &Asset, marks: &Lexicon) -> Cell {
    let thumb = a
        .content_hash
        .as_deref()
        .map(|hash| trove_core::media::thumb::abs_path(cache_root, hash))
        .filter(|p| p.is_file());
    // Live font preview inputs: family (probed at import) plus the font
    // file to register (blob or linked source).
    let (font_family, font_blob) = if a.kind == AssetKind::Font {
        let blob = if a.origin == trove_core::model::Origin::Linked {
            a.facts.source_path.as_ref().map(PathBuf::from)
        } else {
            a.rel_path.as_ref().map(|rel| library_root.join(rel))
        }
        .filter(|p| p.is_file());
        (a.facts.font.family.clone(), blob)
    } else {
        (None, None)
    };
    let name = display_name(a);
    let name_marks = marks.ranges_in(ROW_TARGETS, &name);
    Cell {
        id: a.id,
        kind: a.kind,
        thumb,
        width: a.width,
        height: a.height,
        trashed: a.trashed_at.is_some(),
        name,
        name_marks,
        score: None,
        size_bytes: a.size_bytes,
        added: a.created_at.format("%Y-%m-%d %H:%M").to_string(),
        // Capture date is what a timeline is about; files without EXIF fall
        // back to the import date so nothing is lost.
        day: a
            .captured_at
            .unwrap_or(a.created_at)
            .format("%Y-%m-%d")
            .to_string(),
        font_family,
        font_blob,
    }
}

/// The recently confirmed colours, as the picker's featured row.
///
/// The picker wants `Hsla`; the history stores `#rrggbb` (what the colour
/// search consumes and what survives a config round-trip). Only the front of
/// the list is shown — the featured row is one strip, not a scroll region.
pub(crate) fn recent_picker_colors(_: &App) -> Vec<Hsla> {
    // Ten: the featured row in the colour-filter popover must fit without
    // spilling, and the default ramp below keeps to the same width.
    const FEATURED: usize = 10;

    trove_core::history::AppHistory::load()
        .colors()
        .iter()
        .rev()
        .take(FEATURED)
        .filter_map(|hex| hex_to_hsla(hex))
        .collect()
}

/// Parse `#rgb` / `#rrggbb` into the picker's colour type. Anything else,
/// including the 8-digit form, is rejected rather than guessed at.
fn hex_to_hsla(hex: &str) -> Option<Hsla> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    // `from_str_radix` accepts a leading sign, so reject non-hex first.
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (r, g, b) = match hex.len() {
        // A single digit repeats itself rather than scaling, so `#fff` is white.
        3 => {
            // Validated as ASCII hex above, so each byte decodes directly and
            // the repeated-digit expansion is exact.
            let digit = |byte: u8| (byte as char).to_digit(16).expect("validated hex");
            let mut digits = hex.bytes().map(digit);
            let (r, g, b) = (
                digits.next().expect("length checked"),
                digits.next().expect("length checked"),
                digits.next().expect("length checked"),
            );
            ((r * 0x11) as u8, (g * 0x11) as u8, (b * 0x11) as u8)
        }
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    Some(
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
        .into(),
    )
}

/// Convert a picked colour to the `#rrggbb` string the colour search takes.
///
/// The framework picker reports `Hsla`, and its own hex formatter is private,
/// so the conversion lives here. Alpha is dropped: a colour search matches
/// pixels, which have no transparency to match against.
pub(super) fn hsla_to_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    )
}
