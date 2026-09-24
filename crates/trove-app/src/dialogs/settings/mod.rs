//! Settings window: sidebar pages built on gpui-kit's `Settings` widget
//! (the same scaffold OpenLogi uses for its settings window — page
//! navigation, search, groups and data-bound fields all come from the
//! component).
//!
//! Unlike the rest of `dialogs/`, this is a standalone OS window with
//! open-or-focus semantics: opening it again focuses the existing window
//! instead of stacking a second one. [`SettingsPage`] deep-links to a page
//! on a *fresh* open (`open_at`); an already-open window is just focused —
//! the `Settings` widget owns the page selection from then on.
//!
//! The window carries the [`LibraryController`]: the library-location row
//! hot-swaps the open library (no restart), and the maintenance page runs
//! the `trove-core` maintenance jobs — thumbnail rebuild on the background
//! executor, index rebuild and orphan sweep inline (they are database-bound
//! and quick). Outcomes land on [`LibraryController::notice`], a status
//! line both the general and the maintenance page render.
//!
//! Pages are rebuilt on every render, so titles follow the active locale
//! after a live language switch. The expensive numbers they show (library
//! statistics, fingerprint coverage) are *snapshots*: queried when the
//! window opens and whenever a background job finishes or the library is
//! hot-swapped (observed via `busy` / library-root transitions), never per
//! render.

mod about;
mod ai;
mod appearance;
mod files;
mod model;
mod plugins;
mod search;
mod shortcuts;

pub(super) use std::path::PathBuf;

pub(super) use gpui_kit::base::{h_flex, v_flex};
pub(super) use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::group_box::GroupBoxVariant;
pub(super) use gpui_kit::component::scroll::ScrollableElement as _;
pub(super) use gpui_kit::component::setting::{
    SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
pub(super) use gpui_kit::component::{ActiveTheme, Disableable as _, IconName, Sizable, ThemeMode};
use gpui_kit::component::{Root, TitleBar};
pub(super) use gpui_kit::prelude::FluentBuilder as _;
pub(super) use gpui_kit::*;

pub(super) use crate::app::i18n::SUPPORTED;
pub(super) use crate::library::LibraryController;
pub(super) use trove_core::config::{AppConfig, Appearance};
pub(super) use trove_core::keybindings;
pub(super) use trove_core::store::stats::LibraryStats;

/// A boolean setting that reads and writes the on-disk config.
///
/// Shared by every page with a switch, because the write side is the part that
/// is easy to get half-done: saving without `refresh_windows` leaves an open
/// preview showing the old value until it is reopened.
pub(super) fn config_switch(
    read: fn(&AppConfig) -> bool,
    write: impl Fn(&mut AppConfig, bool) + 'static,
) -> SettingField<bool> {
    SettingField::switch(
        move |_cx| read(&AppConfig::load()),
        move |value, cx| {
            let mut config = AppConfig::load();
            write(&mut config, value);
            let _ = config.save();
            cx.refresh_windows();
        },
    )
}

/// The AI vendors Trove can talk to, as `(stored id, display name)` pairs in
/// the shape [`SettingField::dropdown`] takes. Shared by every settings page
/// that configures an endpoint (ai, search) so the option lists cannot drift.
pub(super) fn vendor_options() -> Vec<(SharedString, SharedString)> {
    use trove_core::ai::vendor::VendorId;
    [
        (VendorId::OpenAI, "OpenAI"),
        (VendorId::Anthropic, "Anthropic"),
        (VendorId::Gemini, "Google Gemini"),
        (VendorId::DashScope, "Alibaba DashScope"),
    ]
    .into_iter()
    .map(|(id, name)| (SharedString::from(id.as_str()), SharedString::from(name)))
    .collect()
}

/// Apply a vendor choice from a settings dropdown: store the id and point
/// the endpoint at that vendor's official address. The endpoint field stays
/// editable, so a relay or a local server can be typed over it afterwards.
pub(super) fn apply_vendor_choice(vendor: &mut String, base_url: &mut String, value: &str) {
    *vendor = value.to_string();
    if let Ok(id) = value.parse::<trove_core::ai::vendor::VendorId>() {
        *base_url = id.default_base_url().to_string();
    }
}

/// Which page a freshly-opened settings window shows. Menu items and other
/// entry points deep-link here; the index must track the `.page(...)` order
/// in [`SettingsView::render`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum SettingsPage {
    /// What this build is, whether it is current, and the interface language.
    #[default]
    About,
    /// Light/dark mode, the named themes, and the custom-theme folder.
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Appearance,
    /// Disk usage, the libraries, watched folders and the maintenance jobs.
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Files,
    /// How the 3D preview draws a model.
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Model,
    /// The full-text index and the visual fingerprints.
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Search,
    /// The embedding endpoint and the vector store it feeds.
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Ai,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Shortcuts,
}

