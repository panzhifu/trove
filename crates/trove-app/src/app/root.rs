//! The application's root view.
//!
//! This is the window content the app shell assembles: it owns the
//! [`LibraryController`], composes the dock layout and the title bar, and hosts
//! a whole-window file-drop surface. `main` only boots the window and mounts
//! this view inside a `Root`.

use gpui_kit::base::animation::{ease_in_cubic, ease_out_cubic};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dock::{
    DockLayout, DockPlacement, InsertTarget, PaneRef, PanelId, panel_handle,
};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::prelude::FluentBuilder as _;

// Re-export gpui's `Widget`/styled-building names (div, Window, Context,
// Render, IntoElement, ExternalPaths, …) plus gpui-kit's styling extensions.
use gpui_kit::*;

use super::settings_write;
use crate::app::actions::*;
use crate::app::title_bar::TitleBarView;
use crate::app::tray;
use crate::app::{capture, status_bar};
use crate::components::preview::chrome::{self, Chrome};
use crate::library::jobs;
use crate::library::{LibraryController, SelectionSource};
use crate::panels::{ExplorerPanel, FoldersPanel, InspectorPanel, TagsPanel, WorkspacePanel};
use trove_core::config::{AppConfig, LibraryEntry};
use trove_core::library::Library;
use trove_core::services::update;
use uuid::Uuid;

/// The library to open at startup: the recorded one, or a freshly registered
/// default on a first run. Both directories are created on the way out.
fn open_library_at_startup() -> Library {
    let mut config = AppConfig::load();
    let entry = config
        .ensure_active_library()
        .unwrap_or_else(|e| panic!("prepare library: {e}"));
    Library::open(entry.dir(), entry.cache_dir()).unwrap_or_else(|e| panic!("open library: {e}"))
}

