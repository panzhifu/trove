//! Trove desktop application — bootstrap and root ownership.
//!
//! Per the gpui-kit coding guides, `main` only initializes GPUI, registers
//! menus and key bindings, opens the window and mounts the root view; it
//! carries no feature or layout logic. The [`AppView`] lives in the sibling
//! `app` module.

use gpui_kit::component::Root;
use gpui_kit::*;

mod actions;
mod app;
mod jobs;
mod panels;
mod settings;
mod state;
mod title_bar;

use actions::*;
use app::AppView;

/// Keyboard map for the asset grid. The `Workspace` key context is active
/// only while the grid (or one of its cells) holds focus, so typing in the
/// search input or elsewhere never triggers grid navigation.
const WORKSPACE_CONTEXT: &str = "Workspace";

fn register_menus(cx: &mut App) {
    cx.set_menus(vec![
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Import files…", ImportFiles),
                MenuItem::action("Export library…", ExportLibrary).disabled(true),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
            ],
            disabled: false,
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Select all", SelectAll),
                MenuItem::action("Clear selection", ClearSelection),
                MenuItem::separator(),
                MenuItem::action("Move to trash", TrashSelected),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("All assets", ShowAllAssets),
                MenuItem::action("Trash", ShowTrash),
                MenuItem::separator(),
                MenuItem::action("Refresh", RefreshLibrary),
            ],
            disabled: false,
        },
        Menu {
            name: "Help".into(),
            items: vec![MenuItem::action("About Trove", About)],
            disabled: false,
        },
    ]);
}

fn register_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("enter", OpenPreview, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("delete", TrashSelected, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("backspace", TrashSelected, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("escape", ClearSelection, Some(WORKSPACE_CONTEXT)),
    ]);
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            register_menus(cx);
            register_keys(cx);

            cx.spawn(async move |cx| {
                let options = cx.update(|cx| gpui_kit::WindowOptions {
                    window_bounds: Some(WindowBounds::centered(
                        size(px(1024.), px(720.)),
                        cx,
                    )),
                    ..crate::title_bar::window_options()
                });
                cx.open_window(
                    options,
                    |window, cx| {
                        let view = cx.new(|cx| AppView::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("failed to open window");
            })
            .detach();
        });
}
