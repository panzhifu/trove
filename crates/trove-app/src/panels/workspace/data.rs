//! Grid data model and the paged query pass.
//!
//! [`Cell`]/[`Row`] are the frozen per-layout-epoch structures the
//! virtualized list renders; `ViewKey`/`DataKey` decide when the row
//! structure and the cached query result are rebuilt.

use super::*;
use std::collections::HashMap;
use std::path::Path;
use trove_core::model::Asset;
use trove_core::services::font_manager::SystemFont;
use trove_core::store::BrowseContext;

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
    /// A virtual system-font entry (not an imported asset): shown only in
    /// the fonts view, resolved through `LibraryController::virtual_fonts`
    /// instead of the store.
    pub(super) system_font: bool,
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
    pub(super) filter_min_rating: Option<u8>,
    pub(super) filter_ext: Option<String>,
    pub(super) sort: AssetSort,
    pub(super) sort_desc: bool,
    pub(super) grid_loaded: usize,
    pub(super) library_root: PathBuf,
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
            controller,
            search_box,
            color_picker,
            pending_color_search: None,
            color_search_armed: false,
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
            viewport_backend: None,
            viewport_observer: None,
            fonts_scan_task: None,
            total_refresh: None,
            count_recheck: false,
            count_settle: None,
            filter_exts: None,
            title_cache: None,
            page_guard: Rc::new(CellFlag::new(usize::MAX)),
        };
        observe_controller(cx, &this.controller);
        this
    }

    /// Run the paged query for `key` and materialize the assets into cells.
    /// The view dispatch itself lives in `trove_core::store::BrowseContext`;
    /// this only maps the render-side [`DataKey`] onto it and surfaces query
    /// errors. Called only when the [`DataKey`] changes — never on the
    /// per-frame path. An active visual search replaces the browse query
    /// with a rank-ordered id fetch; the fonts view appends virtual cells
    /// for system fonts the library has not imported. `count_total = false`
    /// skips the exact COUNT (a lower-bound total comes back) — the caller
    /// overlays its cached exact number.
    pub(super) fn run_data_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
        count_total: bool,
    ) -> (usize, Vec<Cell>) {
        if let Some(ids) = &key.visual {
            return self.run_visual_pass(cx, key, ids);
        }

        let limit = Some(key.grid_loaded as u32);
        let ctx = BrowseContext {
            collection: key.collection,
            in_trash: key.in_trash,
            in_recent: key.in_recent,
            smart: key.smart,
            tag: key.tag,
            folder: key.folder.clone(),
            search: key.search.clone(),
            kind: key.filter_kind,
            is_favorite: key.filter_favorite,
            orientation: key.filter_orientation,
            aspect: key.filter_aspect,
            min_rating: key.filter_min_rating,
            ext: key.filter_ext.clone(),
            sort: key.sort,
            sort_desc: key.sort_desc,
        };
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();
        // Flush pending outbox rows first so a just-finished write (import,
        // edit) is reflected in the same refresh.
        let _ = ctl.library.drain_search_queue();
        let text_index = ctl.library.text_index();
        let page = match if count_total {
            ctx.run(conn, text_index, limit)
        } else {
            ctx.run_without_count(conn, text_index, limit)
        } {
            Ok(page) => page,
            Err(e) => {
                self.report_view_error(cx, e);
                trove_core::model::Page::new(0, Vec::new())
            }
        };
        let (total, list) = (page.total as usize, page.items);

        let mut cells: Vec<Cell> = list
            .iter()
            .filter(|a| key.in_trash || a.trashed_at.is_none())
            .map(|a| cell_from_asset(&key.library_root, a))
            .collect();

        // The fonts view mixes in virtual entries for system fonts whose
        // family the library has not imported. They are presentation only:
        // nothing is copied or written to the store, so no count anywhere
        // else is affected.
        let mut virtual_fonts = HashMap::new();
        let fonts_view = key.filter_kind == Some(AssetKind::Font)
            && !key.in_trash
            && !key.in_recent
            && key.search.is_empty();
        if fonts_view {
            let imported: std::collections::HashSet<String> =
                cells.iter().filter_map(|c| c.font_family.clone()).collect();
            if let Some(system) = self.controller.read(cx).system_fonts.clone() {
                let extra: Vec<Cell> = system
                    .iter()
                    .filter(|font| !imported.contains(&font.family))
                    .map(|font| {
                        let id = virtual_font_id(&font.path);
                        virtual_fonts.insert(id, font.clone());
                        virtual_font_cell(id, font)
                    })
                    .collect();
                cells.extend(extra);
            }
        }
        self.controller.update(cx, |ctl, _| {
            ctl.virtual_fonts = virtual_fonts;
        });
        // The fonts view's own count describes what the grid shows (library
        // fonts + virtual entries); every library-wide count elsewhere
        // stays a pure store number.
        let total = if fonts_view { cells.len() } else { total };
        (total, cells)
    }

    /// Materialize the visual-search hits into cells, preserving rank
    /// order. Missing records (deleted since the scan) drop out.
    fn run_visual_pass(
        &mut self,
        cx: &mut Context<Self>,
        key: &DataKey,
        ids: &[Uuid],
    ) -> (usize, Vec<Cell>) {
        let conn = self.controller.read(cx).library.store().conn();
        let by_id: HashMap<Uuid, _> = assets::by_ids(conn, ids)
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
        let cells: Vec<Cell> = ids
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|a| cell_from_asset(&key.library_root, a))
            .collect();
        self.controller.update(cx, |ctl, _| {
            ctl.virtual_fonts.clear();
        });
        (cells.len(), cells)
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

