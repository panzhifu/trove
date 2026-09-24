//! Application-level library state shared by the dock panels.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use uuid::Uuid;

use trove_core::config::AppConfig;
use trove_core::library::Library;
use trove_core::media::height_color::StoredLook;
use trove_core::model::{AspectPreset, AssetKind, AssetSort, Orientation, ResolutionBand};
use trove_core::store::browse::SearchTiers;
use trove_core::store::model_look;
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

/// One row of the status-bar task panel: a live or recently-settled job.
///
/// Mirrors the backend [`TaskStatus`] plus the last progress numbers the
/// watchers saw. Kept on the controller (not read live from
/// [`trove_core::tasks::TaskManager::snapshot`]) so a *finished* job lingers
/// long enough to be retried — the core registry purges a job as soon as the
/// next one starts.
#[derive(Debug, Clone)]
pub struct TaskCard {
    pub id: trove_core::tasks::TaskId,
    pub kind: trove_core::tasks::TaskKind,
    pub label: String,
    pub status: trove_core::tasks::TaskStatus,
    pub done: u64,
    pub total: u64,
}

impl TaskCard {
    /// A settled row: nothing runs, but a failed/cancelled one can be retried.
    pub fn finished(&self) -> bool {
        matches!(
            self.status,
            trove_core::tasks::TaskStatus::Completed
                | trove_core::tasks::TaskStatus::Failed
                | trove_core::tasks::TaskStatus::Cancelled
        )
    }
}

/// What it takes to re-run a job the user can retry. The provider is rebuilt
/// from the saved config on retry, so only the run's *inputs* are stored here.
#[derive(Debug, Clone)]
pub enum Retryable {
    Import {
        kind: trove_core::tasks::TaskKind,
        options: trove_core::tasks::import::ImportOptions,
        total: usize,
    },
    Embedding,
    Analysis {
        request: trove_core::tasks::ai_analysis::AiAnalysisRunRequest,
        undo: bool,
    },
}

/// The kind of a [`Retryable`], used as its map key.
pub fn retryable_kind(retryable: &Retryable) -> trove_core::tasks::TaskKind {
    use trove_core::tasks::TaskKind;
    match retryable {
        Retryable::Import { kind, .. } => *kind,
        Retryable::Embedding => TaskKind::EmbeddingBackfill,
        Retryable::Analysis { .. } => TaskKind::AiAnalysis,
    }
}

/// Presentation of the workspace asset area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Justified thumbnail grid.
    #[default]
    Grid,
    /// Compact full-width rows (name / kind / size / date).
    List,
    /// Justified grid split into day sections, newest day first.
    Timeline,
}

/// Outcome of the last AI-endpoint connection test (Settings ▸ AI).
///
/// Lives on the controller rather than inside the settings view because the
/// test is an HTTP round trip: it finishes on a background thread and has to
/// survive the page being rebuilt (pages are rebuilt every render).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AiProbe {
    /// Never run, or cleared by a library swap.
    #[default]
    Idle,
    /// A test is in flight — the button is disabled meanwhile.
    Running,
    /// The endpoint answered; `dim` is the vector width it reported.
    Ok { dim: usize },
    /// Unreachable, refused, or misconfigured. The message is the reason the
    /// user has to read: this surfaces without a log window.
    Failed { message: String },
}

impl AiProbe {
    /// Whether a test is in flight (the settings button guards on this
    /// instead of [`LibraryController::busy`], which the maintenance jobs
    /// own).
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Outcome of the last analysis-endpoint connection test (Settings ▸ AI).
///
/// A sibling of [`AiProbe`] rather than a reuse of it. An embedding endpoint
/// answers with a vector and a width; an analysis endpoint answers with
/// words, and the two are configured separately — a machine can easily have
/// one reachable and the other not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AnalysisProbe {
    /// Never run, or cleared by a library swap.
    #[default]
    Idle,
    /// A test is in flight — the button is disabled meanwhile.
    Running,
    /// The model answered. `reply` is what it said, which is the only proof
    /// the request reached a *model* rather than some web server that
    /// happens to accept POSTs.
    Ok { reply: String },
    /// Unreachable, refused, or misconfigured. The message is the reason the
    /// user has to read: this surfaces without a log window.
    Failed { message: String },
}

