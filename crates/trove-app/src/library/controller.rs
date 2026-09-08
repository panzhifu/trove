//! Application-level library state shared by the dock panels.

use std::path::PathBuf;
use std::time::Instant;

use uuid::Uuid;

use trove_core::library::Library;
use trove_core::model::{AssetKind, AssetSort};
use trove_core::store::view_history;

/// Current import activity, shown by the Explorer panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ImportPhase {
    #[default]
    Idle,
    Running {
        total: usize,
        done: usize,
    },
    Done {
        imported: usize,
        skipped: usize,
    },
}

/// Presentation of the workspace asset area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Justified thumbnail grid.
    #[default]
    Grid,
    /// Compact full-width rows (name / kind / size / date).
    List,
}

/// Page size of the workspace asset grid: how many assets one page of the
/// paged queries loads. Scrolling near the end loads the next page.
pub const GRID_PAGE_SIZE: usize = 200;

/// Owns the open [`Library`] plus transient view state (which collection is
/// browsed, which asset is selected). The Dock panels read it each frame and
/// are re-rendered when a mutation bumps [`generation`](Self::generation).
pub struct LibraryController {
    pub library: Library,
    /// Monotonic revision; any change that should refresh the panels
    /// increments it.
    pub generation: u64,
    /// The collection being browsed in the workspace view (`None` = all).
    pub current_collection: Option<Uuid>,
    /// Selected assets; the last entry is the primary (shown in Inspector).
    pub selected_assets: Vec<Uuid>,
    /// When set, the workspace browses the trash instead of a collection.
    pub showing_trash: bool,
    /// When set, the workspace browses the recently-viewed history.
    pub showing_recent: bool,
    /// When set, only assets carrying this tag are shown.
    pub active_tag: Option<Uuid>,
    /// When set, only assets imported from this source-path prefix are shown.
    pub active_folder: Option<String>,
    /// When set, the workspace shows the live results of this smart collection.
    pub active_smart: Option<Uuid>,
    /// Active full-text search term (FTS). Overrides the other views when set.
    pub search_text: String,
    /// Grid filters (compose with any view; cleared on library swap).
    pub filter_kind: Option<AssetKind>,
    pub filter_favorite: bool,
    /// When set, only assets carrying this color label are shown.
    pub filter_color: Option<String>,
    /// Grid or list presentation of the asset area.
    pub view_mode: ViewMode,
    /// Listing sort (ignored by the live FTS search, which sorts by
    /// relevance). Cleared state = newest first.
    pub sort: AssetSort,
    pub sort_desc: bool,
    /// Progress of the most recent import.
    pub import_phase: ImportPhase,
    /// Asset ids currently displayed by the workspace grid (this page only).
    /// Written by the panel each render; the source of Select-all.
    pub visible_assets: Vec<Uuid>,
    /// The last clicked asset in the grid: the fixed end of a Shift range
    /// selection. Cleared whenever the selection is cleared.
    pub selection_anchor: Option<Uuid>,
    /// Debounce guard for view recording: the asset id and the instant its
    /// view was last written. A repeat selection of the same asset within
    /// [`VIEW_DEBOUNCE`] does not touch the database again.
    last_view_record: Option<(Uuid, Instant)>,
    /// How many assets the grid has loaded so far (pagination cursor).
    pub grid_loaded: usize,
    /// Status line surfaced by the Settings dialog (maintenance jobs,
    /// library switches). Set by whichever action ran last.
    pub notice: Option<String>,
    /// Result of the last integrity check (Settings ▸ Maintenance). `None`
    /// until the first run; cleared when the library is swapped so a report
    /// never outlives its library.
    pub integrity_report: Option<trove_core::services::maintenance::IntegrityReport>,
    /// A maintenance / library job is running; Settings buttons refuse to
    /// start a second one until it finishes.
    pub busy: bool,
}

/// Minimum delay between two view-history writes for the same asset.
/// Rapid clicks / arrow-key walks do not spam the database; every distinct
/// selection is recorded immediately.
const VIEW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);

impl LibraryController {
    pub fn new(library: Library) -> Self {
        Self {
            library,
            generation: 0,
            current_collection: None,
            selected_assets: Vec::new(),
            showing_trash: false,
            showing_recent: false,
            active_tag: None,
            active_folder: None,
            active_smart: None,
            search_text: String::new(),
            filter_kind: None,
            filter_favorite: false,
            filter_color: None,
            view_mode: ViewMode::default(),
            sort: AssetSort::default(),
            sort_desc: true,
            import_phase: ImportPhase::Idle,
            visible_assets: Vec::new(),
            selection_anchor: None,
            last_view_record: None,
            grid_loaded: GRID_PAGE_SIZE,
            notice: None,
            integrity_report: None,
            busy: false,
        }
    }

