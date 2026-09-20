//! The application's root view.
//!
//! This is the window content the app shell assembles: it owns the
//! [`LibraryController`], composes the dock layout and the title bar, and hosts
//! a whole-window file-drop surface. `main` only boots the window and mounts
//! this view inside a `Root`.

use std::path::PathBuf;

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
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

use crate::app::actions::*;
use crate::app::title_bar::TitleBarView;
use crate::app::tray;
use crate::library::jobs;
use crate::library::{ImportPhase, LibraryController, SelectionSource};
use crate::panels::{ExplorerPanel, FoldersPanel, InspectorPanel, TagsPanel, WorkspacePanel};
use trove_core::config::AppConfig;
use trove_core::library::Library;
use trove_core::paths;
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

/// The release worth telling the user about, or `None`.
///
/// The status bar asks on every frame, so the config file is only read when a
/// newer release is actually on the table — and a version the user already
/// waved off is filtered out here rather than inside the update service,
/// which knows nothing about preferences.
fn pending_update() -> Option<(String, String)> {
    let (version, page) = update::available()?;
    let skipped = AppConfig::load().skipped_version().map(str::to_string);
    if skipped.as_deref() == Some(version.as_str()) {
        return None;
    }
    Some((version, page))
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
            let _ = config.record_update_check(update::now_unix());
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

/// The running session's controller, registered by [`AppView::new`]. The
/// library manager's switch reads it to hot-swap the open library in
/// place instead of opening a second main window.
#[derive(Default)]
pub(crate) struct SessionState(pub(crate) Option<gpui::WeakEntity<LibraryController>>);

impl gpui_kit::Global for SessionState {}

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
    /// The workspace panel: reached for its preview's player when the
    /// fullscreen stage is on.
    workspace: Entity<WorkspacePanel>,
    /// Whether this window is currently the fullscreen video stage — the
    /// window itself goes fullscreen and renders the preview's own player,
    /// so no second window, player or soundtrack is ever built.
    video_fullscreen: bool,
    /// Focus handle for that stage. It has to hold the window's focus while
    /// it is up: actions and key bindings are dispatched from the focused
    /// element upwards, and the element that had the focus when the stage
    /// came up is no longer in the frame — without this the dispatch falls
    /// back to the window root, which never sees the stage's context, and
    /// neither the exit button nor Esc can leave the stage.
    video_stage_focus: FocusHandle,
    /// Watches every keystroke while the stage is up. Esc leaves through the
    /// same exit action as the button; the key binding alone cannot be
    /// relied on because it is matched against the focused element's
    /// context, and a focus that slipped off the stage would leave Esc
    /// dead. Watching globally removes that dependency. Dropped on leave,
    /// which unregisters it.
    video_escape: Option<Subscription>,
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
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Follow the OS light/dark switch while it runs (the startup apply
        // happened before this window existed).
        let _appearance = window.observe_window_appearance(|window, cx| {
            crate::app::theme::apply_from_settings(Some(window), cx);
        });
        let library = open_library_at_startup();
        let controller = cx.new(|_cx| LibraryController::new(library));
        // This window is now the running session: the library manager's
        // library switch swaps its library through this handle.
        cx.set_global(crate::app::root::SessionState(Some(controller.downgrade())));
        let title_bar = cx.new(|cx| TitleBarView::new(controller.clone(), cx));

        let explorer = cx.new(|cx| ExplorerPanel::new(window, cx, controller.clone()));
        let folders = cx.new(|cx| FoldersPanel::new(cx, controller.clone()));
        let workspace = cx.new(|cx| WorkspacePanel::new(window, cx, controller.clone()));
        let tags = cx.new(|cx| TagsPanel::new(cx, controller.clone()));
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
            let ctl = controller.read(cx);
            let plain = ctl.selection_source == SelectionSource::Plain;
            let selection: Vec<Uuid> = (*ctl.selected_assets).clone();
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
        Self {
            controller,
            viewport_backend: initial_backend,
            dock,
            inspector,
            last_selection: Vec::new(),
            title_bar,
            workspace,
            video_fullscreen: false,
            video_stage_focus: cx.focus_handle(),
            video_escape: None,
            tray,
            quitting,
            _appearance,
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
        open_capture_picker(controller, dest, handle, cx);

        // No in-process overlay to draw on these platforms: `screencapture -i`
        // is the region picker, and the chain reports back what it wrote.
        #[cfg(not(target_os = "linux"))]
        run_capture_chain(
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
    /// preview's own player, which simply keeps playing. Leaving gives the
    /// window back — the player never notices either way.
    fn enter_video_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Already the stage: answering twice would toggle the window back
        // out of fullscreen under the user.
        if self.video_fullscreen {
            return;
        }
        let Some(player) = self.workspace.read(cx).preview_player(cx) else {
            return;
        };
        player.update(cx, |player, cx| player.set_fullscreen_mode(true, cx));
        self.video_fullscreen = true;
        Self::set_window_fullscreen(window, true);
        // Take the focus now: leaving it on whatever had it means the
        // dispatch tree falls back to the window root as soon as that
        // element leaves the frame, which strands the stage (see
        // [`AppView::video_stage_focus`]).
        window.focus(&self.video_stage_focus, cx);
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

    /// Give the window back to the shell.
    fn leave_video_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The mirror of the guard above: an Exit arriving while the stage is
        // down must not throw the window into fullscreen.
        if !self.video_fullscreen {
            return;
        }
        // Stop watching keystrokes first: dropping the subscription
        // unregisters it, so the Esc that leaves cannot be seen twice.
        self.video_escape = None;
        if let Some(player) = self.workspace.read(cx).preview_player(cx) {
            player.update(cx, |player, cx| player.set_fullscreen_mode(false, cx));
        }
        self.video_fullscreen = false;
        Self::set_window_fullscreen(window, false);
        // Hand the focus back to the workspace: the stage's handle leaves
        // the frame with it, and a focus pointing at nothing would strand
        // the next stage the same way.
        let workspace_focus = self.workspace.read(cx).focus_handle(cx);
        window.focus(&workspace_focus, cx);
        cx.notify();
    }

    /// File ▸ Export library… : save-dialog, then write the metadata catalog
    /// as pretty JSON (`Library::export_metadata`). Library is not `Send`, so
    /// serialization happens on the main thread inside the window callback.
    fn prompt_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let dir = ctl.read(cx).library.root().to_path_buf();
        let rx = cx.prompt_for_new_path(&dir, Some("trove-export.json"));
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = handle.update(cx, |_, window, cx| {
                    let outcome = ctl
                        .update(cx, |ctl, _| ctl.library.export_metadata())
                        .and_then(|json| std::fs::write(&path, json).map_err(|e| e.into()));
                    let note = match outcome {
                        Ok(()) => Notification::success(
                            rust_i18n::t!("app.export_done", path = path.display().to_string())
                                .to_string(),
                        ),
                        Err(e) => Notification::warning(
                            rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                        ),
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    /// File ▸ Export backup archive… : save-dialog, then write the full
    /// backup — the software configuration plus every library's data — as one
    /// zip. The archive build runs on the background executor: a media store
    /// can be gigabytes, and none of it needs the main thread.
    fn prompt_export_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let suggested = trove_core::services::archive::backup_file_name();
        let rx = cx.prompt_for_new_path(&paths::data_dir(), Some(suggested.as_str()));
        let handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let outcome = cx
                    .background_executor()
                    .spawn(async move { trove_core::services::archive::create_full_backup(&path) })
                    .await;
                let _ = handle.update(cx, |_, window, cx| {
                    let note = match outcome {
                        Ok(report) => Notification::success(
                            rust_i18n::t!(
                                "app.backup_done",
                                path = report.path.display().to_string()
                            )
                            .to_string(),
                        ),
                        Err(e) => Notification::warning(
                            rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                        ),
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    /// Bottom status bar: selection count, library path, import state and the
    /// latest notice (errors surface here even outside Settings).
    fn status_bar(&self, cx: &Context<Self>) -> Div {
        let ctl = self.controller.read(cx);
        let selected = ctl.selected_assets.len();
        let root = ctl.library.root().display().to_string();
        let import = match &ctl.import_phase {
            ImportPhase::Idle => rust_i18n::t!("statusbar.import_idle").to_string(),
            ImportPhase::Running { total, done } => {
                // Zero is the job's "counting the folder" state: there is no
                // fraction to show until the walk on the backend thread ends.
                if *total == 0 {
                    rust_i18n::t!("statusbar.import_scanning").to_string()
                } else {
                    rust_i18n::t!("statusbar.import_running", done = done, total = total)
                        .to_string()
                }
            }
            ImportPhase::Done { imported, skipped } => rust_i18n::t!(
                "statusbar.import_done",
                imported = imported,
                skipped = skipped
            )
            .to_string(),
        };
        let notice = ctl.notice.clone();
        let (undo_len, redo_len) = (ctl.library.undo_len(), ctl.library.redo_len());
        // Recent-operation descriptions for the status-bar history tooltip.
        let undo_entries = ctl.library.undo_entries(5);
        let redo_entries = ctl.library.redo_entries(3);
        let history_tooltip = if undo_len == 0 && redo_len == 0 {
            None
        } else {
            let mut lines: Vec<String> = undo_entries.iter().map(describe_op).collect();
            if !redo_entries.is_empty() {
                lines.push(rust_i18n::t!("statusbar.redo_header").to_string());
                lines.extend(redo_entries.iter().map(describe_op));
            }
            Some(lines.join("\n"))
        };
        h_flex()
            .h(px(26.))
            .px_3()
            .items_center()
            .gap_4()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(rust_i18n::t!("statusbar.selected", count = selected).to_string())
            .when(undo_len > 0 || redo_len > 0, {
                let history_tooltip = history_tooltip.clone();
                move |bar| {
                    let history_seg = div()
                        .id("statusbar-history")
                        .when(undo_len > 0, |seg| {
                            seg.child(rust_i18n::t!("statusbar.undo", count = undo_len).to_string())
                        })
                        .when(undo_len > 0 && redo_len > 0, |seg| seg.child(" · "))
                        .when(redo_len > 0, |seg| {
                            seg.child(rust_i18n::t!("statusbar.redo", count = redo_len).to_string())
                        });
                    let seg = match history_tooltip {
                        Some(text) => {
                            let text = SharedString::from(text);
                            history_seg.tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(text.clone())
                                    .build(window, cx)
                            })
                        }
                        None => history_seg,
                    };
                    bar.child(seg)
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(rust_i18n::t!("statusbar.library", path = root).to_string()),
            )
            // Which renderer is drawing an open 3D model: the GPU adapter, a
            // CPU fallback reason, or the progress of a load that is still
            // running. The model viewport used to carry this in the panel's
            // title bar; the status bar is where machine-level facts belong,
            // and the vacated title-bar room is where the preview's next
            // tools go.
            .when_some(self.viewport_backend.clone(), |bar, backend| {
                bar.child(div().max_w(px(360.)).truncate().child(backend))
            })
            .child(import)
            // The release badge: the only place a pending update is announced
            // without the user asking. Clicking it opens the release page —
            // installing is the user's call, not ours.
            .when_some(pending_update(), |bar, (version, page)| {
                bar.child(
                    div()
                        .id("statusbar-update")
                        .cursor_pointer()
                        .text_color(cx.theme().info)
                        .hover(|style| style.underline())
                        .child(
                            rust_i18n::t!("statusbar.update_available", version = version)
                                .to_string(),
                        )
                        .tooltip({
                            let hint = rust_i18n::t!("statusbar.update_hint").to_string();
                            move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(hint.clone())
                                    .build(window, cx)
                            }
                        })
                        .on_click(move |_, _, _| {
                            let _ = trove_core::services::open_external::open_url(&page);
                        }),
                )
            })
            .when_some(notice, |bar, notice| {
                bar.child(
                    div()
                        .max_w(px(420.))
                        .truncate()
                        .text_color(cx.theme().warning)
                        .child(notice),
                )
            })
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

    /// File ▸ Import library… : pick a Trove export JSON and restore its
    /// metadata into the open library (content matches link, the rest
    /// become placeholders that self-heal on re-import).
    fn prompt_import_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                rust_i18n::t!("app.import_library_prompt")
                    .into_owned()
                    .into(),
            ),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.first()
            {
                let note = match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let result = handle
                            .update(cx, |_, _, cx| ctl.read(cx).library.import_metadata(&text));
                        match result {
                            Ok(Ok(report)) => {
                                ctl.update(cx, |ctl, cx| {
                                    ctl.generation += 1;
                                    cx.notify();
                                });
                                Notification::success(
                                    rust_i18n::t!(
                                        "app.import_library_done",
                                        assets = report.assets_linked + report.assets_placeholder,
                                        collections = report.collections,
                                        tags = report.tags,
                                        smart = report.smart_collections,
                                        skipped = report.skipped
                                    )
                                    .to_string(),
                                )
                            }
                            _ => Notification::warning(
                                rust_i18n::t!("app.import_library_failed").to_string(),
                            ),
                        }
                    }
                    Err(_) => Notification::warning(
                        rust_i18n::t!("app.import_library_failed").to_string(),
                    ),
                };
                let _ = handle.update(cx, |_view, window, cx| {
                    window.push_notification(note, cx);
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
                                        jobs::InboxDrain::Refused => {
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

    /// File ▸ Export media package… : pick a destination directory, then
    /// write a portable package (trove-export.json + media/ blobs).
    fn prompt_export_media_package(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                rust_i18n::t!("app.export_media_package_prompt")
                    .into_owned()
                    .into(),
            ),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(dir) = paths.first()
            {
                let result = handle.update(cx, |_, _, cx| {
                    ctl.read(cx)
                        .library
                        .export_media_package(&dir.to_path_buf())
                });
                let note = match result {
                    Ok(Ok(report)) => {
                        ctl.update(cx, |ctl, cx| {
                            ctl.generation += 1;
                            cx.notify();
                        });
                        Notification::success(
                            rust_i18n::t!(
                                "app.export_media_package_done",
                                files = report.files,
                                path = report.path.display().to_string()
                            )
                            .to_string(),
                        )
                    }
                    Ok(Err(e)) => Notification::warning(
                        rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                    ),
                    Err(e) => Notification::warning(
                        rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                    ),
                };
                let _ = handle.update(cx, |_view, window, cx| {
                    window.push_notification(note, cx);
                });
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

        // The dialog/sheet/notification layers are rendered by the app root,
        // not by `Root::render` itself: without these children, dialogs opened
        // via `window.open_dialog` exist in state but never draw.
        let dialog_layer = gpui_kit::component::Root::render_dialog_layer(window, cx);
        let sheet_layer = gpui_kit::component::Root::render_sheet_layer(window, cx);
        let notification_layer = gpui_kit::component::Root::render_notification_layer(window, cx);

        // The stage draws the preview's own player, so it needs that player
        // to exist. If the preview went away while the flag was still set —
        // dismissed, or replaced — give the window back here: rendering the
        // shell in a fullscreen window with `video_fullscreen` stuck true
        // would swallow every later `Enter` (see the guard in
        // `enter_video_fullscreen`) and leave no visible way out.
        if self.video_fullscreen && self.workspace.read(cx).preview_player(cx).is_none() {
            self.leave_video_fullscreen(window, cx);
        }

        // Fullscreen video: this very window becomes the stage, holding the
        // player the preview was already showing — no title bar, no dock, no
        // status bar, and nothing is handed over, so neither the picture nor
        // the sound notices the stage appearing or going away.
        if self.video_fullscreen
            && let Some(player) = self.workspace.read(cx).preview_player(cx)
        {
            // Claim the focus the first frame the stage is up: the handle is
            // only findable once this node has been rendered, and the
            // `focus` in `enter_video_fullscreen` may have landed before the
            // stage existed.
            if !self.video_stage_focus.is_focused(window) {
                window.focus(&self.video_stage_focus, cx);
            }
            return div()
                .id("video-stage")
                .relative()
                .size_full()
                .bg(black())
                .track_focus(&self.video_stage_focus)
                .key_context(crate::VIDEO_FULLSCREEN_CONTEXT)
                .on_action(cx.listener(|this, _: &ExitVideoFullscreen, window, cx| {
                    this.leave_video_fullscreen(window, cx);
                }))
                .child(player)
                .children(dialog_layer)
                .children(sheet_layer)
                .children(notification_layer)
                .into_any_element();
        }

        div()
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
            .on_action(cx.listener(|this, _: &ExportLibrary, window, cx| {
                this.prompt_export(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExportBackup, window, cx| {
                this.prompt_export_backup(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ImportLibrary, window, cx| {
                this.prompt_import_library(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExportMediaPackage, window, cx| {
                this.prompt_export_media_package(window, cx);
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
            .on_action(cx.listener(|this, _: &EnterVideoFullscreen, window, cx| {
                this.enter_video_fullscreen(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExitVideoFullscreen, window, cx| {
                this.leave_video_fullscreen(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Screenshot, window, cx| {
                this.take_screenshot(window, cx);
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
            .on_action(cx.listener(|this, _: &TrashSelected, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    ctl.trash_or_purge_selection();
                    cx.notify();
                });
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
            .child(self.title_bar.clone())
            .child(div().flex_1().min_h_0().child(self.dock.clone()))
            .child(self.status_bar(cx))
            .children(dialog_layer)
            .children(sheet_layer)
            .children(notification_layer)
            .into_any_element()
    }
}

/// One history line for the status-bar tooltip: localized verb plus the
/// recorded target (name when a single object was touched, a count
/// otherwise).
fn describe_op(desc: &trove_core::history::undo::OpDesc) -> String {
    let action = rust_i18n::t!(desc.action.key()).to_string();
    match (&desc.target, desc.count) {
        (Some(name), _) => format!("{action} {name}"),
        (None, n) if n > 1 => format!("{action} ×{n}"),
        (None, _) => action,
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

/// Run a capture on the background executor and import what it wrote.
///
/// `fallback` is the second chance when the first target fails — the picker
/// uses it for the rectangle it highlighted, so a window that vanished
/// between the click and the capture still hands over those pixels.
///
/// A cancelled capture (the user answered the platform's own picker with
/// Esc) is not an error: nothing was written, so nothing is imported and
/// nothing is reported.
fn run_capture_chain(
    target: trove_core::services::screenshot::CaptureTarget,
    fallback: Option<trove_core::services::screenshot::CaptureTarget>,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    handle: AnyWindowHandle,
    cx: &mut App,
) {
    use trove_core::services::screenshot::{self, Error};

    let requested = format!("{target:?}");
    cx.spawn(async move |cx| {
        let path = dest.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move {
                match screenshot::capture(&target, &path) {
                    Ok(source) => Ok(source),
                    Err(Error::Cancelled) => Err(Error::Cancelled),
                    Err(first) => match fallback {
                        Some(fallback) => {
                            tracing::warn!(
                                error = %first.message(),
                                "screenshot target failed; trying the highlighted rectangle"
                            );
                            screenshot::capture(&fallback, &path).map_err(|second| {
                                // Both reasons, so the log names what actually
                                // refused rather than just the first attempt.
                                Error::Failed(format!(
                                    "{}; then the highlighted rectangle: {}",
                                    first.message(),
                                    second.message()
                                ))
                            })
                        }
                        None => Err(first),
                    },
                }
            })
            .await;
        let _ = handle.update(cx, |_, window, cx| match outcome {
            Ok(source) => {
                tracing::info!(
                    source = ?source,
                    dest = %dest.display(),
                    "screenshot captured; importing"
                );
                jobs::import_paths_app(&controller, vec![dest], window, cx);
            }
            Err(Error::Cancelled) => {
                tracing::info!(target = %requested, "screenshot cancelled");
            }
            Err(error) => {
                tracing::error!(error = %error.message(), "screenshot capture failed");
                window.push_notification(
                    Notification::warning(
                        rust_i18n::t!("notice.screenshot_failed", error = error.message())
                            .to_string(),
                    ),
                    cx,
                );
            }
        });
    })
    .detach();
}

/// Open the screenshot picker over a frozen frame.
///
/// Both halves of the preparation are D-Bus round trips — the frame, then
/// the compositor's window list (bounded by its own timeout) — so they run
/// on the background executor, never on the UI thread. When even the frame
/// cannot be had, the platform's own picker takes over: on Linux that is
/// `grim -g "$(slurp)"` or `scrot -s`, a working region capture without the
/// window snapping.
#[cfg(target_os = "linux")]
fn open_capture_picker(
    controller: Entity<LibraryController>,
    dest: PathBuf,
    handle: AnyWindowHandle,
    cx: &mut App,
) {
    use trove_core::services::screenshot::CaptureTarget;

    let fallback = {
        let (dest, controller) = (dest.clone(), controller.clone());
        move |cx: &mut App| {
            run_capture_chain(CaptureTarget::PickArea, None, dest, controller, handle, cx);
        }
    };
    cx.spawn(async move |cx| {
        let prepared = cx
            .background_executor()
            .spawn(async { prepare_pick() })
            .await;
        match prepared {
            Ok((frame, candidates)) => {
                let _ = handle.update(cx, |_, _, cx| {
                    crate::components::capture_pick::open(
                        frame, candidates, dest, controller, handle, cx,
                    );
                });
            }
            Err(reason) => {
                tracing::warn!(reason, "no in-process picker; using the platform picker");
                let _ = handle.update(cx, |_, _, cx| fallback(cx));
            }
        }
    })
    .detach();
}

/// The frame the picker freezes on, plus the windows it can snap to.
///
/// The window list is a bonus, never a requirement: without it (no scripting
/// interface, a compositor that did not answer in time) the picker still drags
/// rectangles, it just never highlights a window. On a session without KWin
/// there is not even a frame to freeze — the compositor's own picker takes
/// over, which is the same region capture with a plainer interface.
#[cfg(target_os = "linux")]
fn prepare_pick() -> Result<
    (
        image::RgbaImage,
        Vec<crate::components::capture_pick::Candidate>,
    ),
    String,
> {
    use crate::components::capture_pick::{Candidate, window_label};
    use trove_core::services::{kwin, kwin_script};

    if !kwin::available() {
        return Err("this session has no compositor interface for a frozen frame".into());
    }
    let frame = kwin::capture_workspace_image().map_err(|failure| failure.labelled())?;
    let candidates = kwin_script::window_list()
        .unwrap_or_default()
        .into_iter()
        .map(|window| Candidate {
            handle: window.handle,
            label: window_label(&window.app, &window.caption),
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        })
        .collect();
    Ok((frame, candidates))
}

/// A window the picker highlighted: the compositor renders that window
/// itself (decoration included, native resolution, nothing else in frame),
/// and the rectangle the user clicked is the fallback if the window is gone
/// by the time we ask for it.
#[cfg(target_os = "linux")]
pub(crate) fn capture_picked_window(
    picked: crate::components::capture_pick::Candidate,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    use trove_core::services::screenshot::CaptureTarget;

    let target = CaptureTarget::Window {
        handle: picked.handle,
    };
    let fallback = CaptureTarget::Area {
        x: picked.x,
        y: picked.y,
        width: picked.width,
        height: picked.height,
    };
    run_capture_chain(
        target,
        Some(fallback),
        dest,
        controller,
        window.window_handle(),
        cx,
    );
}
