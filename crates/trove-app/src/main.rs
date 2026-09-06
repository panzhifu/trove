//! Trove desktop application — bootstrap and root ownership.
//!
//! Per the gpui-kit coding guides, `main` only initializes GPUI, registers
//! menus and key bindings, opens the window and mounts the root view; it
//! carries no feature or layout logic. The [`AppView`] lives in the sibling
//! `app` module.

// Embeds `locales/*.toml` into the binary (compile-time parse; `en.toml` is
// the fallback catalog). After this, `rust_i18n::t!` resolves keys and
// `rust_i18n::set_locale` switches the process-global language — see `i18n`.
rust_i18n::i18n!("locales", fallback = "en");

use gpui_kit::component::Root;
use gpui_kit::*;

mod actions;
mod app;
mod i18n;
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

/// (Re)build the application menus from the active locale. Called at startup
/// and again after a live language switch in Settings.
///
/// Menus go two places: `cx.set_menus` feeds the platform (shortcuts,
/// Wayland global-menu integration), while gpui-kit's in-window
/// [`gpui_kit::component::menu::AppMenuBar`] reads its own `GlobalState`
/// catalog — without the second write the title-bar bar renders empty.
pub fn apply_menus(cx: &mut App) {
    use gpui_kit::component::global_state::GlobalState;

    // `Menu` is not `Clone` (it carries action trait objects), so build the
    // list once per consumer.
    cx.set_menus(build_menus());
    let owned: Vec<gpui::OwnedMenu> = build_menus().into_iter().map(|menu| menu.owned()).collect();
    GlobalState::global_mut(cx).set_app_menus(owned);
}

fn build_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: rust_i18n::t!("app.file").into_owned().into(),
            items: vec![
                MenuItem::action(rust_i18n::t!("app.import_files").to_string(), ImportFiles),
                MenuItem::action(
                    rust_i18n::t!("app.export_library").to_string(),
                    ExportLibrary,
                )
                .disabled(true),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.settings").to_string(), OpenSettings),
            ],
            disabled: false,
        },
        Menu {
            name: rust_i18n::t!("app.edit").into_owned().into(),
            items: vec![
                MenuItem::action(rust_i18n::t!("app.select_all").to_string(), SelectAll),
                MenuItem::action(
                    rust_i18n::t!("app.clear_selection").to_string(),
                    ClearSelection,
                ),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.move_to_trash").to_string(), TrashSelected),
            ],
            disabled: false,
        },
        Menu {
            name: rust_i18n::t!("app.view").into_owned().into(),
            items: vec![
                MenuItem::action(rust_i18n::t!("app.all_assets").to_string(), ShowAllAssets),
                MenuItem::action(rust_i18n::t!("app.trash").to_string(), ShowTrash),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.refresh").to_string(), RefreshLibrary),
            ],
            disabled: false,
        },
        Menu {
            name: rust_i18n::t!("app.help").into_owned().into(),
            items: vec![MenuItem::action(
                rust_i18n::t!("app.about").to_string(),
                About,
            )],
            disabled: false,
        },
    ]
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
    i18n::init_from_config();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            slim_scrollbars(cx);
            apply_menus(cx);
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
