//! The application's root view.
//!
//! This is the window content the app shell assembles: it owns the
//! [`LibraryController`], composes the dock layout and the title bar, and hosts
//! a whole-window file-drop surface. `main` only boots the window and mounts
//! this view inside a `Root`.

use std::path::PathBuf;

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dock::{DockLayout, DockPlacement, DockSkin, panel_handle};
use gpui_kit::component::notification::Notification;
use gpui_kit::prelude::FluentBuilder as _;

// Re-export gpui's `Widget`/styled-building names (div, Window, Context,
// Render, IntoElement, ExternalPaths, …) plus gpui-kit's styling extensions.
use gpui_kit::*;

use crate::app::actions::*;
use crate::app::title_bar::TitleBarView;
use crate::library::jobs;
use crate::library::{ImportPhase, LibraryController};
use crate::panels::{ExplorerPanel, InspectorPanel, TagsPanel, WorkspacePanel};
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
        let library =
            Library::open(default_library_path()).unwrap_or_else(|e| panic!("open library: {e}"));
        // Record the library for Settings ▸ recent libraries (best-effort).
        let mut config = AppConfig::load();
        let _ = config.push_recent_library(library.root().to_path_buf());
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

        // Redraw the status bar whenever the controller state changes.
        cx.observe(&controller, |_, _, cx| cx.notify()).detach();

        Self {
            controller,
            dock,
            title_bar,
        }
    }

    /// File ▸ Export library… : save-dialog, then write the metadata catalog
    /// as pretty JSON (`Library::export_metadata`). Library is not `Send`, so
    /// serialization happens on the main thread inside the window callback.
    fn prompt_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctl = self.controller.clone();
        let handle = window.window_handle();
        let dir = ctl.read(cx).library.root().to_path_buf();
        let rx = cx.prompt_for_new_path(&dir, Some("trove-export.json"));
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = handle.update(cx, |_, window, cx| {
                    let outcome = ctl
                        .update(cx, |ctl, _| ctl.library.export_metadata())
                        .and_then(|json| std::fs::write(&path, json).map_err(|e| e.into()));
                    let note = match outcome {
                        Ok(()) => Notification::success(
                            rust_i18n::t!("app.export_done", path = path.display().to_string())
                                .to_string(),
                        ),
                        Err(e) => Notification::warning(
                            rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                        ),
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }

    /// Bottom status bar: selection count, library path, import state and the
    /// latest notice (errors surface here even outside Settings).
    fn status_bar(&self, cx: &Context<Self>) -> Div {
        let ctl = self.controller.read(cx);
        let selected = ctl.selected_assets.len();
        let root = ctl.library.root().display().to_string();
        let import = match &ctl.import_phase {
            ImportPhase::Idle => rust_i18n::t!("statusbar.import_idle").to_string(),
            ImportPhase::Running { total, done } => {
                rust_i18n::t!("statusbar.import_running", done = done, total = total).to_string()
            }
            ImportPhase::Done { imported, skipped } => rust_i18n::t!(
                "statusbar.import_done",
                imported = imported,
                skipped = skipped
            )
            .to_string(),
        };
        let notice = ctl.notice.clone();
        let (undo_len, redo_len) = (ctl.library.undo_len(), ctl.library.redo_len());
        h_flex()
            .h(px(26.))
            .px_3()
            .items_center()
            .gap_4()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(rust_i18n::t!("statusbar.selected", count = selected).to_string())
            .when(undo_len > 0 || redo_len > 0, |bar| {
                bar.child(
                    div()
                        .when(undo_len > 0, |seg| {
                            seg.child(rust_i18n::t!("statusbar.undo", count = undo_len).to_string())
                        })
                        .when(undo_len > 0 && redo_len > 0, |seg| seg.child(" · "))
                        .when(redo_len > 0, |seg| {
                            seg.child(rust_i18n::t!("statusbar.redo", count = redo_len).to_string())
                        }),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(rust_i18n::t!("statusbar.library", path = root).to_string()),
            )
            .child(import)
            .when_some(notice, |bar, notice| {
                bar.child(
                    div()
                        .max_w(px(420.))
                        .truncate()
                        .text_color(cx.theme().warning)
                        .child(notice),
                )
            })
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
            if let Ok(result) = rx.await
                && let Ok(Some(paths)) = result
            {
                let _ = handle.update(cx, |_view, window, cx| {
                    jobs::import_paths_app(&ctl, paths, window, cx);
                });
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
                        .child(rust_i18n::t!("app.version", version = "0.2.1").to_string()),
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
                jobs::import_paths_app(&controller, paths.0.iter().cloned().collect(), window, cx);
            })
            // Menu-bar actions: handled here so they work wherever the focus
            // is (the menu bar itself never holds the grid's focus).
            .on_action(cx.listener(|this, _: &ImportFiles, window, cx| {
                this.prompt_import(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExportLibrary, window, cx| {
                this.prompt_export(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                crate::dialogs::settings::SettingsDialog::open(window, cx, this.controller.clone());
            }))
            .on_action(cx.listener(|this, _: &FindDuplicates, window, cx| {
                crate::dialogs::duplicates::DuplicateDialog::open(
                    window,
                    cx,
                    this.controller.clone(),
                );
            }))
            .on_action(cx.listener(|this, _: &ShowAllAssets, _, cx| {
                this.controller
                    .update(cx, |ctl, _cx| ctl.select_collection(None));
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
                this.controller
                    .update(cx, |ctl, _cx| ctl.select_all_visible());
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
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    if let Err(e) = ctl.library.undo() {
                        ctl.notice = Some(
                            rust_i18n::t!("app.undo_failed", error = e.to_string()).to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                this.controller.update(cx, |ctl, cx| {
                    if let Err(e) = ctl.library.redo() {
                        ctl.notice = Some(
                            rust_i18n::t!("app.redo_failed", error = e.to_string()).to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &About, window, cx| {
                this.show_about(window, cx);
            }))
            .child(self.title_bar.clone())
            .child(div().flex_1().min_h_0().child(self.dock.clone()))
            .child(self.status_bar(cx))
            .children(dialog_layer)
            .children(sheet_layer)
            .children(notification_layer)
    }
}
