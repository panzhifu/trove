//! Application-level library state shared by the dock panels.

use uuid::Uuid;

use trove_core::library::Library;

/// Current import activity, shown by the Explorer panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ImportPhase {
    #[default]
    Idle,
    Running { total: usize, done: usize },
    Done { imported: usize, skipped: usize },
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
    /// When set, only assets carrying this tag are shown.
    pub active_tag: Option<Uuid>,
    /// When set, the workspace shows the live results of this smart collection.
    pub active_smart: Option<Uuid>,
    /// Active full-text search term (FTS). Overrides the other views when set.
    pub search_text: String,
    /// Progress of the most recent import.
    pub import_phase: ImportPhase,
    /// Asset ids currently displayed by the workspace grid (this page only).
    /// Written by the panel each render; the source of Select-all.
    pub visible_assets: Vec<Uuid>,
    /// The last clicked asset in the grid: the fixed end of a Shift range
    /// selection. Cleared whenever the selection is cleared.
    pub selection_anchor: Option<Uuid>,
    /// How many assets the grid has loaded so far (pagination cursor).
    pub grid_loaded: usize,
}

impl LibraryController {
    pub fn new(library: Library) -> Self {
        Self {
            library,
            generation: 0,
            current_collection: None,
            selected_assets: Vec::new(),
            showing_trash: false,
            active_tag: None,
            active_smart: None,
            search_text: String::new(),
            import_phase: ImportPhase::Idle,
            visible_assets: Vec::new(),
            selection_anchor: None,
            grid_loaded: GRID_PAGE_SIZE,
        }
    }

    pub fn begin_import(&mut self, total: usize) {
        self.import_phase = ImportPhase::Running { total, done: 0 };
        self.generation += 1;
    }

    /// Called by the import job on the main thread after committing one file.
    pub fn import_progress(&mut self, done: usize) {
        if let ImportPhase::Running { total, .. } = &mut self.import_phase {
            self.import_phase = ImportPhase::Running { total: *total, done };
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
        self.active_smart = None;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    pub fn select_trash(&mut self) {
        self.current_collection = None;
        self.showing_trash = true;
        self.active_smart = None;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Set the active smart collection; `None` returns to "All assets".
    /// Clears the other browse contexts.
    pub fn select_smart(&mut self, smart: Option<Uuid>) {
        self.active_smart = smart;
        self.current_collection = None;
        self.active_tag = None;
        self.showing_trash = false;
        self.selected_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Set the full-text search term. Any non-empty term takes over the
    /// browsed view; clearing it restores the previous context.
    pub fn set_search(&mut self, text: String) {
        self.search_text = text;
        if !self.search_text.trim().is_empty() {
            self.active_smart = None;
        }
        self.reset_grid_page();
        self.generation += 1;
    }

    /// The primary (last-clicked) selected asset.
    pub fn primary(&self) -> Option<Uuid> {
        self.selected_assets.last().copied()
    }

    /// Single click: select only `id` (`None` clears the selection).
    pub fn select_asset(&mut self, asset: Option<Uuid>) {
        self.selected_assets = asset.into_iter().collect();
        self.selection_anchor = asset;
        self.generation += 1;
    }

    /// Shift+click: replace the selection with the contiguous range between
    /// the anchor and `id` in the grid's display order. Without an anchor
    /// (or when either end is off-view) this degrades to a single select.
    pub fn select_range_to(&mut self, id: Uuid) {
        let anchor = self.selection_anchor;
        let Some(start) = anchor.or(Some(id)) else { return };
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
            eprintln!("trash/purge selection failed: {e}");
        }
        self.generation += 1;
        ids.len()
    }
}
