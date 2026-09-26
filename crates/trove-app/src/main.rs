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

// Embeds `locales/*.toml` into the binary (compile-time parse; `zh-CN.toml` is
// the fallback catalog). After this, `rust_i18n::t!` resolves keys and
// `rust_i18n::set_locale` switches the process-global language — see `i18n`.
rust_i18n::i18n!("locales", fallback = "zh-CN");

use gpui_kit::component::Root;
use gpui_kit::*;

mod app;
mod assets;
mod components;
mod dialogs;
mod fonts;
mod library;
mod logging;
mod panels;
mod plugins;

use app::AppView;

fn main() {
    // Logging first: everything after this point can emit events.
    logging::init();
    app::i18n::init_from_config();
    gpui_kit::application()
        .with_assets(assets::TroveAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            // Themes before the first paint: the registry has to know every
            // theme before `apply_from_settings` picks one per mode.
            crate::app::theme::register_builtin_themes(cx);
            // User themes last: they may redefine a bundled name.
            crate::app::theme::register_user_themes(cx);
            crate::app::theme::apply_from_settings(None, cx);
            crate::components::scrollbar::init(cx);

            // Menus are owned by the title bar module — it renders them, so it
            // also defines and registers them.
            crate::app::title_bar::apply_menus(cx);

            // Plugins last but before any window: the pipeline registry must
            // be complete before the first import builds it.
            crate::plugins::init(cx);

            // One read serves the whole boot: the keybindings the actions
            // register with, and whether a library exists at all — decided
            // here, so the spawned closure moves a bool rather than the
            // config.
            let config = trove_core::config::AppConfig::load();
            app::keybindings::register(cx, &config);
            let has_library = !config.libraries.is_empty();

            cx.spawn(async move |cx| {
                let options = cx.update(|cx| gpui_kit::WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
                    ..crate::app::title_bar::window_options()
                });
                if let Err(error) = cx.open_window(options, |window, cx| {
                    // With no library there is nothing to open, so the asset
                    // manager is the whole application until one exists.
                    // Both branches return the same `Root` type, so the
                    // window's root is decided here and nowhere else.
                    if !has_library {
                        let view = cx.new(|cx| app::LibraryManagerView::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    } else {
                        let view = cx.new(|cx| AppView::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    }
                })
                // A window that will not open is logged, not fatal: the
                // process (and its tray) stay up, and the log line names the
                // cause instead of a backtrace naming a panic.
                {
                    tracing::error!(%error, "failed to open the main window");
                }
            })
            .detach();
        });
}