impl AnalysisProbe {
    /// Whether a test is in flight (the settings button guards on this).
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Page size of the workspace asset grid: how many assets one page of the
/// paged queries loads. Scrolling near the end loads the next page.
pub const GRID_PAGE_SIZE: usize = 200;

/// Where the colour filter's similarity rail sits for someone who has never
/// touched it: the middle of the box, which is what "find me the red ones"
/// means before the user says otherwise.
pub const DEFAULT_COLOUR_SIMILARITY: f32 = 50.0;

/// Ranked result set of a visual search (similar-image / by-colour): a
/// display label plus `(asset id, similarity score)` pairs in rank order.
/// The workspace grid shows exactly these assets while it is active.
#[derive(Debug, Clone)]
pub struct VisualSearchResults {
    pub label: String,
    pub hits: Vec<(Uuid, f32)>,
    /// Rank-ordered ids derived from `hits`, cached because the workspace
    /// render copies them into its per-frame object snapshot (and rebuilds
    /// the query key with them) on every frame. Deriving them per frame is
    /// O(hits) — capped at [`CANDIDATE_CAP`](crate::search::CANDIDATE_CAP)
    /// — for a list that only changes when a new search lands.
    pub ids: Rc<Vec<Uuid>>,
    /// The colour this result set was a question about, when it was one.
    ///
    /// The similarity slider re-runs *the same ask* at a different box width,
    /// and the label it would otherwise have to be read back out of is
    /// translated (`workspace.color_search` × nine locales), so the answer is
    /// stored rather than parsed.
    pub colour: Option<String>,
}

impl VisualSearchResults {
    /// Build the result set, deriving the rank-ordered id list once.
    pub fn new(label: String, hits: Vec<(Uuid, f32)>, colour: Option<String>) -> Self {
        let ids = Rc::new(hits.iter().map(|(id, _)| *id).collect());
        Self {
            label,
            hits,
            ids,
            colour,
        }
    }
}

/// How the selection was last changed. The root view auto-shows the
/// inspector only for plain selections: a multi-select gesture (Ctrl
/// toggle / Shift range) keeps the right dock on whatever tab it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionSource {
    /// Not a selection change (cleared selection, initial state).
    #[default]
    None,
    /// Plain click / keyboard move: exactly one asset becomes the selection.
    Plain,
    /// Ctrl toggle or Shift range: the selection may hold many assets.
    Multi,
}