/// Ask GitHub for the newest release on the background executor, then stamp
/// the check and repaint.
///
/// `delay` keeps the launch path from competing with the first paint and the
/// startup library scan; the Help menu and Settings pass zero.
///
/// Every surface reads the same state out of [`trove_core::services::update`],
/// so one `refresh_windows` updates the status bar, the About dialog and the
/// Settings row together. The timestamp is written even when the check fails:
/// an offline machine should not probe GitHub again on every single launch.
fn spawn_update_check(cx: &mut App, delay: std::time::Duration) {
    cx.spawn(async move |cx| {
        if !delay.is_zero() {
            cx.background_executor().timer(delay).await;
        }
        cx.background_executor()
            .spawn(async { update::check_now(env!("CARGO_PKG_VERSION")) })
            .await;
        cx.update(|cx| {
            let mut config = AppConfig::load();
            settings_write::note(
                config.record_update_check(update::now_unix()),
                "update check recorded",
            );
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Help ▸ Check for Updates… and the Settings button: the same probe, run the
/// moment the user asks for it.
pub(crate) fn run_update_check(cx: &mut App) {
    spawn_update_check(cx, std::time::Duration::ZERO);
}

/// The About page's Download button: stage this platform's installer under
/// the state directory, progress and all. Two tasks — the download itself on
/// the background executor, and a ticker that repaints the windows while
/// bytes land, because the row re-reads the state per frame but frames only
/// happen when something asks for one.
pub(crate) fn start_update_download(version: String, cx: &mut App) {
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { update::download_and_stage(&version) })
            .await;
        // The state carries the same failure for the row to show; this line
        // is for the log.
        if let Err(error) = &result {
            tracing::warn!(%error, "update download failed");
        }
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            if !matches!(
                update::download_state(),
                update::DownloadState::Downloading { .. }
            ) {
                break;
            }
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

/// The release worth telling the user about, or `None`.
///
/// The status bar and the About dialog both ask on every frame, so the config
/// file is only read when a newer release is actually on the table — and a
/// version the user already waved off is filtered out here rather than inside
/// the update service, which knows nothing about preferences.
pub(crate) fn pending_update() -> Option<(String, String)> {
    let (version, page) = update::available()?;
    let skipped = AppConfig::load().skipped_version().map(str::to_string);
    if skipped.as_deref() == Some(version.as_str()) {
        return None;
    }
    Some((version, page))
}

/// The running session's controller, registered by [`AppView::new`]. The
/// library manager's switch reads it to hot-swap the open library in
/// place instead of opening a second main window.
#[derive(Default)]
pub(crate) struct SessionState(pub(crate) Option<gpui::WeakEntity<LibraryController>>);

impl gpui_kit::Global for SessionState {}

/// Marker for the keyed repository-package toast: the standing
/// "exporting…"/"importing…" note is replaced by the outcome instead of
/// stacking one toast per step.
pub struct RepositoryNotice;

/// Hand the session's main window over to `entry` — the library manager's
/// hot-swap path, from inside the main window itself. Returns whether the
/// swap happened: a refused switch (an import mid-flight, a library that
/// will not open) leaves the current one open.
fn swap_session_to(cx: &mut App, entry: &LibraryEntry) -> bool {
    let Some(controller) = cx
        .try_global::<SessionState>()
        .and_then(|state| state.0.as_ref().and_then(|weak| weak.upgrade()))
    else {
        return false;
    };
    controller.update(cx, |ctl, cx| {
        let swapped = ctl
            .swap_library(entry.dir(), entry.cache_dir())
            .and_then(|()| AppConfig::load().set_active_library(&entry.slug))
            .is_ok();
        if swapped {
            // The old watch task scanned for the previous library;
            // restart the resident watch on the new one.
            if let Some(handle) = ctl.watch_handle {
                let entity = cx.entity();
                crate::library::jobs::start_watch_service(&entity, handle, cx);
            }
            ctl.generation += 1;
            cx.notify();
        }
        swapped
    })
}

// ============================ stage motion ===================================

/// How long the stage takes to arrive: a fade, the last few percent of its
/// size and a few pixels of rise, played as one gesture. Long enough to read
/// as motion, short enough that the picture a user asked for is not late.
const STAGE_ENTER_TIME: std::time::Duration = std::time::Duration::from_millis(180);

/// How long the exit fade plays before the window goes back to the shell.
/// Shorter than the entrance — leaving should feel quicker than arriving.
const STAGE_EXIT_TIME: std::time::Duration = std::time::Duration::from_millis(140);

/// The size the stage settles from and sinks back to, as a fraction of the
/// window: three percent under full, the distance between "appeared" and
/// "arrived". The stage content re-fits into the shrinking wrapper every
/// frame — the still and the video picture scale with it, the text block
/// keeps its user-dialled size and merely breathes with the clip window.
const STAGE_SETTLE_SCALE: f32 = 0.97;

/// How far the stage rises while it settles in, in px, and sinks back while
/// it leaves. Same order of smallness as the grid tile entrance, so the two
/// read as one house gesture.
const STAGE_SETTLE_RISE: f32 = 8.0;

/// Root view: owns the controller and hosts the dock area, plus a drop
/// surface that imports any dropped files into the current collection.
pub struct AppView {
    controller: Entity<LibraryController>,
    /// The renderer description last seen on the workspace panel, carried
    /// into the status bar. The panel only notifies when it changes, and this
    /// copy is compared against again on each notify so a redundant one can
    /// never re-render the whole app view.
    viewport_backend: Option<String>,
    dock: Entity<gpui_kit::component::dock::DockArea>,
    /// The inspector panel entity, kept so the selection observer can switch
    /// the right dock to its tab (see [`AppView::show_inspector`]).
    inspector: Entity<InspectorPanel>,
    /// The selection the auto-show observer last saw: showing the inspector
    /// only when the selection actually changed, so unrelated controller
    /// notifies never steal the right dock's tab.
    last_selection: Vec<Uuid>,
    title_bar: Entity<TitleBarView>,
    /// The workspace panel: reached for its preview's surface when the
    /// fullscreen stage is on.
    workspace: Entity<WorkspacePanel>,
    /// Whether this window is currently the fullscreen preview stage — the
    /// window itself goes fullscreen and renders the preview's own surface
    /// (the video player, the still, the model viewport, the subtitle
    /// editor), so no second window, player or soundtrack is ever built.
    stage_fullscreen: bool,
    /// Focus handle for that stage. It has to hold the window's focus while
    /// it is up: actions and key bindings are dispatched from the focused
    /// element upwards, and the element that had the focus when the stage
    /// came up is no longer in the frame — without this the dispatch falls
    /// back to the window root, which never sees the stage's context, and
    /// neither the exit button nor Esc can leave the stage.
    stage_focus: FocusHandle,
    /// Watches every keystroke while the stage is up. Esc leaves through the
    /// same exit action as the button; the key binding alone cannot be
    /// relied on because it is matched against the focused element's
    /// context, and a focus that slipped off the stage would leave Esc
    /// dead. Watching globally removes that dependency. Dropped on leave,
    /// which unregisters it.
    video_escape: Option<Subscription>,
    /// Which stage transition comes next. Every enter and every leave bumps
    /// it, and the count rides in the transition's element id, so each run's
    /// animation starts from zero — including the exit, which would
    /// otherwise inherit the entrance's finished state under a shared id
    /// and read `delta = 1` on its first frame: gone without a fade.
    stage_generation: u64,
    /// The stage is playing its exit fade, and the real teardown — the
    /// window back to the shell — is waiting for it in
    /// [`Self::finish_leave_stage_fullscreen`]. Esc and `f` during the beat
    /// find the guard in [`Self::leave_stage_fullscreen`] closed and wait
    /// like everyone else; nothing re-enters a stage that is already
    /// leaving, because [`Self::enter_stage_fullscreen`] guards on
    /// `stage_fullscreen`, which stays up until the fade lands.
    stage_closing: bool,
    /// The stage corner exit's auto-hide chrome, shared with the players'
    /// bars: it shows for the first stretch after the stage comes up,
    /// reveals on any pointer movement over the stage, and hides once the
    /// pointer has rested — hovered, it pins itself up so it cannot fade
    /// under the hand reaching for it. Rebuilt on every entry.
    stage_chrome: Chrome,
    /// The tray icon, when the desktop has a tray that took it. `None` means
    /// no tray: the app then closes the old-fashioned way.
    tray: Option<tray::Tray>,
    /// Set while the app is quitting, so the close guard lets the window go.
    /// Shared with the `on_window_should_close` closure, which cannot reach
    /// the view itself.
    quitting: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Kept alive for the life of the view: dropping it would unregister the
    /// OS light/dark observer that re-applies the appearance.
    _appearance: Subscription,
    /// The image cache every image in the app resolves through. Thumbnail
    /// files are keyed by content hash on disk, so a maintenance rebuild
    /// rewrites them under paths a path-keyed cache treats as immutable —
    /// clearing the cache is what makes the grid read the new bytes without
    /// a restart.
    images: Entity<gpui::RetainAllImageCache>,
    /// The thumbnail epoch last seen on the controller, so the cache above
    /// is cleared exactly once per rewrite.
    seen_thumb_epoch: u64,
}

impl AppView {
    /// Re-start an interrupted embedding backfill: the same entry point the
    /// retry button uses, driven at startup by the journal's interrupted
    /// record instead of a click. The starter re-derives its own inputs and
    /// refuses (with a notice) when the embedding provider is no longer
    /// configured, so a stale journal entry cannot force a doomed run.
    fn auto_resume_backfill(
        &mut self,
        controller: &Entity<LibraryController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::library::jobs::start_embedding_backfill_app(controller, window, cx);
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Follow the OS light/dark switch while it runs (the startup apply
        // happened before this window existed).
        let _appearance = window.observe_window_appearance(|window, cx| {
            crate::app::theme::apply_from_settings(Some(window), cx);
        });
        let library = open_library_at_startup();
        // Interrupted sweep jobs (embedding backfill — checkpointed, idempotent,
        // and a spend the user already opted into) resume on their own instead
        // of waiting for a manual retry; per-asset AI jobs stay manual, because
        // silently continuing those re-spends money without a new ask. The
        // check reads the library before the controller takes ownership of it.
        let resume_backfill = library
            .interrupted_tasks()
            .iter()
            .any(|entry| entry.kind == trove_core::tasks::TaskKind::EmbeddingBackfill)
            && !library
                .tasks()
                .is_running(&trove_core::tasks::TaskKind::EmbeddingBackfill);
        let controller = cx.new(|_cx| LibraryController::new(library));
        // The local embedder's weights take seconds to reach the device and
        // the provider cache is per process, so without this the first
        // committed search pays the whole load before its fused refinement
        // lands. The gate inside skips every configuration that would not
        // ask the provider (tier off, cloud engine, model not on disk).
        crate::library::jobs::warm_local_embedder_app(cx);
        if resume_backfill {
            let weak = controller.downgrade();
            cx.defer_in(window, move |this, window, cx| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                this.auto_resume_backfill(&controller, window, cx);
            });
        }
        // This window is now the running session: the library manager's
        // library switch swaps its library through this handle.
        cx.set_global(crate::app::root::SessionState(Some(controller.downgrade())));
        let title_bar = cx.new(TitleBarView::new);

        let explorer = cx.new(|cx| ExplorerPanel::new(window, cx, controller.clone()));
        let folders = cx.new(|cx| FoldersPanel::new(cx, controller.clone()));
        let workspace = cx.new(|cx| WorkspacePanel::new(window, cx, controller.clone()));
        let tags = cx.new(|cx| TagsPanel::new(window, cx, controller.clone()));
        let inspector = cx.new(|cx| InspectorPanel::new(window, cx, controller.clone()));

        // Status bar ← model renderer: the workspace panel watches its model
        // viewport and only speaks up when the renderer description changes
        // (GPU adapter name, CPU fallback reason, stream progress), so this
        // observer does not fire on every drag frame.
        cx.observe(&workspace, |this, workspace, cx| {
            let backend = workspace.read(cx).viewport_backend().map(str::to_string);
            if backend != this.viewport_backend {
                this.viewport_backend = backend;
                cx.notify();
            }
        })
        .detach();

        // Trove draws its own title bar (see `app::dock_skin`): the framework
        // skin always appends a "⋯" menu, and there is no switch for it.
        let dock = crate::app::dock_skin::dock_area("trove", None, window, cx);
        // The dock keeps its own handle on the panel; this clone is what the
        // status bar reads through.
        let workspace_view = workspace.clone();
        dock.update(cx, |area, cx| {
            // Panels must be registered through `panel_handle` + `panel_view`:
            // a bare entity (`DockLayout::panel`) cannot be downcast back into
            // a presentation handle by the skin, so `title`/`title_suffix`
            // hooks are never called and the title bar falls back to
            // `panel_name`.
            area.set_dock(
                DockPlacement::Left,
                DockLayout::tabs()
                    .panel_view(panel_handle(explorer), cx)
                    .panel_view(panel_handle(folders), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Left, px(260.), window, cx);
            area.set_center(
                DockLayout::tabs().panel_view(panel_handle(workspace.clone()), cx),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Right,
                DockLayout::tabs()
                    .panel_view(panel_handle(tags), cx)
                    .panel_view(panel_handle(inspector.clone()), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Right, px(300.), window, cx);
        });

        // Auto-show the inspector: whenever a *plain* selection change
        // (single click / keyboard move) lands on a non-empty selection,
        // switch the right dock to the inspector tab and open the dock if
        // collapsed. Multi-select gestures (Ctrl toggle / Shift range) and
        // clears never steal the tab.
        cx.observe_in(&controller, window, move |this, controller, window, cx| {
            // A finished transcription leaves its subtitle-export request on
            // the controller; only here is there a window to write the
            // sidecars and to ask before overwriting one that exists.
            if let Some(request) = controller.update(cx, |ctl, _| ctl.pending_subtitle_save.take())
            {
                crate::library::jobs::auto_save_subtitles_app(&controller, request, window, cx);
            }
            // A thumbnail rebuild rewrites the files but not their paths; the
            // epoch is the signal to drop the cached decodes so the next
            // paint re-reads the pictures from disk.
            let thumb_epoch = controller.read(cx).thumb_epoch;
            if this.seen_thumb_epoch != thumb_epoch {
                this.seen_thumb_epoch = thumb_epoch;
                this.images
                    .update(cx, |images, cx| images.clear(window, cx));
            }
            let (plain, selection) = {
                let ctl = controller.read(cx);
                (
                    ctl.selection_source == SelectionSource::Plain,
                    (*ctl.selected_assets).clone(),
                )
            };
            let changed = this.last_selection != selection;
            this.last_selection = selection;
            if changed && plain && !this.last_selection.is_empty() {
                this.show_inspector(window, cx);
            }
            cx.notify();
        })
        .detach();

        crate::library::jobs::start_watch_service(&controller, window.window_handle(), cx);
        crate::library::jobs::start_index_drain_service(&controller, window.window_handle(), cx);
        start_collect_server(cx);

        let tray = tray::Tray::install();
        // Whether the app is on its way out: while it is, closing the window
        // must actually close it, or the guard below would keep a quitting
        // app alive with no window to show. Shared with that closure, which
        // cannot reach the view.
        let quitting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Closing the window minimizes it instead of ending the app — but
        // only when there is a tray to come back from, or the close button
        // would become a way to strand the process with no window at all.
        // The flag is the way out: while quitting, a close is a close.
        let has_tray = tray.is_some();
        let quitting_for_close = quitting.clone();
        window.on_window_should_close(cx, move |window, cx| {
            // The settings window goes wherever the main window goes: it is
            // the main window's companion, and closing (or tucking away) the
            // main window must not leave it floating behind.
            crate::dialogs::settings::close(cx);
            if quitting_for_close.load(std::sync::atomic::Ordering::Relaxed) {
                true
            } else if has_tray {
                window.minimize_window();
                false
            } else {
                true
            }
        });
        if has_tray {
            // The tray's own thread cannot touch windows or the library, so
            // its commands are queued and drained here, where they can.
            cx.spawn_in(window, async move |view, cx| {
                loop {
                    cx.background_executor().timer(tray::POLL).await;
                    let _ = view.update_in(cx, |this, window, cx| {
                        if let Some(command) = this.tray.as_ref().and_then(tray::Tray::poll) {
                            this.run_tray_command(command, window, cx);
                        }
                    });
                }
            })
            .detach();
        }

        // At most one release check a day, well after the window is up (see
        // `update_check_due`). An unreachable GitHub is silent by design:
        // offline is the normal case for a local-first app.
        let config = AppConfig::load();
        if config.update_check() && config.update_check_due(update::now_unix()) {
            spawn_update_check(cx, update::STARTUP_DELAY);
        }

        // Read the panel's initial state here: the observe registration
        // above only speaks up on a change, so the first value has to be
        // picked up by hand.
        let initial_backend = workspace_view
            .read(cx)
            .viewport_backend()
            .map(str::to_string);
        let thumb_epoch = controller.read(cx).thumb_epoch;
        Self {
            controller,
            viewport_backend: initial_backend,
            dock,
            inspector,
            last_selection: Vec::new(),
            title_bar,
            workspace,
            stage_fullscreen: false,
            stage_focus: cx.focus_handle(),
            video_escape: None,
            stage_generation: 0,
            stage_closing: false,
            stage_chrome: Chrome::new(),
            tray,
            quitting,
            _appearance,
            images: gpui::RetainAllImageCache::new(cx),
            seen_thumb_epoch: thumb_epoch,
        }
    }

    /// Answer a click from the tray menu. Everything here runs on the main
    /// thread, which is the only place a window or the library may be
    /// touched.
    fn run_tray_command(
        &mut self,
        command: tray::TrayCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            tray::TrayCommand::ShowWindow => {
                // The window was only minimized, never closed, so it is
                // still there to bring back.
                window.activate_window();
                cx.activate(true);
            }
            // Deliberately not `dispatch_action`: that starts at whatever
            // holds the focus, and a minimized window may hold none — in
            // which case the dispatch falls back to the window root and
            // never reaches this view's handlers. Calling the same two
            // functions the actions call keeps the tray independent of
            // where the focus happens to be.
            tray::TrayCommand::ImportFiles => {
                // The window has to be back before a picker opens over it.
                window.activate_window();
                cx.activate(true);
                self.prompt_import(window, cx);
            }
            tray::TrayCommand::OpenSettings => {
                window.activate_window();
                cx.activate(true);
                crate::dialogs::settings::open(cx, self.controller.clone());
            }
            tray::TrayCommand::Quit => {
                // Drop the icon first: the app must not stay in the shell
                // after it has gone, nor look alive while it is closing.
                self.tray = None;
                // Let the window go, or the close guard below would keep a
                // quitting app alive with nothing on screen.
                self.quitting
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                cx.quit();
            }
        }
    }

    /// Bring the inspector tab to the front of the right dock, opening the
    /// dock first when it is collapsed. A no-op when the inspector is already
    /// the displayed tab or lives outside the right dock (the user may have
    /// dragged it elsewhere).
    fn show_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel_id = PanelId::from(self.inspector.entity_id());
        self.dock.update(cx, |area, cx| {
            if !area.is_dock_open(DockPlacement::Right) {
                area.toggle_dock(DockPlacement::Right, window, cx);
            }
            // Find the inspector's tab group and keep its current slot, so
            // the switch only changes the displayed tab, never the order.
            let target = area.layout(DockPlacement::Right).and_then(|tree| {
                let node = tree.find_panel_node(panel_id)?;
                match tree.find_node(node)?.kind() {
                    PaneRef::Tabs { panels, active_ix } => {
                        if panels.get(active_ix) == Some(&panel_id) {
                            return None;
                        }
                        panels
                            .iter()
                            .position(|panel| *panel == panel_id)
                            .map(|ix| (node, ix))
                    }
                    _ => None,
                }
            });
            if let Some((node, ix)) = target {
                area.move_panel(
                    panel_id,
                    InsertTarget::Tabs {
                        node,
                        ix: Some(ix),
                        activate: true,
                    },
                    window,
                    cx,
                );
            }
        });
    }

    /// Edit ▸ Paste Import: bring the clipboard image into the library. Text
    /// entries are ignored for now (URL import is on the roadmap).
    fn paste_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(gpui::ClipboardEntry::Image(image)) = item.entries().first() else {
            return;
        };
        let ext = match image.format {
            gpui::ImageFormat::Png => "png",
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Gif => "gif",
            gpui::ImageFormat::Webp => "webp",
            _ => "png",
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("trove-paste-{nanos}.{ext}"));
        if std::fs::write(&path, &image.bytes).is_ok() {
            // Copy, not link: the temp file is ours and disposable, and a
            // link would leave the asset pointing at a path the system may
            // empty at any moment.
            jobs::import_copied_app(&self.controller, vec![path], window, cx);
        }
    }

    /// Edit ▸ Copy Image: hand the primary selection's pixels to the system
    /// clipboard. Decoding a large photo can take a moment, so the work runs
    /// on the background executor and reports through a notification.
    fn copy_primary_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.controller.read(cx).primary_image_file();
        let Some(path) = path else {
            window.push_notification(
                Notification::warning(rust_i18n::t!("notice.copy_image_none").to_string()),
                cx,
            );
            return;
        };
        let handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            let target = path.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { crate::library::clipboard::copy_image(&target) })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let note = match outcome {
                    Ok((w, h)) => Notification::success(
                        rust_i18n::t!(
                            "notice.copy_image_done",
                            width = w.to_string(),
                            height = h.to_string()
                        )
                        .to_string(),
                    ),
                    Err(error) => Notification::warning(
                        rust_i18n::t!("notice.copy_image_failed", error = error.to_string())
                            .to_string(),
                    ),
                };
                window.push_notification(note, cx);
            });
        })
        .detach();
    }

    /// File ▸ Screenshot: pick a region and import the PNG.
    ///
    /// One entry point covers every target the capture chain knows how to
    /// reach: what the user chooses in the picker is what gets imported, so
    /// there is no menu full of granularities to pick between first. On Linux
    /// the picker is ours — a frozen frame with the compositor's window list,
    /// where a click can also mean "this window"; elsewhere the platform's own
    /// region picker is the whole interaction.
    fn take_screenshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use trove_core::services::screenshot;

        // The capture lands in the incoming directory and stays there: the
        // import links it, so this is its permanent home.
        let dest = screenshot::destination(&trove_core::paths::incoming_dir());
        let controller = self.controller.clone();
        let handle = window.window_handle();

        tracing::info!(dest = %dest.display(), "take screenshot");

        window.push_notification(
            Notification::info(rust_i18n::t!("notice.screenshot_started").to_string()),
            cx,
        );

        #[cfg(target_os = "linux")]
        capture::open_capture_picker(controller, dest, handle, cx);

        // No in-process overlay to draw on these platforms: `screencapture -i`
        // is the region picker, and the chain reports back what it wrote.
        #[cfg(not(target_os = "linux"))]
        capture::run_capture_chain(
            screenshot::CaptureTarget::PickArea,
            None,
            dest,
            controller,
            handle,
            cx,
        );
    }

    /// Put the OS window into (or out of) fullscreen, idempotently: the only
    /// platform primitive is a toggle, so asking for the state it is already
    /// in would flip it the wrong way — which is what happened when the user
    /// had gone fullscreen with their own window-manager key first.
    fn set_window_fullscreen(window: &Window, on: bool) {
        if window.is_fullscreen() != on {
            window.toggle_fullscreen();
        }
    }

    /// Take the stage over: the window goes fullscreen and renders the
    /// preview's own surface, which simply keeps doing what it was doing —
    /// the video keeps playing, the still keeps its pan/zoom, the model
    /// keeps its camera. Leaving gives the window back; nothing on the
    /// stage notices either way.
    fn enter_stage_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Already the stage: answering twice would toggle the window back
        // out of fullscreen under the user.
        if self.stage_fullscreen {
            return;
        }
        let Some(stage) = self.workspace.read(cx).preview_stage(cx) else {
            return;
        };
        drop(stage);
        self.workspace
            .update(cx, |ws, cx| ws.set_stage_mode(true, cx));
        self.stage_fullscreen = true;
        // A fresh transition id: the entrance the next frame paints must run
        // from its own zero, not inherit whatever state the previous stage's
        // animation left under this node.
        self.stage_generation += 1;
        // Fresh chrome: the corner exit shows for the first stretch so it is
        // discovered, then this visit's watcher hides it like any other
        // reveal. The loop ends on the first tick after the stage is down,
        // so each entry owns exactly one watcher and none outlives its visit.
        self.stage_chrome = Chrome::new();
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(chrome::WATCH_INTERVAL).await;
                let Ok(keep) = view.update(cx, |this, cx| {
                    if !this.stage_fullscreen {
                        return false;
                    }
                    if this.stage_chrome.tick() {
                        cx.notify();
                    }
                    true
                }) else {
                    break;
                };
                if !keep {
                    break;
                }
            }
        })
        .detach();
        Self::set_window_fullscreen(window, true);
        // Take the focus now: leaving it on whatever had it means the
        // dispatch tree falls back to the window root as soon as that
        // element leaves the frame, which strands the stage (see
        // [`AppView::stage_focus`]).
        window.focus(&self.stage_focus, cx);
        // Belt and braces for Esc and the fullscreen key: this watch fires
        // only when a keystroke resolved to nothing (the docs: after
        // everything else, skipped if propagation stopped) — that is,
        // exactly when the stage's own key bindings could not see the
        // focus. It then leaves through the same action the button uses.
        self.video_escape = Some(cx.observe_keystrokes(
            |_this: &mut Self,
             event: &KeystrokeEvent,
             window: &mut Window,
             cx: &mut Context<Self>| {
                if matches!(event.keystroke.key.as_str(), "escape" | "f") {
                    window.dispatch_action(Box::new(ExitVideoFullscreen), cx);
                }
            },
        ));
        cx.notify();
    }

    /// Give the window back to the shell — but not all at once. The stage
    /// first plays its exit fade: `stage_closing` keeps it on screen while it
    /// dissolves, the video keeps playing through the beat, and the teardown
    /// below lands when the fade does. Reduced motion skips the beat — there
    /// is nothing to watch, so the window goes straight back.
    fn leave_stage_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The mirror of the guard above: an Exit arriving while the stage is
        // down must not throw the window into fullscreen. A second one while
        // the exit fade is playing finds `stage_closing` closed — the leaving
        // has already begun, and cannot begin again.
        if !self.stage_fullscreen || self.stage_closing {
            return;
        }
        if cx.reduce_motion() {
            self.finish_leave_stage_fullscreen(window, cx);
            return;
        }
        self.stage_closing = true;
        // A fresh transition id, distinct from the entrance's: reusing one id
        // would let the exit inherit the entrance's elapsed time and read as
        // an instant disappearance instead of a fade.
        self.stage_generation += 1;
        cx.spawn_in(window, async move |view, cx| {
            // One beat past the fade, so the teardown never outruns the last
            // frame of it: a stage that vanishes at three percent opacity
            // reads exactly like the hard cut this replaces.
            cx.background_executor()
                .timer(STAGE_EXIT_TIME + std::time::Duration::from_millis(50))
                .await;
            let _ = view.update_in(cx, |this, window, cx| {
                this.finish_leave_stage_fullscreen(window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// The teardown behind [`Self::leave_stage_fullscreen`]: stop watching
    /// keystrokes, hand the window and the focus back to the workspace.
    /// Idempotent — the render path calls it directly when the preview dies
    /// mid-stage (nothing left to fade), and the exit timer may land after
    /// that has already happened.
    fn finish_leave_stage_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stage_closing = false;
        if !self.stage_fullscreen {
            return;
        }
        // Stop watching keystrokes first: dropping the subscription
        // unregisters it, so the Esc that leaves cannot be seen twice.
        self.video_escape = None;
        self.workspace
            .update(cx, |ws, cx| ws.set_stage_mode(false, cx));
        self.stage_fullscreen = false;
        Self::set_window_fullscreen(window, false);
        // Hand the focus back to the workspace: the stage's handle leaves
        // the frame with it, and a focus pointing at nothing would strand
        // the next stage the same way.
        let workspace_focus = self.workspace.read(cx).focus_handle(cx);
        window.focus(&workspace_focus, cx);
        cx.notify();
    }

    /// File ▸ Import files… : system file picker, then background import.
    fn prompt_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(rust_i18n::t!("app.import_prompt").into_owned().into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(result) = rx.await
                && let Ok(Some(paths)) = result
            {
                let _ = handle.update(cx, |_view, window, cx| {
                    jobs::import_paths_app(&ctl, paths, window, cx);
                });
            }
        })
        .detach();
    }

    /// File ▸ Import from URL… : prompt for a link, download it in the
    /// background into the collect inbox, then run the standard inbox drain
    /// (import + record the source URL on the asset).
    fn prompt_import_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(
                rust_i18n::t!("app.import_url_placeholder")
                    .into_owned()
                    .to_string(),
            )
        });
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        window.open_dialog(cx, move |dialog, _, _| {
            let url_input = url_input.clone();
            let ctl = ctl.clone();
            // `handle` is `AnyWindowHandle` (Copy): each `move` closure below
            // captures its own copy, no explicit clone needed.
            dialog
                .title(rust_i18n::t!("app.import_url").to_string())
                .width(px(460.))
                .child(Input::new(&url_input).small().appearance(true))
                .on_ok(move |_, _, cx| {
                    let url: String = url_input.read(cx).value().trim().to_string();
                    if !url.is_empty() {
                        let ctl = ctl.clone();
                        let task = cx.background_executor().spawn(async move {
                            trove_core::services::collect::fetch_to_inbox(&url)
                        });
                        cx.spawn(async move |cx| {
                            let result = task.await;
                            let _ = handle.update(cx, |_view, window, cx| match result {
                                Ok(name) => {
                                    // Drain the inbox right away; when an
                                    // import is already running the file
                                    // stays queued for the watcher's next
                                    // sweep, and the user is told so. A file
                                    // the library already holds (the same name
                                    // and size — a re-download) says nothing:
                                    // the outcome would have been a no-op too.
                                    match jobs::collect_inbox_app(&ctl, window, cx) {
                                        jobs::InboxDrain::Started | jobs::InboxDrain::Idle => {}
                                        jobs::InboxDrain::Refused
                                        | jobs::InboxDrain::CapReached => {
                                            window.push_notification(
                                                Notification::info(
                                                    rust_i18n::t!(
                                                        "notice.import_url_queued",
                                                        name = name
                                                    )
                                                    .to_string(),
                                                ),
                                                cx,
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    window.push_notification(
                                        Notification::warning(
                                            rust_i18n::t!("notice.download_failed", error = e)
                                                .to_string(),
                                        ),
                                        cx,
                                    );
                                }
                            });
                        })
                        .detach();
                    }
                    true
                })
        });
    }

    /// File ▸ Export repository file… : save-dialog, then write the active
    /// library — records, its own media, and copies of the linked files — as
    /// one `.trove` package. The build runs on the background executor: the
    /// package writer snapshots the database itself, so the open library is
    /// never touched from off the main thread. A keyed toast stands in while
    /// the package builds and the outcome replaces it — a library with
    /// gigabytes of linked files should not export in silence.
    fn prompt_export_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = AppConfig::load().active_entry();
        let suggested = trove_core::services::repo_package::package_file_name(&entry.name);
        let rx = cx.prompt_for_new_path(&entry.dir(), Some(suggested.as_str()));
        let handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                // The save dialog has no extension filter, so the format's
                // extension is the app's to enforce: whatever the dialog
                // came back with, the file the user chose ends in `.trove`.
                let path = match path.extension().and_then(|e| e.to_str()) {
                    Some(e) if e.eq_ignore_ascii_case("trove") => path,
                    _ => path.with_extension("trove"),
                };
                let _ = handle.update(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::info(
                            rust_i18n::t!("app.export_repository_started").to_string(),
                        )
                        .id1::<RepositoryNotice>("repository-package"),
                        cx,
                    );
                });
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::services::repo_package::export_library_package(
                            &entry.dir(),
                            &entry.name,
                            &path,
                        )
                    })
                    .await;
                let _ = handle.update(cx, |_, window, cx| {
                    let note = match outcome {
                        Ok(report) if report.linked_missing == 0 => Notification::success(
                            rust_i18n::t!(
                                "app.export_repository_done",
                                path = report.path.display().to_string(),
                                files = report.files
                            )
                            .to_string(),
                        ),
                        Ok(report) => Notification::warning(
                            rust_i18n::t!(
                                "app.export_repository_missing",
                                path = report.path.display().to_string(),
                                count = report.linked_missing
                            )
                            .to_string(),
                        ),
                        Err(e) => Notification::warning(
                            rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                        ),
                    }
                    .id1::<RepositoryNotice>("repository-package");
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    /// File ▸ Import repository file… : pick a `.trove` package, register a
    /// library named after it, unpack it on the background executor, then
    /// switch this window to the imported library. A failure rolls the
    /// registration back — an entry pointing at half a package helps nobody.
    fn prompt_import_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                rust_i18n::t!("app.import_repository_prompt")
                    .into_owned()
                    .into(),
            ),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(archive) = paths.first()
            {
                // Read the manifest first: the library name decides the
                // registry entry, and the registry entry decides the
                // directory the package unpacks into.
                let read = {
                    let archive = archive.clone();
                    cx.background_executor()
                        .spawn(async move {
                            trove_core::services::repo_package::read_manifest(&archive)
                        })
                        .await
                };
                let manifest = match read {
                    Ok(manifest) => manifest,
                    Err(_) => {
                        let _ = handle.update(cx, |_, window, cx| {
                            window.push_notification(
                                Notification::warning(
                                    rust_i18n::t!("app.import_repository_failed").to_string(),
                                ),
                                cx,
                            );
                        });
                        return;
                    }
                };
                let entry = {
                    let name = manifest.library.name.clone();
                    let mut config = AppConfig::load();
                    config.add_library(&name).ok()
                };
                let Some(entry) = entry else {
                    return;
                };

                let _ = handle.update(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::info(
                            rust_i18n::t!("app.import_repository_started").to_string(),
                        )
                        .id1::<RepositoryNotice>("repository-package"),
                        cx,
                    );
                });

                let archive = archive.clone();
                let dest = entry.dir();
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::services::repo_package::install_library_package(&archive, &dest)
                    })
                    .await;

                match outcome {
                    Ok(report) => {
                        // Hand the window over to the imported library, the
                        // same hot-swap the library manager's enter performs.
                        let switched = handle
                            .update(cx, |_, _, cx| swap_session_to(cx, &entry))
                            .unwrap_or(false);
                        let _ = handle.update(cx, |_, window, cx| {
                            let note = Notification::success(
                                rust_i18n::t!(
                                    "app.import_repository_done",
                                    name = report.library_name,
                                    assets = report.assets_total,
                                    materialized = report.materialized,
                                    kept = report.kept_linked,
                                    missing = report.missing
                                )
                                .to_string(),
                            )
                            .id1::<RepositoryNotice>("repository-package");
                            window.push_notification(note, cx);
                            if !switched {
                                cx.refresh_windows();
                            }
                        });
                    }
                    Err(_) => {
                        let mut config = AppConfig::load();
                        let _ = config.forget_library(&entry.slug);
                        let _ = std::fs::remove_dir_all(entry.dir());
                        let _ = handle.update(cx, |_, window, cx| {
                            window.push_notification(
                                Notification::warning(
                                    rust_i18n::t!("app.import_repository_failed").to_string(),
                                )
                                .id1::<RepositoryNotice>("repository-package"),
                                cx,
                            );
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// Help ▸ About Trove.
    fn show_about(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title(rust_i18n::t!("app.about").to_string())
                .width(px(360.))
                .close_button(false)
                .child(crate::dialogs::with_close_x(
                    "about-close-x",
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .text_sm()
                        .text_color(rgb(0x8a8a8a))
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(0x1f1f1f))
                                .child("Trove"),
                        )
                        .child(rust_i18n::t!("app.about_body").to_string())
                        .child(
                            rust_i18n::t!("app.version", version = env!("CARGO_PKG_VERSION"))
                                .to_string(),
                        )
                        // A pending release, offered where new versions are
                        // usually looked for. Nothing appears here when the
                        // build is current or the last check failed.
                        .when_some(pending_update(), |column, (version, page)| {
                            column.child(
                                div()
                                    .id("about-update")
                                    .cursor_pointer()
                                    .text_color(rgb(0x185fa5))
                                    .hover(|style| style.underline())
                                    .child(
                                        rust_i18n::t!("app.update_available", version = version)
                                            .to_string(),
                                    )
                                    .on_click(move |_, _, _| {
                                        let _ =
                                            trove_core::services::open_external::open_url(&page);
                                    }),
                            )
                        }),
                ))
        });
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No `cx.on_action` here: `Context::on_action` lands on
        // `Window::on_action`, which is only legal while painting, and a
        // view's `Render::render` runs in the layout/prepaint phase — the
        // debug assertion fires on the first frame. The exit is owned by the
        // stage element below, which registers its listener the legal way
        // (an element's `.on_action` is applied during paint) and holds the
        // window focus the whole time the stage is up, so it always sits on
        // the dispatch path.
        let controller = self.controller.clone();

        // Dialog / sheet / notification layers are no longer children of the
        // application view: since gpui-kit 0.7 the component layer mounts them
        // through the `WindowState` root plugin (registered by `gpui_kit::init`),
        // which renders its own overlay above the root surface.

        // The stage draws the preview's own surface, so it needs that surface
        // to exist. If the preview went away while the flag was still set —
        // dismissed, or replaced — give the window back here: rendering the
        // shell in a fullscreen window with `stage_fullscreen` stuck true
        // would swallow every later `Enter` (see the guard in
        // `enter_stage_fullscreen`) and leave no visible way out.
        let stage = self.workspace.read(cx).preview_stage(cx);
        if self.stage_fullscreen && stage.is_none() {
            // Nothing left to fade: the exit beat is for a stage that is
            // still showing something, so the teardown lands right here
            // rather than after [`STAGE_EXIT_TIME`] of an empty window.
            self.finish_leave_stage_fullscreen(window, cx);
        }

        // Fullscreen preview: this very window becomes the stage, holding
        // the surface the preview was already showing — no title bar, no
        // dock, no status bar, and nothing is handed over, so neither the
        // picture nor the sound notices the stage appearing or going away.
        // The video keeps its own exit button in the transport; every other
        // kind gets the floating one in the corner, the one visible way out
        // a stage without controls would otherwise lack.
        if self.stage_fullscreen
            && let Some(stage) = stage
        {
            let on_video = self.workspace.read(cx).preview_player(cx).is_some();
            // Claim the focus the first frame the stage is up: the handle is
            // only findable once this node has been rendered, and the
            // `focus` in `enter_stage_fullscreen` may have landed before the
            // stage existed.
            if !self.stage_focus.is_focused(window) {
                window.focus(&self.stage_focus, cx);
            }
            // The surface the stage hosts: the preview itself plus, for the
            // kinds without controls of their own, the corner exit. One
            // wrapper for both, so the arrival and the exit move and fade
            // them together — the button is part of the stage's arrival, not
            // a decal that was already there when the picture landed. The
            // button hides and reveals like the players' bars do, through the
            // same fade; hovering it pins it up for as long as the pointer
            // rests there.
            let exit_fade =
                chrome::presence(self.stage_chrome.shown(), "stage-exit-button", window, cx);
            let surface = div().relative().size_full().child(stage).when(
                !on_video && exit_fade.should_render(),
                |surface| {
                    surface.child(
                        div()
                            .id("stage-exit-button")
                            .absolute()
                            .top_2()
                            .right_2()
                            .on_hover(cx.listener(|this, hovered: &bool, _, _cx| {
                                this.stage_chrome.pin(*hovered);
                            }))
                            .opacity(exit_fade.progress)
                            .child(
                                crate::components::controls::icon_button(
                                    "stage-exit-fullscreen",
                                    gpui_kit::assets::IconName::Shrink,
                                    rust_i18n::t!("video.exit_fullscreen").to_string(),
                                )
                                .on_click(cx.listener(
                                    |_, _, window, cx| {
                                        window.dispatch_action(Box::new(ExitVideoFullscreen), cx);
                                    },
                                )),
                            ),
                    )
                },
            );
            // Arrive and leave through one gesture — fade while settling the
            // last few percent of size and a few pixels of rise, reversed on
            // the way home. The size is laid out, not transformed: gpui has
            // no transform for divs, and the stage's content re-fits into
            // the wrapper every frame anyway (see [`STAGE_SETTLE_SCALE`]).
            // Each run gets its own id (the generation), so a re-entered
            // stage replays from its own zero; reduced motion shows the
            // stage as it is.
            let animated = if cx.reduce_motion() {
                surface.into_any_element()
            } else if self.stage_closing {
                let id = ElementId::Name(format!("stage-exit-{}", self.stage_generation).into());
                surface
                    .with_animation(
                        id,
                        Animation::new(STAGE_EXIT_TIME),
                        move |surface, delta| {
                            let eased = ease_in_cubic(delta);
                            let scale = 1.0 - (1.0 - STAGE_SETTLE_SCALE) * eased;
                            surface
                                .opacity(1.0 - eased)
                                .w(relative(scale))
                                .h(relative(scale))
                                .top(px(STAGE_SETTLE_RISE * eased))
                        },
                    )
                    .into_any_element()
            } else {
                let id = ElementId::Name(format!("stage-enter-{}", self.stage_generation).into());
                surface
                    .with_animation(
                        id,
                        Animation::new(STAGE_ENTER_TIME),
                        move |surface, delta| {
                            let eased = ease_out_cubic(delta);
                            let scale = STAGE_SETTLE_SCALE + (1.0 - STAGE_SETTLE_SCALE) * eased;
                            surface
                                .opacity(eased)
                                .w(relative(scale))
                                .h(relative(scale))
                                .top(px((1.0 - eased) * STAGE_SETTLE_RISE))
                        },
                    )
                    .into_any_element()
            };
            return div()
                .id("preview-stage")
                .relative()
                .size_full()
                .bg(black())
                // The animated surface sits centred so its entrance can grow
                // from a shade under full size without drifting from the
                // middle; at rest it fills this node exactly as the bare
                // stage always did.
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .track_focus(&self.stage_focus)
                .key_context(crate::app::keybindings::VIDEO_FULLSCREEN_CONTEXT)
                .on_action(cx.listener(|this, _: &ExitVideoFullscreen, window, cx| {
                    this.leave_stage_fullscreen(window, cx);
                }))
                .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| {
                    // Any movement over the stage is the gesture that reveals
                    // the corner exit; only the reveal itself needs a paint.
                    if this.stage_chrome.moved_anywhere() {
                        cx.notify();
                    }
                }))
                .child(animated)
                .into_any_element();
        }

        let shell = div()
            .id("app-root")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            // Whole-window file drop surface.
            .on_drop::<ExternalPaths>(move |paths, window, cx| {
                jobs::import_paths_app(&controller, paths.0.iter().cloned().collect(), window, cx);
            })
            // Menu-bar actions: handled here so they work wherever the focus
            // is (the menu bar itself never holds the grid's focus).
            .on_action(cx.listener(|this, _: &ImportFiles, window, cx| {
                this.prompt_import(window, cx);
            }))
            // Plugin commands: one generic action carries every plugin's
            // commands; the payload routes to the plugin that declared it.
            .on_action(cx.listener(|_, action: &RunPluginCommand, window, cx| {
                crate::plugins::run_command(&action.command, window, cx);
            }))
            .on_action(cx.listener(|_: &mut Self, _: &ManageLibraries, _, cx| {
                crate::app::library_manager::open(cx);
            }))
            .on_action(cx.listener(|this, _: &ExportRepository, window, cx| {
                this.prompt_export_repository(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ImportRepository, window, cx| {
                this.prompt_import_repository(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                crate::dialogs::settings::open(cx, this.controller.clone());
            }))
            .on_action(cx.listener(|this, _: &FindDuplicates, window, cx| {
                crate::dialogs::duplicates::DuplicateDialog::open(
                    window,
                    cx,
                    this.controller.clone(),
                );
            }))
            .on_action(cx.listener(|this, _: &ShowAllAssets, _, cx| {
                this.controller
                    .update(cx, |ctl, _cx| ctl.select_collection(None));
            }))
            .on_action(cx.listener(|this, _: &ShowTrash, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.select_trash());
            }))
            .on_action(cx.listener(|this, _: &RefreshLibrary, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &PasteImport, window, cx| {
                this.paste_import(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyImage, window, cx| {
                this.copy_primary_image(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ImportUrl, window, cx| {
                this.prompt_import_url(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ImportFromApp, window, cx| {
                crate::dialogs::migrate::MigrateDialog::open(
                    window,
                    cx,
                    Some(this.controller.clone()),
                );
            }))
            .on_action(cx.listener(|this, _: &EnterVideoFullscreen, window, cx| {
                this.enter_stage_fullscreen(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExitVideoFullscreen, window, cx| {
                this.leave_stage_fullscreen(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Screenshot, window, cx| {
                this.take_screenshot(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                // The window root handles it, so Ctrl+K works from the tags
                // panel or a settings dialog just as it does from the grid.
                this.workspace
                    .update(cx, |ws, cx| ws.summon_search(window, cx));
            }))
            .on_action(cx.listener(|this, _: &BatchRename, window, cx| {
                crate::dialogs::rename::RenameDialog::open(window, cx, this.controller.clone());
            }))
            .on_action(cx.listener(|this, _: &BatchConvert, window, cx| {
                crate::dialogs::convert::ConvertDialog::open(window, cx, this.controller.clone());
            }))
            .on_action(cx.listener(|this, _: &BatchEdit, window, cx| {
                crate::dialogs::edit::EditDialog::open(window, cx, this.controller.clone());
            }))
            // Acts on the selection, like the other batch actions: hitting it
            // with nothing selected is a mistake worth naming rather than a
            // reason to spend money on the whole library.
            .on_action(cx.listener(|this, _: &AutoTag, window, cx| {
                crate::library::jobs::start_analysis_app(
                    &this.controller,
                    crate::library::jobs::AnalysisTarget::Selection,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ExportXmp, window, cx| {
                crate::library::jobs::export_xmp_app(&this.controller, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.controller
                    .update(cx, |ctl, _cx| ctl.select_all_visible());
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.clear_selection());
            }))
            .on_action(cx.listener(|this, _: &TrashSelected, window, cx| {
                // Same action, same gate as the toolbar button: the key is the
                // faster way to lose a library, and in the trash view it deletes
                // for good rather than moving anything.
                crate::panels::workspace::trash_or_purge_gated(&this.controller, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    if let Err(e) = ctl.library.undo() {
                        ctl.notice = Some(
                            rust_i18n::t!("app.undo_failed", error = e.to_string()).to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    if let Err(e) = ctl.library.redo() {
                        ctl.notice = Some(
                            rust_i18n::t!("app.redo_failed", error = e.to_string()).to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &About, window, cx| {
                this.show_about(window, cx);
            }))
            .on_action(cx.listener(|_, _: &CheckUpdates, _, cx| {
                run_update_check(cx);
            }))
            // The app-menu furniture. On macOS these arrive from the app and
            // Window menus (and the platform chords cmd-q / cmd-h / cmd-m);
            // elsewhere nothing dispatches them until a menu or a future
            // binding does, so the handlers are unconditional.
            .on_action(cx.listener(|_, _: &Quit, _, cx| {
                cx.quit();
            }))
            .on_action(cx.listener(|_, _: &HideApp, _, cx| {
                cx.hide();
            }))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| {
                window.minimize_window();
            }))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _| {
                window.zoom_window();
            }))
            .child(self.title_bar.clone())
            .child(div().flex_1().min_h_0().child(self.dock.clone()))
            .child(status_bar::status_bar(
                &self.controller,
                self.viewport_backend.clone(),
                cx,
            ));

        // Every image resolves through one cache, so a thumbnail file
        // rewritten in place can be re-read: gpui keys cached decodes by
        // source path, and without this scope a rebuilt thumbnail would keep
        // serving the old pixels until a restart. The wrapper styles itself
        // full-size — it sits where the root shell used to, and a default
        // style would shrink the whole app to its content height, collapsing
        // the dock (flex_1 of nothing) to zero.
        gpui::image_cache(self.images.clone())
            .size_full()
            .child(shell)
            .into_any_element()
    }
}

/// Boot the local collect service (127.0.0.1). The server thread only
/// writes files into the inbox dir; the backend watch task imports them.
fn start_collect_server(_cx: &mut Context<AppView>) {
    let config = AppConfig::load();
    if !config.collect_enabled() {
        return;
    }
    match trove_core::services::collect::spawn_server(config.collect_port()) {
        Some(port) => {
            let _ = port;
        }
        None => {
            // Port taken (another instance?) — surfacing it would block
            // startup on a non-fatal condition; the next watcher cycle
            // still drains any inbox files the other instance wrote.
        }
    }
}