/// The view-identity part of a [`DataKey`]: the refresh counter and the
/// pagination cursor are zeroed, so two keys compare equal when only churn
/// (imports, edits) separates them. Keys the cached exact total.
pub(super) fn total_identity(key: &DataKey) -> DataKey {
    let mut identity = key.clone();
    identity.generation = 0;
    identity.grid_loaded = 0;
    identity
}

/// One store record → one paintable cell. Shared by the browse query pass
/// and the visual-search pass so both grids render identically.
fn cell_from_asset(library_root: &Path, a: &Asset) -> Cell {
    let thumb = a
        .sha256
        .as_deref()
        .map(|sha| trove_core::media::thumb::abs_path(library_root, sha))
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
        // Capture date is what a timeline is about; files without EXIF fall
        // back to the import date so nothing is lost.
        day: a
            .captured_at
            .unwrap_or(a.created_at)
            .format("%Y-%m-%d")
            .to_string(),
        font_family,
        font_blob,
        system_font: false,
    }
}

/// A virtual cell for a system font: no store record backs it — the
/// library, inspector and preview resolve `id` through
/// `LibraryController::virtual_fonts`.
fn virtual_font_cell(id: Uuid, font: &SystemFont) -> Cell {
    let style = font
        .style
        .clone()
        .unwrap_or_else(|| rust_i18n::t!("sysfonts.no_style").to_string());
    Cell {
        id,
        kind: AssetKind::Font,
        thumb: None,
        width: None,
        height: None,
        trashed: false,
        name: format!("{} · {}", font.family, style),
        size_bytes: std::fs::metadata(&font.path).map(|m| m.len()).unwrap_or(0),
        added: rust_i18n::t!("sysfonts.system_note").to_string(),
        // Timeline views never see these cells (they only appear in the
        // fonts view, which is kind-filtered and undated).
        day: String::new(),
        font_family: Some(font.family.clone()),
        font_blob: Some(font.path.clone()),
        system_font: true,
    }
}

/// Deterministic synthetic id for a system-font file. Nothing persists
/// these ids; they only have to stay stable for the session so a selected
/// virtual cell keeps resolving across data-pass rebuilds.
fn virtual_font_id(path: &Path) -> Uuid {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let high = hasher.finish();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write_u64(0x9e37_79b9_7f4a_7c15);
    path.hash(&mut hasher);
    let low = hasher.finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    // Mark as a random (v4) uuid so downstream UUID formatting stays sane.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// The recently confirmed colours, as the picker's featured row.
///
/// The picker wants `Hsla`; the history stores `#rrggbb` (what the colour
/// search consumes and what survives a config round-trip). Only the front of
/// the list is shown — the featured row is one strip, not a scroll region.
pub(super) fn recent_picker_colors(_: &App) -> Vec<Hsla> {
    const FEATURED: usize = 12;

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