/// Owns the open [`Library`] plus transient view state (which collection is
/// browsed, which asset is selected). The Dock panels read it each frame and
/// are re-rendered when a mutation bumps [`generation`](Self::generation).
/// How long a library swap waits for a cancelled import to actually stop.
/// The staging pool checks the flag per file and subprocesses are killed on
/// timeout, so a healthy job settles in well under a second; the cap is a
/// guard against a job that cannot stop, not the expected wait.
const IMPORT_CANCEL_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct LibraryController {
    pub library: Library,
    /// The running import job, if any: what the cancel button presses and
    /// what a library swap must stop before it can swap the store.
    pub(crate) import_task: Option<crate::library::jobs::ImportTaskHandle>,
    /// Every live and recently-settled job the status-bar panel lists. The
    /// watchers add a row when a job starts and update it from the job's
    /// events; finished rows linger (bounded) so the user can retry them.
    pub tasks: Vec<TaskCard>,
    /// How to re-run each kind the last time it ran, so the panel's Retry
    /// button can restart a failed/cancelled job. Keyed by kind.
    pub retryable: std::collections::HashMap<trove_core::tasks::TaskKind, Retryable>,
    /// Whether the status-bar task panel is expanded.
    pub task_panel_open: bool,
    /// Monotonic revision of the library *contents* (imports, edits, trash,
    /// renames, collection/tag mutations, browse switches). Keys every data
    /// cache: the explorer snapshot, the folder/tag/extension scans, the
    /// title cache and the workspace data pass.
    pub generation: u64,
    /// Monotonic revision of the browse *filters* (kind / favorite / shape /
    /// aspect / rating / extension). Filters change no library content, so
    /// caches that do not read them (the sidebar counts, the folder and tag
    /// scans) must stay valid across a filter change; only caches that
    /// actually read a filter key on this counter or on the filter value
    /// itself (the title's favorites label, the explorer's kind highlight).
    pub filter_generation: u64,
    /// The collection being browsed in the workspace view (`None` = all).
    pub current_collection: Option<Uuid>,
    /// Selected assets; the last entry is the primary (shown in Inspector).
    /// Behind an `Rc` because the workspace render copies the selection into
    /// its per-frame snapshot: with a plain `Vec` that is an O(selection)
    /// allocation on every frame (notably a full-page Select-all), with an
    /// `Rc` it is a refcount bump. Mutate through
    /// [`Rc::make_mut`](std::rc::Rc::make_mut) so a snapshot held by the
    /// current frame cannot be corrupted.
    pub selected_assets: Rc<Vec<Uuid>>,
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
    /// Active full-text search term. Overrides the other views when set.
    pub search_text: String,
    /// Grid filters (compose with any view; cleared on library swap).
    pub filter_kind: Option<AssetKind>,
    pub filter_favorite: bool,
    pub filter_orientation: Option<Orientation>,
    /// Media aspect-ratio preset (WeChat cover, 4:3 photo, …). The UI keeps
    /// this mutually exclusive with [`Self::filter_orientation`] — picking
    /// one clears the other — but both compose at the query layer.
    pub filter_aspect: Option<AspectPreset>,
    /// Resolution band by longer edge. Independent of the two shape filters:
    /// they compare proportions, this compares size, and a 4K frame is 4K at
    /// any ratio.
    pub filter_resolution: Option<ResolutionBand>,
    /// How tight a colour search's match box is, on the interface's 0–100 rail
    /// (0 loosest, 100 tightest). It is a *question width*, not a score
    /// threshold: see `trove_core::media::search::ColourMatch`.
    pub colour_similarity: f32,
    pub filter_min_rating: Option<u8>,
    pub filter_ext: Option<String>,
    /// Grid or list presentation of the asset area.
    pub view_mode: ViewMode,
    /// Listing sort (ignored by the live search, which sorts by
    /// relevance). Cleared state = newest first.
    pub sort: AssetSort,
    pub sort_desc: bool,
    /// Progress of the most recent import.
    pub import_phase: ImportPhase,
    /// Asset ids currently displayed by the workspace grid. Published lazily
    /// by the panel ([`set_visible_assets`](Self::set_visible_assets)) because
    /// flattening the frozen rows on every frame only to serve Select-all and
    /// Shift-range is pure waste: both are user gestures, so the ids can be
    /// collected when the gesture happens instead. Read through
    /// [`visible_assets`](Self::visible_assets).
    pub(crate) visible_assets: Vec<Uuid>,
    /// The last clicked asset in the grid: the fixed end of a Shift range
    /// selection. Cleared whenever the selection is cleared.
    pub selection_anchor: Option<Uuid>,
    /// Debounce guard for view recording: the asset id and the instant its
    /// view was last written. A repeat selection of the same asset within
    /// [`VIEW_DEBOUNCE`] does not touch the database again.
    last_view_record: Option<(Uuid, Instant)>,
    /// How many assets the grid has loaded so far (pagination cursor).
    pub grid_loaded: usize,
    /// Grid zoom from the title-bar slider: multiplier on the ideal grid
    /// row height (1.0 = default). Part of the workspace layout key.
    pub row_height_scale: f32,
    /// How the selection was last changed (see [`SelectionSource`]).
    pub selection_source: SelectionSource,
    /// Active visual-search results shown by the grid in place of the
    /// normal browse query. `None` = not searching.
    pub visual_results: Option<VisualSearchResults>,
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
    /// Result of the last AI-endpoint connection test (Settings ▸ AI). Not a
    /// task-manager job: it is one call against a server the user typed in,
    /// so it reports inline on the page and never blocks a job slot.
    pub ai_probe: AiProbe,
    /// Result of the last analysis-endpoint connection test (Settings ▸ AI).
    /// Same reasoning as [`Self::ai_probe`]: one call against a server the
    /// user typed in, answered inline on the page, no job slot taken.
    pub analysis_probe: AnalysisProbe,
    /// The embedding of the committed search term, when one has been fetched
    /// (`jobs::request_query_embedding_app` runs after Enter). The workspace
    /// hands it to the query, which fuses it into the text ranking for as
    /// long as its `text` still matches the search box; `None` = the search
    /// is text-only, which is also what an unconfigured endpoint gives.
    pub query_vector: Option<trove_core::search::vector::QueryVector>,
    /// The search legs the current query may use, resolved from the stored
    /// config. A settings toggle re-resolves it through
    /// [`Self::refresh_search_tiers`].
    pub search_tiers: SearchTiers,
    /// The AI plan for the current search term, when the AI tier is on and the
    /// planner has answered (`jobs::request_ai_plan_app`). Cleared whenever
    /// the term changes: a plan carries no term of its own, so a stale one
    /// cannot be recognised the way a stale [`Self::query_vector`] can.
    pub ai_plan: Option<trove_core::ai::search_planner::AiSearchPlan>,
    /// Cached duplicate clusters for the duplicates dialog: computed once on
    /// a backend thread (the O(n²) pHash pass must not run per render frame),
    /// invalidated on cleanup and library swap.
    pub duplicates: Option<std::sync::Arc<Vec<trove_core::store::assets::DuplicateGroup>>>,
    /// Whether the duplicate scan above is currently running.
    pub duplicates_computing: bool,
    /// Resident folder-watch task, bound to the current library's task
    /// manager. A library swap cancels it; the caller restarts it on the new
    /// library (see `jobs::start_watch_service`).
    pub watch_task: Option<crate::library::jobs::WatchTask>,
    /// Window handle the watch pump needs to start imports with toasts.
    pub(crate) watch_handle: Option<gpui_kit::AnyWindowHandle>,
    /// Coalescing state for import progress: the done count and instant of
    /// the last throttled generation bump (see the import consts below).
    last_import_refresh: Option<(usize, std::time::Instant)>,
}

/// Minimum delay between two view-history writes for the same asset.
/// Rapid clicks / arrow-key walks do not spam the database; every distinct
/// selection is recorded immediately.
const VIEW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Import progress coalescing: at most one full grid refresh per interval,
/// or every [`IMPORT_REFRESH_FILES`] files for fast bursts. Every generation
/// bump re-runs the whole workspace data pass, which must not follow the
/// import pump's 80 ms cadence; `finish_import` always refreshes last.
const IMPORT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const IMPORT_REFRESH_FILES: usize = 10;

