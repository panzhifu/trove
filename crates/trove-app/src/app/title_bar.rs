//! The window's custom title bar.
//!
//! Owns the draggable bar and the application menu bar (File / Edit / View /
//! Help, rendered by gpui-kit's `AppMenuBar` from the menus registered here).
//! Dragging and double-click-to-zoom come from gpui-kit's `TitleBar`; this
//! module also owns the menu definitions, so the bar and the data it renders
//! live in the same place.

use gpui_kit::base::h_flex;
use gpui_kit::component::TitleBar;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::*;

use crate::app::actions::*;
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
        // Wayland: the dock/launcher matches this against the .desktop entry
        // (StartupWMClass) to show the app icon; the embedded `icon` field
        // above only applies to X11.
        app_id: Some("trove".to_string()),
        // X11-only in gpui (Wayland takes the icon from the .desktop entry);
        // embedded so it ships inside the binary.
        icon: Some(std::sync::Arc::new(
            image::load_from_memory(include_bytes!("../../../../design/icon/trove-256.png"))
                .expect("embedded app icon is a valid PNG")
                .into_rgba8(),
        )),
        ..Default::default()
    }
}

// ============================================================================
// Menus
// ============================================================================

/// (Re)build the application menus from the active locale. Called at startup
/// and again after a live language switch in Settings.
///
/// Menus go two places: `cx.set_menus` feeds the platform (shortcuts,
/// Wayland global-menu integration), while gpui-kit's in-window
/// [`AppMenuBar`] reads its own `GlobalState` catalog — without the second
/// write the title-bar bar renders empty.
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
                MenuItem::action(
                    rust_i18n::t!("app.manage_libraries").to_string(),
                    ManageLibraries,
                ),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.import_files").to_string(), ImportFiles),
                MenuItem::action(rust_i18n::t!("app.import_url").to_string(), ImportUrl),
                MenuItem::action(rust_i18n::t!("app.screenshot").to_string(), ScreenshotFull),
                MenuItem::action(
                    rust_i18n::t!("app.screenshot_region").to_string(),
                    ScreenshotRegion,
                ),
                MenuItem::action(
                    rust_i18n::t!("app.screenshot_active_window").to_string(),
                    ScreenshotActiveWindow,
                ),
                MenuItem::action(
                    rust_i18n::t!("app.screenshot_window").to_string(),
                    ScreenshotWindow,
                ),
                MenuItem::action(
                    rust_i18n::t!("app.screenshot_screen").to_string(),
                    ScreenshotScreen,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    rust_i18n::t!("app.export_library").to_string(),
                    ExportLibrary,
                ),
                MenuItem::action(
                    rust_i18n::t!("app.import_library").to_string(),
                    ImportLibrary,
                ),
                MenuItem::action(
                    rust_i18n::t!("app.find_duplicates").to_string(),
                    FindDuplicates,
                ),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.export_backup").to_string(), ExportBackup),
                MenuItem::action(
                    rust_i18n::t!("app.export_media_package").to_string(),
                    ExportMediaPackage,
                ),
                MenuItem::action(rust_i18n::t!("xmp.menu").to_string(), ExportXmp),
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
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.paste_import").to_string(), PasteImport),
                MenuItem::action(rust_i18n::t!("app.copy_image").to_string(), CopyImage),
                MenuItem::action(
                    rust_i18n::t!("workspace.batch_rename").to_string(),
                    BatchRename,
                ),
                MenuItem::action(
                    rust_i18n::t!("workspace.batch_convert").to_string(),
                    BatchConvert,
                ),
                MenuItem::action(rust_i18n::t!("edit.menu").to_string(), BatchEdit),
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
            items: vec![
                MenuItem::action(rust_i18n::t!("app.check_updates").to_string(), CheckUpdates),
                MenuItem::separator(),
                MenuItem::action(rust_i18n::t!("app.about").to_string(), About),
            ],
            disabled: false,
        },
    ]
}

// ============================================================================
// Title bar view
// ============================================================================

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
