//! Trove desktop application — bootstrap and root ownership.
//!
//! Per the gpui-kit coding guides, `main` only initializes GPUI, registers
//! menus and key bindings, opens the window and mounts the root view; it
//! carries no feature or layout logic. The [`AppView`] lives in the sibling
//! `app` module.

// Windows: link as a GUI-subsystem binary in release builds.
//
// Without this the binary is a console-subsystem program, so Windows
// allocates a console window on launch and closing that console sends
// `CTRL_CLOSE_EVENT`, killing the app. A GUI process gets no console to
// close. Debug builds deliberately keep the console: `println!`/`eprintln!`
// and panics still surface there while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Embeds `locales/*.toml` into the binary (compile-time parse; `en.toml` is
// the fallback catalog). After this, `rust_i18n::t!` resolves keys and
// `rust_i18n::set_locale` switches the process-global language — see `i18n`.
rust_i18n::i18n!("locales", fallback = "en");

use gpui_kit::component::Root;
use gpui_kit::*;

mod app;
mod components;
mod dialogs;
mod fonts;
mod library;
mod panels;

use app::AppView;
use app::actions::*;

/// Keyboard map for the asset grid. The `Workspace` key context is active
/// only while the grid (or one of its cells) holds focus, so typing in the
/// search input or elsewhere never triggers grid navigation.
const WORKSPACE_CONTEXT: &str = "Workspace";

/// Key context of the `ExplorerPanel` (collections tree). Its only binding
/// is Escape: the inline add/rename editor's input lets the key propagate,
/// so the panel can dismiss the editor.
const EXPLORER_CONTEXT: &str = "Explorer";

pub(crate) fn register_keys(cx: &mut App) {
    use trove_core::config::AppConfig;
    use trove_core::keybindings::default_keybindings;

    let config = AppConfig::load();
    let defaults = default_keybindings();

    // Resolve effective key for an action (custom override > default).
    let key_for = |action: &str, fallback: &str| -> String {
        config
            .keybindings
            .get(action)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };

    let mut bindings = vec![];
    // Look up the default key for an action from `defaults` (primary source).
    let default_key = |action: &str| -> Option<String> {
        defaults
            .iter()
            .find(|d| d.action == action)
            .map(|d| d.key.to_string())
    };

    macro_rules! bind {
        ($action:ident, $action_name:literal) => {
            if let Some(k) = default_key($action_name) {
                let k = key_for($action_name, &k);
                if !k.is_empty() {
                    bindings.push(KeyBinding::new(&k, $action, Some(WORKSPACE_CONTEXT)));
                }
            }
        };
    }

    bind!(MoveLeft, "MoveLeft");
    bind!(MoveRight, "MoveRight");
    bind!(MoveUp, "MoveUp");
    bind!(MoveDown, "MoveDown");
    bind!(OpenPreview, "OpenPreview");
    // Backspace stays a fixed alias for TrashSelected.
    let trash_key = key_for("TrashSelected", "delete");
    if !trash_key.is_empty() {
        bindings.push(KeyBinding::new(
            &trash_key,
            TrashSelected,
            Some(WORKSPACE_CONTEXT),
        ));
    }
    bindings.push(KeyBinding::new(
        "backspace",
        TrashSelected,
        Some(WORKSPACE_CONTEXT),
    ));
    bind!(SelectAll, "SelectAll");
    bind!(ClearSelection, "ClearSelection");
    bind!(Undo, "Undo");
    bind!(Redo, "Redo");
    // Menu-only until the user binds a key in Settings ▸ Shortcuts.
    bind!(BatchRename, "BatchRename");
    bind!(BatchConvert, "BatchConvert");
    bind!(CopyImage, "CopyImage");
    // Paste import is global (works wherever focus is).
    bindings.push(KeyBinding::new("ctrl-shift-v", PasteImport, None));
    // Esc dismisses the explorer's inline add/rename editor. The input's own
    // Escape handler propagates the key, so this fires only while the editor
    // input holds focus inside the explorer panel.
    bindings.push(KeyBinding::new(
        "escape",
        CancelEditor,
        Some(EXPLORER_CONTEXT),
    ));

    // Screenshots are global (like paste import), not grid-scoped.
    macro_rules! bind_global {
        ($action:ident, $action_name:literal) => {
            if let Some(k) = default_key($action_name) {
                let k = key_for($action_name, &k);
                if !k.is_empty() {
                    bindings.push(KeyBinding::new(&k, $action, None));
                }
            }
        };
    }
    bind_global!(ScreenshotFull, "ScreenshotFull");
    bind_global!(ScreenshotRegion, "ScreenshotRegion");

    cx.bind_keys(bindings);
}

/// Slim the global scrollbar theme: a hairline thumb that widens slightly on
/// hover, over a narrow track. Applies to every scrollable surface at once.
fn slim_scrollbars(cx: &mut App) {
    use gpui_kit::base::{ScrollbarStyles, Theme};
    use gpui_kit::component::ActiveTheme as _;

    let mut thumb = cx.theme().muted_foreground;
    thumb.a = 0.35;
    let mut thumb_hover = thumb;
    thumb_hover.a = 0.6;
    let theme = Theme::global_mut(cx);
    theme.scrollbar = theme.scrollbar.clone().with_styles(
        ScrollbarStyles::default()
            .track(|t| t.width(px(8.)))
            .thumb(|s| s.width(px(4.)).inset(px(2.)).radius(px(2.)).bg(thumb))
            .thumb_hover(|s| s.width(px(6.)).inset(px(1.)).radius(px(3.)).bg(thumb_hover))
            .thumb_active(|s| s.width(px(6.)).inset(px(1.)).radius(px(3.)).bg(thumb_hover)),
    );
}

fn main() {
    app::i18n::init_from_config();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            // Themes before the first paint: the registry has to know every
            // theme before `apply_from_settings` picks one per mode.
            crate::app::theme::register_builtin_themes(cx);
            // User themes last: they may redefine a bundled name.
            crate::app::theme::register_user_themes(cx);
            crate::app::theme::apply_from_settings(None, cx);
            slim_scrollbars(cx);

            // Menus are owned by the title bar module — it renders them, so it
            // also defines and registers them.
            crate::app::title_bar::apply_menus(cx);

            register_keys(cx);

            cx.spawn(async move |cx| {
                let options = cx.update(|cx| gpui_kit::WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
                    ..crate::app::title_bar::window_options()
                });
                cx.open_window(options, |window, cx| {
                    let view = cx.new(|cx| AppView::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");
            })
            .detach();
        });
}
