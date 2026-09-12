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
//! executor, FTS rebuild and orphan sweep inline (they are database-bound
//! and quick). Outcomes land on [`LibraryController::notice`], a status
//! line both the general and the maintenance page render.
//!
//! Pages are rebuilt on every render, so titles follow the active locale
//! after a live language switch. The expensive numbers they show (library
//! statistics, fingerprint coverage) are *snapshots*: queried when the
//! window opens and whenever a background job finishes or the library is
//! hot-swapped (observed via `busy` / library-root transitions), never per
//! render.

mod appearance;
mod general;
mod language;
mod maintenance;
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
pub(super) use gpui_kit::component::{
    ActiveTheme, Disableable as _, IconName, Sizable, ThemeMode, WindowExt,
};
use gpui_kit::component::{Root, TitleBar};
pub(super) use gpui_kit::prelude::FluentBuilder as _;
pub(super) use gpui_kit::*;

pub(super) use crate::app::i18n::SUPPORTED;
pub(super) use crate::library::LibraryController;
pub(super) use trove_core::config::{AppConfig, Appearance};
pub(super) use trove_core::keybindings::{self, KeyBindingConfig};
pub(super) use trove_core::store::stats::LibraryStats;

/// Which page a freshly-opened settings window shows. Menu items and other
/// entry points deep-link here; the index must track the `.page(...)` order
/// in [`SettingsView::render`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum SettingsPage {
    #[default]
    General,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Appearance,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Search,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Maintenance,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Language,
    #[expect(dead_code, reason = "deep-link target; no entry point wired yet")]
    Shortcuts,
}

impl SettingsPage {
    /// Sidebar index — must track the `.page(...)` order in `render`.
    fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Appearance => 1,
            Self::Search => 2,
            Self::Maintenance => 3,
            Self::Language => 4,
            Self::Shortcuts => 5,
        }
    }
}

/// Handle of the open settings window, so `open` can focus instead of
/// stacking windows. A stale handle (window was closed) fails its update
/// and a fresh window opens in its place.
#[derive(Default)]
struct SettingsWindowState(Option<AnyWindowHandle>);

impl gpui_kit::Global for SettingsWindowState {}

/// Open the settings window on its default (General) page, or focus it if
/// it is already open.
pub fn open(cx: &mut App, controller: Entity<LibraryController>) {
    open_at(SettingsPage::General, cx, controller);
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
    /// Last-seen `busy` flag and library root: a transition means a job
    /// finished or the library changed, both of which stale the snapshot.
    last_busy: bool,
    last_root: PathBuf,
    /// Kept for the life of the view: dropping it unregisters the OS
    /// light/dark observer.
    _appearance: Subscription,
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
            }
            cx.notify();
        })
        .detach();
        let stats = Self::compute_snapshots(&controller, cx);
        let last_root = controller.read(cx).library.root().to_path_buf();
        let this = Self {
            controller,
            focus_handle: cx.focus_handle(),
            initial_page: Some(page),
            stats,
            last_busy: false,
            last_root,
            _appearance,
        };
        this.focus_handle.focus(window, cx);
        this
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
        let settings = settings
            .page(general::general_page(
                &self.controller,
                stats.library.clone(),
            ))
            .page(appearance::appearance_page(&self.controller, cx))
            .page(search::search_page(&self.controller, stats.sig_coverage))
            .page(maintenance::maintenance_page(&self.controller))
            .page(language::language_page(&self.controller))
            .page(shortcuts::shortcuts_page());

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