    pub fn begin_import(&mut self, total: usize) {
        self.import_phase = ImportPhase::Running { total, done: 0 };
        self.generation += 1;
    }

    /// Called by the import job on the main thread after committing one file.
    pub fn import_progress(&mut self, done: usize) {
        if let ImportPhase::Running { total, .. } = &mut self.import_phase {
            self.import_phase = ImportPhase::Running {
                total: *total,
                done,
            };
            self.generation += 1;
        }
    }

    /// Called by the import job once all files have been committed.
    pub fn finish_import(&mut self, imported: usize, skipped: usize) {
        self.import_phase = ImportPhase::Done { imported, skipped };
        self.generation += 1;
    }

    pub fn is_importing(&self) -> bool {
        matches!(self.import_phase, ImportPhase::Running { .. })
    }

    /// Reset the grid pagination cursor; every view switch starts a fresh
    /// first page.
    fn reset_grid_page(&mut self) {
        self.grid_loaded = GRID_PAGE_SIZE;
    }

    pub fn select_collection(&mut self, collection: Option<Uuid>) {
        self.current_collection = collection;
        self.showing_trash = false;
        self.showing_recent = false;
        self.active_smart = None;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    pub fn select_trash(&mut self) {
        self.current_collection = None;
        self.showing_trash = true;
        self.showing_recent = false;
        self.active_smart = None;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Browse the recently-viewed history view.
    pub fn select_recent(&mut self) {
        self.current_collection = None;
        self.showing_trash = false;
        self.showing_recent = true;
        self.active_smart = None;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Record `asset` as viewed in the history table. Debounced per asset:
    /// a repeated selection of the same asset within [`VIEW_DEBOUNCE`] is
    /// ignored, anything else is written immediately. Failures are silent —
    /// history is a convenience, not a workflow step.
    fn record_view(&mut self, asset: Uuid) {
        if let Some((id, at)) = self.last_view_record
            && id == asset
            && at.elapsed() < VIEW_DEBOUNCE
        {
            return;
        }
        let conn = self.library.store().conn();
        if view_history::record(conn, asset).is_ok() {
            self.last_view_record = Some((asset, Instant::now()));
        }
    }

    /// Set the active smart collection; `None` returns to "All assets".
    /// Clears the other browse contexts.
    pub fn select_smart(&mut self, smart: Option<Uuid>) {
        self.active_smart = smart;
        self.current_collection = None;
        self.active_tag = None;
        self.showing_trash = false;
        self.showing_recent = false;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Set the full-text search term. Any non-empty term takes over the
    /// browsed view; clearing it restores the previous context.
    /// Grid filters; each change resets the pagination cursor.
    pub fn set_filter_kind(&mut self, kind: Option<AssetKind>) {
        if self.filter_kind != kind {
            self.filter_kind = kind;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    pub fn set_filter_favorite(&mut self, favorite: bool) {
        if self.filter_favorite != favorite {
            self.filter_favorite = favorite;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    /// Set the active color-label filter; `None` clears it.
    pub fn set_filter_color(&mut self, color: Option<String>) {
        if self.filter_color != color {
            self.filter_color = color;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    pub fn clear_filters(&mut self) {
        let changed =
            self.filter_kind.is_some() || self.filter_favorite || self.filter_color.is_some();
        self.filter_kind = None;
        self.filter_favorite = false;
        self.filter_color = None;
        if changed {
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    /// Switch the asset-area presentation. Re-layouts the frozen rows.
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        if self.view_mode != mode {
            self.view_mode = mode;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    /// Change the listing sort. Re-layouts the frozen rows.
    pub fn set_sort(&mut self, sort: AssetSort, desc: bool) {
        if self.sort != sort || self.sort_desc != desc {
            self.sort = sort;
            self.sort_desc = desc;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    /// Remove assets from the currently browsed collection (no-op outside a
    /// collection view — trash / smart / tag views have no membership).
    pub fn remove_from_current_collection(&mut self, ids: &[Uuid]) -> usize {
        let Some(cid) = self.current_collection else {
            return 0;
        };
        match self.library.remove_assets_from_collection(cid, ids) {
            Ok(removed) => {
                if removed > 0 {
                    self.generation += 1;
                }
                removed
            }
            Err(e) => {
                self.notice = Some(
                    rust_i18n::t!("workspace.remove_failed", error = e.to_string()).to_string(),
                );
                0
            }
        }
    }

    /// Swap the open library for another one at `path` (hot switch from
    /// Settings). Resets every view state; refused mid-import so a running
    /// job cannot keep writing into the previous store.
    pub fn swap_library(&mut self, path: PathBuf) -> Result<(), trove_core::Error> {
        if self.is_importing() {
            return Err(trove_core::Error::Validation("import in progress".into()));
        }
        let library = Library::open(path)?;
        self.library = library;
        self.current_collection = None;
        self.showing_trash = false;
        self.showing_recent = false;
        self.active_smart = None;
        self.active_tag = None;
        self.selected_assets.clear();
        self.selection_anchor = None;
        self.search_text.clear();
        self.filter_kind = None;
        self.filter_favorite = false;
        self.filter_color = None;
        self.import_phase = ImportPhase::Idle;
        self.integrity_report = None;
        self.visible_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
        Ok(())
    }

    pub fn set_search(&mut self, text: String) {
        self.search_text = text;
        if !self.search_text.trim().is_empty() {
            self.active_smart = None;
        }
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Record an error notice. Returns `true` only when the message is new,
    /// so render-path callers can notify safely without re-render loops.
    pub fn report_error(&mut self, msg: String) -> bool {
        if self.notice.as_deref() != Some(msg.as_str()) {
            self.notice = Some(msg);
            true
        } else {
            false
        }
    }

    /// The primary (last-clicked) selected asset.
    pub fn primary(&self) -> Option<Uuid> {
        self.selected_assets.last().copied()
    }

    /// Single click: select only `id` (`None` clears the selection).
    pub fn select_asset(&mut self, asset: Option<Uuid>) {
        self.selected_assets = asset.into_iter().collect();
        self.selection_anchor = asset;
        if let Some(id) = asset {
            self.record_view(id);
        }
        self.generation += 1;
    }

    /// Shift+click: replace the selection with the contiguous range between
    /// the anchor and `id` in the grid's display order. Without an anchor
    /// (or when either end is off-view) this degrades to a single select.
    pub fn select_range_to(&mut self, id: Uuid) {
        let anchor = self.selection_anchor;
        let Some(start) = anchor.or(Some(id)) else {
            return;
        };
        let flat = &self.visible_assets;
        let (a, b) = (
            flat.iter().position(|&x| x == start),
            flat.iter().position(|&x| x == id),
        );
        self.selected_assets = match (a, b) {
            (Some(a), Some(b)) if a <= b => flat[a..=b].to_vec(),
            (Some(a), Some(b)) => flat[b..=a].to_vec(),
            _ => vec![id],
        };
        self.selection_anchor = Some(id);
        self.record_view(id);
        self.generation += 1;
    }

    /// Ctrl/Cmd click: toggle `id` in the selection.
    pub fn toggle_asset(&mut self, id: Uuid) {
        if let Some(pos) = self.selected_assets.iter().position(|&x| x == id) {
            self.selected_assets.remove(pos);
        } else {
            self.selected_assets.push(id);
        }
        self.selection_anchor = Some(id);
        if let Some(primary) = self.selected_assets.last().copied() {
            self.record_view(primary);
        }
        self.generation += 1;
    }

    /// Ids an action applies to when triggered from a clicked asset: the whole
    /// selection if it contains the asset, otherwise just the asset.
    pub fn action_targets(&self, clicked: Uuid) -> Vec<Uuid> {
        if self.selected_assets.contains(&clicked) {
            self.selected_assets.clone()
        } else {
            vec![clicked]
        }
    }

    /// Drop assets from the selection (e.g. after moving them to the trash).
    pub fn deselect(&mut self, ids: &[Uuid]) {
        self.selected_assets.retain(|id| !ids.contains(id));
        self.generation += 1;
    }

    pub fn select_tag(&mut self, tag: Option<Uuid>) {
        self.active_tag = tag;
        self.active_smart = None;
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Set the active source-folder filter; `None` clears it. Composes with
    /// every browse context exactly like the tag filter.
    pub fn select_folder(&mut self, folder: Option<String>) {
        if self.active_folder != folder {
            self.active_folder = folder;
            self.reset_grid_page();
            self.generation += 1;
        }
    }

    /// Select every asset currently displayed by the grid.
    pub fn select_all_visible(&mut self) {
        self.selected_assets = self.visible_assets.clone();
        self.generation += 1;
    }

    /// Clear the selection entirely.
    pub fn clear_selection(&mut self) {
        if !self.selected_assets.is_empty() {
            self.selected_assets.clear();
            self.selection_anchor = None;
            self.generation += 1;
        }
    }

    /// Move the current selection to the trash (or purge it when browsing the
    /// trash). Returns the number of affected ids.
    pub fn trash_or_purge_selection(&mut self) -> usize {
        let ids = std::mem::take(&mut self.selected_assets);
        if ids.is_empty() {
            return 0;
        }
        let result = if self.showing_trash {
            self.library.purge_assets(&ids).map(|r| r.purged)
        } else {
            self.library.trash_assets(&ids)
        };
        if let Err(e) = result {
            self.notice =
                Some(rust_i18n::t!("workspace.trash_failed", error = e.to_string()).to_string());
        }
        self.generation += 1;
        ids.len()
    }
}
