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
mod rules;
mod settings;
mod state;
mod title_bar;

use actions::*;
use app::AppView;

/// Keyboard map for the asset grid. The `Workspace` key context is active
/// only while the grid (or one of its cells) holds focus, so typing in the
/// search input or elsewhere never triggers grid navigation.
const WORKSPACE_CONTEXT: &str = "Workspace";

/// Key context of the `ExplorerPanel` (collections tree). Its only binding
/// is Escape: the inline add/rename editor's input lets the key propagate,
/// so the panel can dismiss the editor.
const EXPLORER_CONTEXT: &str = "Explorer";

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
                ),
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
                MenuItem::action(
                    rust_i18n::t!("app.move_to_trash").to_string(),
                    TrashSelected,
                ),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.undo").to_string(), Undo),
                MenuItem::action(rust_i18n::t!("app.redo").to_string(), Redo),
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
    // Esc dismisses the explorer's inline add/rename editor. The input's own
    // Escape handler propagates the key, so this fires only while the editor
    // input holds focus inside the explorer panel.
    bindings.push(KeyBinding::new(
        "escape",
        CancelEditor,
        Some(EXPLORER_CONTEXT),
    ));

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

/// Initialise the CLIP semantic-search engine from the persisted config.
/// Best-effort: on any failure (missing model files, missing ONNX Runtime
/// library) the engine simply stays disabled and the status string reports
/// why, so the app keeps working with the visual-only backend.
fn init_semantic_search() {
    use trove_core::config::AppConfig;
    use trove_core::media::clip;

    let config = AppConfig::load();
    if config.search_mode() != "semantic" {
        return;
    }
    let Some(model) = config.clip_model_path() else {
        return;
    };
    // `ort` may panic if the dylib fails to load, so catch it. Failures are
    // recorded in the engine state and shown in Settings ▸ Search; there is
    // no window yet at startup, so nothing else to surface here.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| clip::configure(&model)));
}

fn main() {
    i18n::init_from_config();
    init_semantic_search();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            slim_scrollbars(cx);
            apply_menus(cx);
            register_keys(cx);

            cx.spawn(async move |cx| {
                let options = cx.update(|cx| gpui_kit::WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
                    ..crate::title_bar::window_options()
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