/// Resolve the three search legs from the stored config.
///
/// A tier is on only when its toggle is set *and* its endpoint is configured —
/// an enabled-but-unconfigured leg is inert, which is what keeps a search from
/// failing on a half-filled settings page.
fn resolved_search_tiers(config: &AppConfig) -> SearchTiers {
    SearchTiers {
        full_text: config.search.full_text,
        semantic: config.semantic_endpoint().is_some(),
        ai: config.search.ai_enabled(),
    }
}

impl LibraryController {
    pub fn new(library: Library) -> Self {
        let config = AppConfig::load();
        Self {
            library,
            generation: 0,
            filter_generation: 0,
            current_collection: None,
            selected_assets: Rc::new(Vec::new()),
            showing_trash: false,
            showing_recent: false,
            active_tag: None,
            active_folder: None,
            active_smart: None,
            search_text: String::new(),
            filter_kind: None,
            filter_favorite: false,
            filter_orientation: None,
            filter_aspect: None,
            filter_resolution: None,
            colour_similarity: DEFAULT_COLOUR_SIMILARITY,
            filter_min_rating: None,
            filter_ext: None,
            view_mode: ViewMode::default(),
            sort: AssetSort::default(),
            sort_desc: true,
            import_phase: ImportPhase::Idle,
            visible_assets: Vec::new(),
            selection_anchor: None,
            last_view_record: None,
            grid_loaded: GRID_PAGE_SIZE,
            row_height_scale: config.grid_zoom(),
            selection_source: SelectionSource::None,
            visual_results: None,
            notice: None,
            integrity_report: None,
            busy: false,
            ai_probe: AiProbe::Idle,
            analysis_probe: AnalysisProbe::Idle,
            query_vector: None,
            search_tiers: resolved_search_tiers(&config),
            ai_plan: None,
            duplicates: None,
            duplicates_computing: false,
            watch_task: None,
            watch_handle: None,
            import_task: None,
            last_import_refresh: None,
            tasks: Vec::new(),
            retryable: std::collections::HashMap::new(),
            task_panel_open: false,
        }
    }

    pub fn begin_import(&mut self, total: usize) {
        self.import_phase = ImportPhase::Running { total, done: 0 };
        self.last_import_refresh = None;
        self.generation += 1;
    }

    /// Called by the import pump on its poll cadence. Both `done` and `total`
    /// come straight from the progress event: the real total only becomes
    /// known mid-run (after the backend walk expands folders), so carrying it
    /// here is what lets the bar switch from "Scanning…" to a real fraction.
    /// The done count always lands (progress display), but the generation
    /// bump — and with it the workspace's full re-query — is coalesced to
    /// [`IMPORT_REFRESH_INTERVAL`] / [`IMPORT_REFRESH_FILES`].
    pub fn import_progress(&mut self, done: usize, total: usize) {
        if let ImportPhase::Running {
            total: cur_total, ..
        } = &mut self.import_phase
        {
            self.import_phase = ImportPhase::Running {
                total: if total > 0 { total } else { *cur_total },
                done,
            };
            let due = match self.last_import_refresh {
                Some((files, at)) => {
                    done.saturating_sub(files) >= IMPORT_REFRESH_FILES
                        || at.elapsed() >= IMPORT_REFRESH_INTERVAL
                }
                None => true,
            };
            if due {
                self.last_import_refresh = Some((done, std::time::Instant::now()));
                self.generation += 1;
            }
        }
    }

    /// Called by the import job once all files have been committed. Always
    /// bumps the generation: the final full refresh must not be throttled.
    pub fn finish_import(&mut self, imported: usize, skipped: usize) {
        self.import_phase = ImportPhase::Done { imported, skipped };
        self.import_task = None;
        self.last_import_refresh = None;
        self.generation += 1;
    }

    /// Ask the running import to stop at its next checkpoint. The job
    /// settles asynchronously — the progress toast turns into the outcome
    /// toast on its own.
    pub fn cancel_import(&mut self) {
        if let Some(task) = &self.import_task {
            task.manager.cancel(task.task_id);
        }
    }

    pub fn is_importing(&self) -> bool {
        matches!(self.import_phase, ImportPhase::Running { .. })
    }

    // -- status-bar task center --------------------------------------------

    /// Add (or reset) a live row for a job that just started, dropping the
    /// previous settled row of the same kind so one kind never shows twice.
    pub fn begin_task(
        &mut self,
        id: trove_core::tasks::TaskId,
        kind: trove_core::tasks::TaskKind,
        label: impl Into<String>,
    ) {
        let label = label.into();
        self.tasks
            .retain(|c| c.id == id || c.kind != kind || !c.finished());
        match self.tasks.iter_mut().find(|c| c.id == id) {
            Some(card) => {
                card.kind = kind;
                card.label = label;
                card.status = trove_core::tasks::TaskStatus::Running;
                card.done = 0;
                card.total = 0;
            }
            None => self.tasks.push(TaskCard {
                id,
                kind,
                label,
                status: trove_core::tasks::TaskStatus::Running,
                done: 0,
                total: 0,
            }),
        }
        self.prune_tasks();
    }

