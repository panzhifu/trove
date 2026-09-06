//! The application's root view.
//!
//! This is the window content the app shell assembles: it owns the
//! [`LibraryController`], composes the dock layout and the title bar, and hosts
//! a whole-window file-drop surface. `main` only boots the window and mounts
//! this view inside a `Root`.

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dock::{DockLayout, DockPlacement, DockSkin, panel_handle};

// Re-export gpui's `Widget`/styled-building names (div, Window, Context,
// Render, IntoElement, ExternalPaths, …) plus gpui-kit's styling extensions.
use gpui_kit::*;

use crate::actions::*;
use crate::jobs;
use crate::panels::{ExplorerPanel, InspectorPanel, TagsPanel, WorkspacePanel};
use crate::state::LibraryController;
use crate::title_bar::TitleBarView;
use trove_core::config::AppConfig;
use trove_core::library::Library;

fn default_library_path() -> PathBuf {
    // `TROVE_LIBRARY_DIR` overrides, then the persisted choice, then the
    // default — resolution lives in `trove-core::config`.
    AppConfig::load().resolved_library_path()
}

/// Root view: owns the controller and hosts the dock area, plus a drop
/// surface that imports any dropped files into the current collection.
pub struct AppView {
    controller: Entity<LibraryController>,
    dock: Entity<gpui_kit::component::dock::DockArea>,
    title_bar: Entity<TitleBarView>,
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let library = Library::open(default_library_path())
            .unwrap_or_else(|e| panic!("open library: {e}"));
        let controller = cx.new(|_cx| LibraryController::new(library));
        let title_bar = cx.new(|cx| TitleBarView::new(controller.clone(), cx));

        let explorer = cx.new(|cx| ExplorerPanel::new(window, cx, controller.clone()));
        let workspace = cx.new(|cx| WorkspacePanel::new(window, cx, controller.clone()));
        let tags = cx.new(|cx| TagsPanel::new(cx, controller.clone()));
        let inspector = cx.new(|cx| InspectorPanel::new(window, cx, controller.clone()));

        let (dock, skin) = DockSkin::dock_area("trove", None, window, cx);
        dock.update(cx, |area, cx| {
            // Panels must be registered through `panel_handle` + `panel_view`:
            // a bare entity (`DockLayout::panel`) cannot be downcast back into
            // a presentation handle by the skin, so `title`/`title_suffix`
            // hooks are never called and the title bar falls back to
            // `panel_name`.
            area.set_dock(
                DockPlacement::Left,
                DockLayout::tabs().panel_view(panel_handle(explorer), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Left, px(260.), window, cx);
            area.set_center(
                DockLayout::tabs().panel_view(panel_handle(workspace), cx),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Right,
                DockLayout::tabs()
                    .panel_view(panel_handle(tags), cx)
                    .panel_view(panel_handle(inspector), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Right, px(300.), window, cx);
        });
        skin.set_ellipsis_menu(false, cx);

        Self { controller, dock, title_bar }
    }

    /// File ▸ Import files… : system file picker, then background import.
    fn prompt_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(rust_i18n::t!("app.import_prompt").into_owned().into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(result) = rx.await {
                if let Ok(Some(paths)) = result {
                    let _ = handle.update(cx, |_view, window, cx| {
                        jobs::import_paths_app(&ctl, paths, window, cx);
                    });
                }
            }
        })
        .detach();
    }

    /// Help ▸ About Trove.
    fn show_about(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title(rust_i18n::t!("app.about").to_string())
                .width(px(360.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .text_sm()
                        .text_color(rgb(0x8a8a8a))
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(0x1f1f1f))
                                .child("Trove"),
                        )
                        .child(rust_i18n::t!("app.about_body").to_string())
                        .child(rust_i18n::t!("app.version", version = "0.1.0").to_string()),
                )
        });
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.clone();

        // The dialog/sheet/notification layers are rendered by the app root,
        // not by `Root::render` itself: without these children, dialogs opened
        // via `window.open_dialog` exist in state but never draw.
        let dialog_layer = gpui_kit::component::Root::render_dialog_layer(window, cx);
        let sheet_layer = gpui_kit::component::Root::render_sheet_layer(window, cx);
        let notification_layer = gpui_kit::component::Root::render_notification_layer(window, cx);

        div()
            .id("app-root")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            // Whole-window file drop surface.
            .on_drop::<ExternalPaths>(move |paths, window, cx| {
                jobs::import_paths_app(
                    &controller,
                    paths.0.iter().cloned().collect(),
                    window,
                    cx,
                );
            })
            // Menu-bar actions: handled here so they work wherever the focus
            // is (the menu bar itself never holds the grid's focus).
            .on_action(cx.listener(|this, _: &ImportFiles, window, cx| {
                this.prompt_import(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                crate::settings::SettingsDialog::open(window, cx, this.controller.clone());
            }))
            .on_action(cx.listener(|this, _: &ShowAllAssets, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.select_collection(None));
            }))
            .on_action(cx.listener(|this, _: &ShowTrash, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.select_trash());
            }))
            .on_action(cx.listener(|this, _: &RefreshLibrary, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.select_all_visible());
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                this.controller.update(cx, |ctl, _cx| ctl.clear_selection());
            }))
            .on_action(cx.listener(|this, _: &TrashSelected, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    ctl.trash_or_purge_selection();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &About, window, cx| {
                this.show_about(window, cx);
            }))
            .child(self.title_bar.clone())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.dock.clone()),
            )
            .children(dialog_layer)
            .children(sheet_layer)
            .children(notification_layer)
    }
}
