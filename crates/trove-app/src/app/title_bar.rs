//! The window's custom title bar.
//!
//! Owns the draggable bar and the application menu bar (File / Edit / View /
//! Help, rendered by gpui-kit's `AppMenuBar` from the menus registered in
//! `main`). Dragging and double-click-to-zoom come from gpui-kit's `TitleBar`;
//! this module only supplies the bar's contents.

use gpui_kit::base::h_flex;
use gpui_kit::component::TitleBar;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::*;

use crate::library::LibraryController;

/// Correct `WindowOptions` for a window whose title bar we draw ourselves.
///
/// When constructing `WindowOptions` by hand (rather than starting from
/// [`TitleBar::window_options`]) both fields are required:
///
/// - `titlebar` opts the window into client-side-drawn title bar;
/// - `app_owns_titlebar_drag` is **mandatory on macOS** — without it the
///   system also handles title-bar double clicks, and it delays the click
///   while disambiguating a double-click, so the bar feels unresponsive.
pub fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        app_owns_titlebar_drag: true,
        // Force client-side decoration. gpui defaults to `Server` decoration,
        // in which case the window manager draws a native title bar — that
        // would stack a second bar under ours, and it also suppresses
        // gpui-kit's own window controls (min/max/close) on the right.
        window_decorations: Some(gpui::WindowDecorations::Client),
        ..Default::default()
    }
}

/// The title bar view: a `gpui_kit` `TitleBar` hosting the app menu bar. The
/// right-hand window controls (min/max/close) are drawn by `TitleBar` itself.
pub struct TitleBarView {
    /// Present for symmetry with the other panels; the menus act on the
    /// controller through actions dispatched by `AppView`.
    #[allow(dead_code)]
    controller: Entity<LibraryController>,
    menu_bar: Entity<AppMenuBar>,
    /// Signature of the menu names the bar was built from. The bar's data
    /// source (gpui-kit's `GlobalState`) is replaced on a language switch;
    /// the drift detected here triggers a reload on the next render.
    menu_signature: String,
}

impl TitleBarView {
    pub fn new(controller: Entity<LibraryController>, cx: &mut Context<Self>) -> Self {
        // `AppMenuBar::new` already returns an `Entity<AppMenuBar>`.
        let menu_bar = AppMenuBar::new(cx);
        let menu_signature = menu_signature(cx);
        Self {
            controller,
            menu_bar,
            menu_signature,
        }
    }
}

/// The joined names of the menus stored in gpui-kit's `GlobalState`.
fn menu_signature(cx: &App) -> String {
    use gpui_kit::component::global_state::GlobalState;
    if !cx.has_global::<GlobalState>() {
        return String::new();
    }
    GlobalState::global(cx)
        .app_menus()
        .iter()
        .map(|menu| menu.name.to_string())
        .collect::<Vec<_>>()
        .join("|")
}

impl Render for TitleBarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A live language switch replaces the GlobalState menu catalog; pick
        // the drift up here and rebuild the bar with the new names.
        let signature = menu_signature(cx);
        if signature != self.menu_signature {
            self.menu_signature = signature;
            self.menu_bar.update(cx, |bar, cx| bar.reload(cx));
        }
        TitleBar::new().child(
            h_flex()
                .h_full()
                .items_center()
                .pl_2()
                .gap_1()
                .child(self.menu_bar.clone()),
        )
    }
}