    /// Update a row's progress numbers. A `total` of 0 is "not known yet" and
    /// leaves the stored total untouched (the folder-walk case).
    pub fn update_task(&mut self, id: trove_core::tasks::TaskId, done: u64, total: u64) {
        if let Some(card) = self.tasks.iter_mut().find(|c| c.id == id) {
            card.done = done;
            if total > 0 {
                card.total = total;
            }
        }
    }

    pub fn set_task_status(
        &mut self,
        id: trove_core::tasks::TaskId,
        status: trove_core::tasks::TaskStatus,
    ) {
        if let Some(card) = self.tasks.iter_mut().find(|c| c.id == id) {
            card.status = status;
        }
    }

    /// Remember how to re-run a job. The panel offers Retry for a settled row
    /// whose kind has an entry here.
    pub fn record_retry(&mut self, retryable: Retryable) {
        let kind = retryable_kind(&retryable);
        self.retryable.insert(kind, retryable);
    }

    /// The stored re-run inputs for `kind`, if any.
    pub fn retry_inputs(&self, kind: trove_core::tasks::TaskKind) -> Option<&Retryable> {
        self.retryable.get(&kind)
    }

    pub fn pause_task(&mut self, id: trove_core::tasks::TaskId) {
        let manager = self.library.tasks().clone();
        manager.pause(id);
        self.set_task_status(id, trove_core::tasks::TaskStatus::Paused);
    }

    pub fn resume_task(&mut self, id: trove_core::tasks::TaskId) {
        let manager = self.library.tasks().clone();
        manager.resume(id);
        self.set_task_status(id, trove_core::tasks::TaskStatus::Running);
    }

    pub fn cancel_task(&mut self, id: trove_core::tasks::TaskId) {
        let manager = self.library.tasks().clone();
        manager.cancel(id);
    }