impl SettingsPage {
    /// Sidebar index — must track the `.page(...)` order in `render`.
    fn index(self) -> usize {
        match self {
            Self::About => 0,
            Self::Appearance => 1,
            Self::Files => 2,
            Self::Model => 3,
            Self::Search => 4,
            Self::Ai => 5,
            Self::Shortcuts => 6,
        }
    }
}

/// Handle of the open settings window, so `open` can focus instead of
/// stacking windows. A stale handle (window was closed) fails its update
/// and a fresh window opens in its place.
#[derive(Default)]
struct SettingsWindowState(Option<AnyWindowHandle>);

impl gpui_kit::Global for SettingsWindowState {}

/// Open the settings window on its default (About) page, or focus it if it
/// is already open.
pub fn open(cx: &mut App, controller: Entity<LibraryController>) {
    open_at(SettingsPage::About, cx, controller);
}

/// Open the settings window deep-linked to `page`, or focus the existing
/// window. The page only steers a *fresh* open — an already-open window is
/// focused on whatever page it last showed.
pub fn open_at(page: SettingsPage, cx: &mut App, controller: Entity<LibraryController>) {
    let existing = cx.try_global::<SettingsWindowState>().and_then(|s| s.0);
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(860.), px(620.)), cx)),
        ..crate::app::title_bar::window_options()
    };
    let controller_for_window = controller.clone();
    let handle = cx.open_window(options, move |window, cx| {
        // Record the handle for open-or-focus before the closure returns.
        cx.set_global(SettingsWindowState(Some(window.window_handle())));
        let view = cx.new(|cx| SettingsView::new(page, controller_for_window, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Err(e) = handle {
        // Same surface as the main window's boot failure: this should never
        // happen, and there is nothing sensible to recover with.
        panic!("open settings window: {e}");
    }
}

/// Close the settings window if it is open.
///
/// The settings are the main window's companion, not an independent surface:
/// when the main window goes (closed to the tray, or the app is quitting),
/// this takes the settings window with it instead of leaving it floating
/// behind. Reopening later starts a fresh one — the window holds no state
/// worth keeping.
pub fn close(cx: &mut App) {
    if let Some(state) = cx.try_global::<SettingsWindowState>()
        && let Some(handle) = state.0
    {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    cx.set_global(SettingsWindowState(None));
}

/// DB-backed numbers the settings pages show, snapshotted so renders never
/// query the database. Refreshed on open and on busy / library-root
/// transitions (job finished, library hot-swapped).
#[derive(Clone)]
struct StatsSnapshot {
    library: LibraryStats,
    sig_coverage: (u64, u64),
}

/// The settings window root view.
pub struct SettingsView {
    controller: Entity<LibraryController>,
    focus_handle: FocusHandle,
    /// Deep-linked page for the first render only; `render` consumes it and
    /// the `Settings` widget owns the selection afterwards.
    initial_page: Option<SettingsPage>,
    /// Snapshot of the stats / fingerprint-coverage queries (see
    /// [`StatsSnapshot`]).
    stats: StatsSnapshot,
    /// Disk usage, measured on the background executor. `None` until the
    /// first walk finishes; the Files page shows a measuring line meanwhile.
    storage: Option<trove_core::services::storage::StorageReport>,
    /// Last-seen `busy` flag and library root: a transition means a job
    /// finished or the library changed, both of which stale the snapshot.
    last_busy: bool,
    last_root: PathBuf,
    /// Kept for the life of the view: dropping it unregisters the OS
    /// light/dark observer.
    _appearance: Subscription,
    /// Which slice of the shortcut list the Shortcuts page shows.
    shortcut_filter: shortcuts::ShortcutFilter,
    /// The action waiting for its key, if any. While one waits, the
    /// interceptor below owns every keystroke this window receives.
    capturing: Option<&'static str>,
    capture_interceptor: Option<Subscription>,
    /// This window, so the interceptor records keys typed here and leaves
    /// every other window's keystrokes alone.
    window_id: WindowId,
}

impl SettingsView {
    fn new(
        page: SettingsPage,
        controller: Entity<LibraryController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Follow the OS light/dark switch while this window is open, same as
        // the main window.
        let _appearance = window.observe_window_appearance(|window, cx| {
            crate::app::theme::apply_from_settings(Some(window), cx);
        });
        cx.observe(&controller, |this, _, cx| {
            // Snapshot refresh on job / library transitions, not per render.
            let ctl = this.controller.read(cx);
            let busy = ctl.busy;
            let root = ctl.library.root().to_path_buf();
            if busy != this.last_busy || root != this.last_root {
                this.last_busy = busy;
                this.last_root = root;
                this.stats = this.refresh_snapshots(cx);
                // A finished job (a rebuild, a backup) changes the numbers on
                // the Files page, and so does a different library.
                this.measure_storage(cx);
            }
            cx.notify();
        })
        .detach();
        let stats = Self::compute_snapshots(&controller, cx);
        let last_root = controller.read(cx).library.root().to_path_buf();
        let mut this = Self {
            controller,
            focus_handle: cx.focus_handle(),
            initial_page: Some(page),
            stats,
            storage: None,
            last_busy: false,
            last_root,
            _appearance,
            shortcut_filter: Default::default(),
            capturing: None,
            capture_interceptor: None,
            window_id: window.window_handle().window_id(),
        };
        this.focus_handle.focus(window, cx);
        this.measure_storage(cx);
        this
    }

    /// Walk the application's directories on the background executor. The
    /// measurements are IO over trees that can hold thousands of files, so
    /// they must not run under a render.
    fn measure_storage(&mut self, cx: &mut Context<Self>) {
        let (data_root, cache_root) = {
            let ctl = self.controller.read(cx);
            (
                ctl.library.root().to_path_buf(),
                ctl.library.cache().to_path_buf(),
            )
        };
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_executor()
                .spawn(
                    async move { trove_core::services::storage::report(&data_root, &cache_root) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.storage = Some(report);
                cx.notify();
            });
        })
        .detach();
    }

    fn compute_snapshots(controller: &Entity<LibraryController>, cx: &App) -> StatsSnapshot {
        let ctl = controller.read(cx);
        let library = ctl.library.stats().unwrap_or_default();
        let sig_coverage =
            trove_core::store::visual_search::signature_counts(ctl.library.store().conn())
                .unwrap_or((0, 0));
        StatsSnapshot {
            library,
            sig_coverage,
        }
    }

    fn refresh_snapshots(&self, cx: &App) -> StatsSnapshot {
        Self::compute_snapshots(&self.controller, cx)
    }

    /// Wait for the next keystroke and record it as `action`'s key.
    ///
    /// The interceptor sits ahead of every action binding: the captured key
    /// is recorded and swallowed instead of doing whatever it used to do.
    /// Keys typed into other windows are left alone — only this window's
    /// keystrokes are recorded.
    fn start_capture(&mut self, action: &'static str, cx: &mut Context<Self>) {
        self.stop_capture();
        let weak = cx.weak_entity();
        let window_id = self.window_id;
        self.capture_interceptor = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            let _ = weak.update(cx, |this, cx| this.finish_capture(action, keystroke, cx));
            // Outside the update, so the repaint it asks for never lands
            // while the view is still being written.
            cx.refresh_windows();
        }));
        self.capturing = Some(action);
        cx.notify();
    }

    /// Record what the capture caught. Esc cancels and keeps the old key;
    /// Backspace or Delete with no modifiers clears the binding; anything
    /// else becomes the new key. The repaint is the caller's — it runs
    /// outside this update.
    fn finish_capture(
        &mut self,
        action: &'static str,
        keystroke: Keystroke,
        cx: &mut Context<Self>,
    ) {
        self.stop_capture();

        let plain = keystroke.modifiers == Modifiers::default();
        let key = if plain && keystroke.key == "escape" {
            None
        } else if plain && (keystroke.key == "backspace" || keystroke.key == "delete") {
            Some(String::new())
        } else {
            Some(keystroke.to_string())
        };

        if let Some(key) = key {
            let mut config = AppConfig::load();
            config.keybindings.insert(action.to_string(), key);
            let _ = config.save();
            // Bindings are matched latest-first, so the override wins over
            // the default it replaces without a restart. This only registers;
            // it never reads the view, so it is safe mid-update.
            crate::register_keys(cx);
        }
    }

    /// Stand down from a capture (new capture, or the old one landed).
    fn stop_capture(&mut self) {
        self.capture_interceptor.take();
        self.capturing = None;
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pages are rebuilt per render so a live language switch (via
        // `refresh_windows`) re-localizes every title.
        let stats = self.stats.clone();
        let mut settings = Settings::new("trove-settings")
            .with_group_variant(GroupBoxVariant::Fill)
            .sidebar_width(px(190.));
        if let Some(page) = self.initial_page.take() {
            settings = settings.default_selected_index(SelectIndex {
                page_ix: page.index(),
                group_ix: None,
            });
        }
        let view = cx.entity();
        // The built-in pages, then the plugins page, then whatever pages the
        // app plugins contribute — appended at the end, so the hand-written
        // `SettingsPage` index above (which deep-links only to built-in
        // pages) is unaffected by plugin pages.
        let settings = settings
            .page(about::about_page(&self.controller))
            .page(appearance::appearance_page(&self.controller, cx))
            .page(files::files_page(
                &self.controller,
                stats.library.clone(),
                self.storage,
            ))
            .page(model::model_page())
            .page(search::search_page(&self.controller, stats.sig_coverage))
            .page(ai::ai_page(&self.controller, cx))
            .page(shortcuts::shortcuts_page(self, &view))
            .page(plugins::plugins_page());
        let settings = crate::plugins::settings_pages(cx)
            .into_iter()
            .fold(settings, |settings, page| settings.page(page));

        // Client-side decorations are forced app-wide, so this window draws
        // its own (title + gpui-kit's min/max/close controls).
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus_handle)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .child(rust_i18n::t!("settings.title").to_string()),
                ),
            )
            .child(settings)
    }
}
