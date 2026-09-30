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
mod license;
mod panels;
mod plugins;

// Resolve the boot's collaborators by name, so `main` reads as a list of
// startup steps rather than a grid of qualified paths.
use app::{AppView, LibraryManagerView};
use app::{i18n, keybindings, single_instance, theme, title_bar};
use assets::TroveAssets;
use components::scrollbar;

fn main() {
    // One instance per user, before anything else opens a library: a second
    // process exits here instead of racing the first one's writer lock, watch
    // tasks and tray. Logging comes after, so the duplicate's stderr line is
    // the only output it ever produces.
    if single_instance::acquire().is_none() {
        return;
    }
    // Logging first: everything after this point can emit events.
    trove_core::logging::init(trove_core::logging::LoggingOptions::app());
    // One read serves the whole boot: the language the interface opens in,
    // the keybindings the actions register with, and whether a library
    // exists at all — decided here, before anything can have changed it.
    let config = trove_core::config::AppConfig::load();
    i18n::init_from_config(&config);
    gpui_kit::application()
        .with_assets(TroveAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Themes before the first paint: the registry has to know every
            // theme before `apply_from_settings` picks one per mode.
            theme::register_builtin_themes(cx);
            // User themes last: they may redefine a bundled name.
            theme::register_user_themes(cx);
            theme::apply_from_settings(None, cx);
            scrollbar::init(cx);

            // Menus are owned by the title bar module — it renders them, so it
            // also defines and registers them.
            title_bar::apply_menus(cx);

            // Plugins last but before any window: the pipeline registry must
            // be complete before the first import builds it.
            plugins::init(cx);

            // The boot's single config read, from before the event loop
            // opened, still serves here.
            keybindings::register(cx, &config);
            let has_library = !config.libraries.is_empty();

            cx.spawn(async move |cx| {
                let options = cx.update(|cx| gpui_kit::WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
                    ..title_bar::window_options()
                });
                if let Err(error) = cx.open_window(options, |window, cx| {
                    // With no library there is nothing to open, so the asset
                    // manager is the whole application until one exists.
                    // Both branches return the same `Root` type, so the
                    // window's root is decided here and nowhere else.
                    if !has_library {
                        let view = cx.new(|cx| LibraryManagerView::new(window, cx));
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