    /// Drop the oldest finished rows beyond a small cap so the panel and the
    /// retry map stay bounded across a long session.
    fn prune_tasks(&mut self) {
        const MAX_FINISHED: usize = 8;
        let finished: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, card)| card.finished())
            .map(|(index, _)| index)
            .collect();
        if finished.len() <= MAX_FINISHED {
            return;
        }
        let drop: Vec<trove_core::tasks::TaskId> = finished[..finished.len() - MAX_FINISHED]
            .iter()
            .map(|&index| self.tasks[index].id)
            .collect();
        self.tasks.retain(|card| !drop.contains(&card.id));
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
        self.filter_kind = None;
        self.selected_assets = Rc::new(Vec::new());
        self.close_visual_search();
        self.reset_grid_page();
        self.generation += 1;
    }

    pub fn select_trash(&mut self) {
        self.current_collection = None;
        self.showing_trash = true;
        self.showing_recent = false;
        self.active_smart = None;
        self.selected_assets = Rc::new(Vec::new());
        self.close_visual_search();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Browse the recently-viewed history view.
    pub fn select_recent(&mut self) {
        self.current_collection = None;
        self.showing_trash = false;
        self.showing_recent = true;
        self.active_smart = None;
        self.selected_assets = Rc::new(Vec::new());
        self.close_visual_search();
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
            // Only the recently-viewed grid reorders with history, so only
            // that view invalidates its cached data pass here. Bumping the
            // generation for every selection would re-run the whole grid
            // query (plus a file stat per asset) on every click.
            if self.showing_recent {
                self.generation += 1;
            }
        }
    }

    /// The 3D look this asset was last left in, or `None` while it still uses
    /// the app-wide default.
    pub fn model_look(&self, asset: Uuid) -> Option<StoredLook> {
        let conn = self.library.store().conn();
        model_look::get(conn, asset).ok().flatten()
    }

    /// Remember `look` as this asset's own, so reopening the model puts the
    /// colours back the way they were left.
    ///
    /// Silent on failure, like [`Self::record_view`]: a library whose row cannot
    /// be written should still let the user paint the model, and the panel has
    /// already applied the look on screen. Nothing here bumps a generation —
    /// the look is not part of any listing.
    pub fn remember_model_look(&mut self, asset: Uuid, look: StoredLook) {
        let conn = self.library.store().conn();
        let _ = model_look::set(conn, asset, &look);
    }

    /// Set the active smart collection; `None` returns to "All assets".
    /// Clears the other browse contexts.
    pub fn select_smart(&mut self, smart: Option<Uuid>) {
        self.active_smart = smart;
        self.current_collection = None;
        self.active_tag = None;
        self.showing_trash = false;
        self.showing_recent = false;
        self.selected_assets = Rc::new(Vec::new());
        self.close_visual_search();
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Grid filters; each change resets the pagination cursor. A filter
    /// change bumps [`Self::filter_generation`], not the data generation:
    /// the sidebar counts and the folder/tag/extension scans do not read
    /// the filters and must stay valid.
    pub fn set_filter_kind(&mut self, kind: Option<AssetKind>) {
        if self.filter_kind != kind {
            self.filter_kind = kind;
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn set_filter_favorite(&mut self, favorite: bool) {
        if self.filter_favorite != favorite {
            self.filter_favorite = favorite;
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn set_filter_orientation(&mut self, orientation: Option<Orientation>) {
        if self.filter_orientation != orientation {
            self.filter_orientation = orientation;
            // The shape menu presents the two shape filters as one choice;
            // a concrete shape clears the preset (and vice versa below).
            if orientation.is_some() {
                self.filter_aspect = None;
            }
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn set_filter_aspect(&mut self, aspect: Option<AspectPreset>) {
        if self.filter_aspect != aspect {
            self.filter_aspect = aspect;
            if aspect.is_some() {
                self.filter_orientation = None;
            }
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn set_filter_resolution(&mut self, band: Option<ResolutionBand>) {
        if self.filter_resolution != band {
            self.filter_resolution = band;
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    /// Set the colour search's match-box width (0–100). Deliberately does *not*
    /// bump `generation`: it changes how a colour question is asked, not what
    /// any browsed view lists. The colour search itself is re-run by the panel
    /// that owns the slider.
    pub fn set_colour_similarity(&mut self, similarity: f32) {
        let similarity = similarity.clamp(0.0, 100.0);
        if self.colour_similarity != similarity {
            self.colour_similarity = similarity;
        }
    }

    pub fn set_filter_min_rating(&mut self, rating: Option<u8>) {
        if self.filter_min_rating != rating {
            self.filter_min_rating = rating;
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn set_filter_ext(&mut self, ext: Option<String>) {
        if self.filter_ext != ext {
            self.filter_ext = ext;
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    pub fn clear_filters(&mut self) {
        let changed = self.filter_kind.is_some()
            || self.filter_favorite
            || self.filter_orientation.is_some()
            || self.filter_aspect.is_some()
            || self.filter_resolution.is_some()
            || self.filter_min_rating.is_some()
            || self.filter_ext.is_some();
        self.filter_kind = None;
        self.filter_favorite = false;
        self.filter_orientation = None;
        self.filter_aspect = None;
        self.filter_resolution = None;
        self.filter_min_rating = None;
        self.filter_ext = None;
        if changed {
            self.reset_grid_page();
            self.filter_generation += 1;
        }
    }

    /// Switch the asset-area presentation (grid / list / timeline).
    ///
    /// Pure presentation: it changes no query result, so it must not bump
    /// [`Self::generation`] — that counter keys the data caches (the
    /// explorer snapshot, the folder/tag/extension scans, the workspace
    /// data pass), and a bump would re-run every COUNT and re-query the
    /// page for nothing. The reset pagination cursor already changes the
    /// data key when a deeper page was loaded, and the grid relayout is
    /// keyed on the `ViewMode` through the workspace's `ViewKey`.
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        if self.view_mode != mode {
            self.view_mode = mode;
            self.reset_grid_page();
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

    /// Swap the open library for another one (hot switch from the asset
    /// manager's library switch): the new library's data and cache roots.
    /// Resets every view state; refused mid-import so a running job cannot
    /// keep writing into the previous store.
    pub fn swap_library(
        &mut self,
        data_root: PathBuf,
        cache_root: PathBuf,
    ) -> Result<(), trove_core::Error> {
        // A running import no longer refuses the swap: it is cancelled and
        // the swap waits for the job thread to stop touching the store. The
        // staging pool's per-file checkpoints bound the wait to the in-flight
        // decodes; the cap only guards against a job that cannot stop at all,
        // and timing out simply refuses this swap — nothing is half-done.
        if let Some(task) = self.import_task.take() {
            task.manager.cancel(task.task_id);
            let deadline = std::time::Instant::now() + IMPORT_CANCEL_WAIT;
            // Wait on active, not merely running: a paused import still holds
            // the store and, once cancelled, wakes to unwind.
            while task.manager.is_task_active(task.task_id) {
                if std::time::Instant::now() >= deadline {
                    return Err(trove_core::Error::Validation(
                        "import did not stop in time".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let library = Library::open(data_root, cache_root)?;
        self.library = library;
        self.current_collection = None;
        self.showing_trash = false;
        self.showing_recent = false;
        self.active_smart = None;
        self.active_tag = None;
        self.selected_assets = Rc::new(Vec::new());
        self.selection_anchor = None;
        self.search_text.clear();
        self.filter_kind = None;
        self.filter_favorite = false;
        self.filter_orientation = None;
        self.filter_aspect = None;
        self.filter_resolution = None;
        self.filter_min_rating = None;
        self.filter_ext = None;
        self.import_phase = ImportPhase::Idle;
        // The task panel and its retry map describe the previous library's
        // jobs; a stale retry would target the swapped-out store.
        self.tasks.clear();
        self.retryable.clear();
        self.task_panel_open = false;
        self.integrity_report = None;
        // The probe described the other library's vectors as much as its
        // endpoint; a result from before the swap would be about a store that
        // is no longer open.
        self.ai_probe = AiProbe::Idle;
        self.analysis_probe = AnalysisProbe::Idle;
        // Same for the query embedding and the search plan: they were
        // computed against the previous library's model rows and index.
        self.query_vector = None;
        self.ai_plan = None;
        // Duplicate clusters belong to the library they were computed in; the
        // field doc says "invalidated on cleanup and library swap" and this
        // is the swap half of that. A stale cache here would show the
        // previous library's clusters, and acting on them (trash by id) is a
        // silent no-op against the new store.
        self.duplicates = None;
        self.duplicates_computing = false;
        // The old watch task scans for the previous library; cancel it. The
        // caller restarts the service against the new library.
        if let Some(watch) = self.watch_task.take() {
            watch.manager.cancel(watch.task_id);
        }
        self.visible_assets.clear();
        self.reset_grid_page();
        self.generation += 1;
        Ok(())
    }

    pub fn set_search(&mut self, text: String) {
        self.search_text = text;
        // A plan is term-specific and carries no term of its own, so any
        // change invalidates it; the AI tier fetches a fresh one when it is
        // on.
        self.ai_plan = None;
        if self.search_text.trim().is_empty() {
            // A cleared search has nothing to fuse with. A *changed* term
            // deliberately keeps the old vector: the query compares the
            // term itself and ignores a stale one, so re-typing the same
            // words costs no second request.
            self.query_vector = None;
        } else {
            self.active_smart = None;
            // A typed search replaces the visual-search results view.
            self.close_visual_search();
        }
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Re-resolve the search tiers from the stored config. The settings page
    /// calls this when a toggle changes, so the next data pass sees the new
    /// choice without a restart.
    pub fn refresh_search_tiers(&mut self) {
        self.search_tiers = resolved_search_tiers(&AppConfig::load());
        // A tier change alters what the same search term returns, so the
        // workspace's cached data pass must be invalidated; `generation` is
        // part of its key.
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

    /// Replace the published visible-id list. Called by the workspace panel
    /// when it (re)lays out its rows, not once per frame — the list is a
    /// snapshot of the frozen rows, which only move on a view change.
    pub(crate) fn set_visible_assets(&mut self, ids: Vec<Uuid>) {
        self.visible_assets = ids;
    }

    /// The ids the grid is currently displaying, for Select-all and
    /// Shift-range. Empty until the panel has published a layout.
    pub fn visible_assets(&self) -> &[Uuid] {
        &self.visible_assets
    }

    /// The file behind `id`: the in-library blob for imported assets, the
    /// original path for linked ones. `None` when the record or the file is
    /// gone.
    pub fn asset_file(&self, id: Uuid) -> Option<PathBuf> {
        self.library.asset_file(id)
    }

    /// The primary selection's file, but only when that asset is an image —
    /// the one kind we can hand to the clipboard as pixels.
    pub fn primary_image_file(&self) -> Option<PathBuf> {
        let id = self.primary()?;
        let conn = self.library.store().conn();
        let asset = trove_core::store::assets::get(conn, id).ok().flatten()?;
        (asset.kind == AssetKind::Image).then(|| self.asset_file(id))?
    }

    /// Single click: select only `id` (`None` clears the selection).
    ///
    /// Selection is pure UI state: it must not bump the generation, or
    /// every click would invalidate the grid's cached data pass (a full
    /// SQL re-query plus a file stat per asset) and drag the whole frame
    /// out. The grid repaints the selection highlight from a plain
    /// re-render; the root observer switches the inspector tab.
    pub fn select_asset(&mut self, asset: Option<Uuid>) {
        self.selected_assets = Rc::new(asset.into_iter().collect());
        self.selection_anchor = asset;
        self.selection_source = SelectionSource::Plain;
        if let Some(id) = asset {
            self.record_view(id);
        }
    }

    /// Shift+click: replace the selection with the contiguous range between
    /// the anchor and `id` in the grid's display order. Without an anchor
    /// (or when either end is off-view) this degrades to a single select.
    pub fn select_range_to(&mut self, id: Uuid) {
        let anchor = self.selection_anchor;
        let Some(start) = anchor.or(Some(id)) else {
            return;
        };
        let flat = self.visible_assets.as_slice();
        let (a, b) = (
            flat.iter().position(|&x| x == start),
            flat.iter().position(|&x| x == id),
        );
        let range = match (a, b) {
            (Some(a), Some(b)) if a <= b => flat[a..=b].to_vec(),
            (Some(a), Some(b)) => flat[b..=a].to_vec(),
            _ => vec![id],
        };
        self.selected_assets = Rc::new(range);
        self.selection_anchor = Some(id);
        self.selection_source = SelectionSource::Multi;
        self.record_view(id);
    }

    /// Ctrl/Cmd click: toggle `id` in the selection.
    pub fn toggle_asset(&mut self, id: Uuid) {
        let selected = Rc::make_mut(&mut self.selected_assets);
        if let Some(pos) = selected.iter().position(|&x| x == id) {
            selected.remove(pos);
        } else {
            selected.push(id);
        }
        self.selection_anchor = Some(id);
        self.selection_source = SelectionSource::Multi;
        if let Some(primary) = self.selected_assets.last().copied() {
            self.record_view(primary);
        }
    }

    /// Ids an action applies to when triggered from a clicked asset: the whole
    /// selection if it contains the asset, otherwise just the asset.
    pub fn action_targets(&self, clicked: Uuid) -> Vec<Uuid> {
        if self.selected_assets.contains(&clicked) {
            (*self.selected_assets).clone()
        } else {
            vec![clicked]
        }
    }

    /// Drop assets from the selection (e.g. after moving them to the trash).
    pub fn deselect(&mut self, ids: &[Uuid]) {
        Rc::make_mut(&mut self.selected_assets).retain(|id| !ids.contains(id));
        self.generation += 1;
    }

    pub fn select_tag(&mut self, tag: Option<Uuid>) {
        self.active_tag = tag;
        self.active_smart = None;
        self.close_visual_search();
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
        self.selected_assets = Rc::new(self.visible_assets().to_vec());
        self.selection_source = SelectionSource::Multi;
    }

    /// Clear the selection entirely.
    pub fn clear_selection(&mut self) {
        if !self.selected_assets.is_empty() {
            self.selected_assets = Rc::new(Vec::new());
            self.selection_anchor = None;
            self.selection_source = SelectionSource::None;
        }
    }

    /// Enter the visual-search results view: the grid shows the ranked
    /// hits in place of the browsed view. The selection resets because the
    /// content under it changes.
    pub fn open_visual_search(
        &mut self,
        label: String,
        hits: Vec<(Uuid, f32)>,
        colour: Option<String>,
    ) {
        self.visual_results = Some(VisualSearchResults::new(label, hits, colour));
        self.selected_assets = Rc::new(Vec::new());
        self.selection_anchor = None;
        self.selection_source = SelectionSource::None;
        self.reset_grid_page();
        self.generation += 1;
    }

    /// Leave the visual-search results view; the grid goes back to the
    /// browsed view. No-op when no search is active.
    pub fn close_visual_search(&mut self) {
        if self.visual_results.take().is_some() {
            self.selected_assets = Rc::new(Vec::new());
            self.selection_anchor = None;
            self.selection_source = SelectionSource::None;
            self.reset_grid_page();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A swap no longer refuses while an import runs: the import is
    /// cancelled, the swap waits for the job thread to stop, and the
    /// controller lands on the new library with no job left running.
    #[test]
    fn a_swap_cancels_a_running_import_and_waits() {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];

        let root = std::env::temp_dir().join(format!("trove-ctl-swap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("cache")).unwrap();
        let mut controller = LibraryController::new(
            Library::open(&root, root.join("cache")).expect("the first library opens"),
        );

        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..300 {
            std::fs::write(src.join(format!("img{i}.png")), PNG_1X1).unwrap();
        }

        // Start a real import job the way the job bridge does, and register
        // it on the controller the same way.
        let options = trove_core::tasks::import::ImportOptions {
            data_root: root.clone(),
            cache_root: root.join("cache"),
            storage: trove_core::media::import::ImportStorage::Link,
            source: trove_core::tasks::import::ImportSource::Paths {
                paths: (0..300).map(|i| src.join(format!("img{i}.png"))).collect(),
                into_collection: None,
            },
        };
        let manager = controller.library.tasks().clone();
        let watcher = manager.clone();
        let (task_id, _rx) = manager
            .start(trove_core::tasks::TaskKind::Import, "test", move |ctx| {
                trove_core::tasks::import::run(&options, ctx)
            })
            .expect("the import job starts");
        controller.import_task = Some(crate::library::jobs::ImportTaskHandle { manager, task_id });
        controller.begin_import(300);
        assert!(controller.is_importing());

        let other = root.join("other");
        std::fs::create_dir_all(other.join("cache")).unwrap();

        let swapped = controller.swap_library(other.clone(), other.join("cache"));
        assert!(swapped.is_ok(), "the swap must not be refused: {swapped:?}");
        assert_eq!(controller.library.root(), other);
        assert!(
            !controller.is_importing(),
            "the swap resets the import phase"
        );
        assert!(controller.import_task.is_none());
        // Give the cancelled job a beat to exit, then confirm it stopped.
        for _ in 0..100 {
            if !watcher.is_task_running(task_id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !watcher.is_task_running(task_id),
            "the cancelled import must have stopped"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
