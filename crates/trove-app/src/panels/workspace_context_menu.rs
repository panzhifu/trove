//! Right-click context menu for assets and drag preview.

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::workspace_search::open_image_search;
use trove_core::model::AssetKind;
use trove_core::services::open_with;
use trove_core::store::{assets, collections};

/// Cap on the "Open with" list. Beyond a handful of entries the submenu
/// becomes a scrolling search problem; the default handler is always first.
const MAX_OPEN_WITH_APPS: usize = 10;

/// Build the right-click context menu for an asset cell.
pub(crate) fn asset_context_menu(
    menu: PopupMenu,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    trashed: bool,
) -> PopupMenu {
    if trashed {
        return trash_menu(menu, controller, asset_id);
    }

    let conn = controller.read(cx).library.store().conn();
    let (favorite, current_label, is_image, mime) = assets::get(conn, asset_id)
        .ok()
        .flatten()
        .map(|a| {
            (
                a.is_favorite,
                a.color_label,
                a.kind == AssetKind::Image,
                a.mime,
            )
        })
        .unwrap_or((false, None, false, String::new()));
    let browsed_collection = controller.read(cx).current_collection;

    let ctl_build = controller.clone();
    let c_fav = controller.clone();
    let c_trash = controller.clone();
    let c_remove = controller.clone();
    let c_search = controller.clone();
    let c_label = controller.clone();

    let add_submenu = PopupMenu::build(_window, cx, move |menu, _window, cx| {
        build_collection_submenu(menu, &ctl_build, asset_id, cx)
    });
    let label_submenu = PopupMenu::build(_window, cx, move |menu, _window, _cx| {
        build_color_label_submenu(menu, &c_label, asset_id, current_label.as_deref())
    });

    let disk_path = {
        let ctl = controller.read(cx);
        assets::get(ctl.library.store().conn(), asset_id)
            .ok()
            .flatten()
            .and_then(|a| a.rel_path)
            .map(|rel| ctl.library.root().join(rel))
    };

    let mut menu =
        menu.min_w(px(200.))
            .item(
                PopupMenuItem::new(if favorite {
                    rust_i18n::t!("workspace.remove_from_favorites").to_string()
                } else {
                    rust_i18n::t!("workspace.add_to_favorites").to_string()
                })
                .checked(favorite)
                .on_click(move |_, _, cx| {
                    c_fav.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        let _ = ctl.library.set_assets_favorite(&ids, !favorite);
                        ctl.generation += 1;
                        cx.notify();
                    });
                }),
            )
            .separator()
            .item(PopupMenuItem::submenu(
                rust_i18n::t!("workspace.color_label").to_string(),
                label_submenu,
            ))
            .separator()
            .item(
                PopupMenuItem::new(rust_i18n::t!("workspace.search_by_image").to_string())
                    .on_click(move |_, window, cx| {
                        let ctl = c_search.clone();
                        let ids = ctl.read(cx).action_targets(asset_id);
                        if let Some(id) = ids.first() {
                            open_image_search(*id, &ctl, window, cx);
                        }
                    }),
            )
            .separator();
    // Images are the only kind we can hand over as pixels, so the entry is
    // hidden for everything else rather than failing after the click.
    if is_image {
        menu = menu
            .item(
                PopupMenuItem::new(rust_i18n::t!("workspace.copy_image").to_string()).on_click(
                    move |_, window, cx| {
                        window.dispatch_action(Box::new(crate::app::actions::CopyImage), cx);
                    },
                ),
            )
            .separator();
    }
    if let Some(path) = disk_path {
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.reveal_in_file_manager").to_string())
                .on_click(move |_, _, _cx| {
                    crate::panels::common::reveal_path(&path);
                }),
        );
        menu = menu.separator();
    }
    // "Open with": the installed applications that claim this mime type,
    // plus the desktop default. Stored assets are handed over as a working
    // copy so an editor cannot overwrite a content-addressed blob.
    if let Some(target) = crate::library::open_with::target(controller.read(cx), asset_id) {
        let apps = open_with::applications_for(&mime);
        let c_open = controller.clone();
        let submenu = PopupMenu::build(_window, cx, move |menu, _window, cx| {
            build_open_with_submenu(menu, &c_open, &target, &apps, asset_id, cx)
        });
        menu = menu
            .item(PopupMenuItem::submenu(
                rust_i18n::t!("workspace.open_with").to_string(),
                submenu,
            ))
            .separator();
    }
    let mut menu = menu
        .item(PopupMenuItem::submenu(
            rust_i18n::t!("workspace.add_to_collection").to_string(),
            add_submenu,
        ))
        .separator();
    if browsed_collection.is_some() {
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.remove_from_collection").to_string())
                .on_click(move |_, _, cx| {
                    c_remove.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        ctl.remove_from_current_collection(&ids);
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                }),
        );
        menu = menu.separator();
    }
    menu.item(
        PopupMenuItem::new(rust_i18n::t!("app.move_to_trash").to_string()).on_click(
            move |_, _, cx| {
                c_trash.update(cx, move |ctl, cx| {
                    let ids = ctl.action_targets(asset_id);
                    let _ = ctl.library.trash_assets(&ids);
                    ctl.deselect(&ids);
                    cx.notify();
                });
            },
        ),
    )
}

/// Trash-only menu: restore or delete forever.
fn trash_menu(
    menu: PopupMenu,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
) -> PopupMenu {
    let ctl_restore = controller.clone();
    let ctl_purge = controller.clone();
    menu.min_w(px(180.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("workspace.restore").to_string()).on_click(
                move |_, _, cx| {
                    ctl_restore.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        let _ = ctl.library.restore_assets(&ids);
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("workspace.delete_forever").to_string()).on_click(
                move |_, _, cx| {
                    ctl_purge.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        if let Err(e) = ctl.library.purge_assets(&ids) {
                            ctl.notice = Some(
                                rust_i18n::t!("workspace.purge_failed", error = e.to_string())
                                    .to_string(),
                            );
                        }
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                },
            ),
        )
}

/// "Color label" submenu — delegated to the shared color-label widget.
/// Applies to the whole selection (or just the clicked asset).
fn build_color_label_submenu(
    menu: PopupMenu,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    current: Option<&str>,
) -> PopupMenu {
    super::color_label::menu_entries(
        menu,
        controller,
        super::color_label::LabelTarget::Selection(asset_id),
        current,
    )
}

/// "Open with": the desktop default first, then the applications that
/// declared this mime type, then the way back for an edited working copy.
fn build_open_with_submenu(
    mut menu: PopupMenu,
    controller: &Entity<LibraryController>,
    target: &crate::library::open_with::OpenTarget,
    apps: &[open_with::Application],
    asset_id: Uuid,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let default_label = rust_i18n::t!("workspace.open_default_app").to_string();
    menu = menu
        .min_w(px(220.))
        .item(PopupMenuItem::new(default_label.clone()).on_click({
            let target = target.clone();
            move |_, window, cx| launch_external(window, cx, target.clone(), None)
        }));

    if !apps.is_empty() {
        menu = menu.separator();
    }
    for app in apps.iter().take(MAX_OPEN_WITH_APPS) {
        let label = app.name.clone();
        menu = menu.item(PopupMenuItem::new(label).on_click({
            let target = target.clone();
            let app = app.clone();
            move |_, window, cx| launch_external(window, cx, target.clone(), Some(app.clone()))
        }));
    }

    // Only shown once the working copy actually differs from the library's
    // file — importing an untouched copy would just duplicate the asset.
    if let Some(copy) = crate::library::open_with::edited_copy(controller.read(cx), asset_id) {
        let controller = controller.clone();
        menu = menu.separator().item(
            PopupMenuItem::new(rust_i18n::t!("workspace.import_edited_copy").to_string()).on_click(
                move |_, window, cx| {
                    crate::library::jobs::import_paths_app(
                        &controller,
                        vec![copy.clone()],
                        window,
                        cx,
                    );
                },
            ),
        );
    }
    menu
}

/// Hand the file to `app` (or the desktop default) off the UI thread: a
/// stored asset is copied into the working directory first, which for a
/// large file is not something to do inside a click handler.
fn launch_external(
    window: &mut Window,
    cx: &mut App,
    target: crate::library::open_with::OpenTarget,
    app: Option<open_with::Application>,
) {
    let app_label = app
        .as_ref()
        .map(|app| app.name.clone())
        .unwrap_or_else(|| rust_i18n::t!("workspace.open_default_app").to_string());
    let name = target.file_name();
    let working_copy = target.working_copy;
    let handle = window.window_handle();

    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn({
                let target = target.clone();
                let app = app.clone();
                async move { crate::library::open_with::launch(&target, app.as_ref()) }
            })
            .await;
        let _ = handle.update(cx, |_, window, cx| {
            let note = match outcome {
                Ok(()) if working_copy => Notification::info(
                    rust_i18n::t!("notice.open_with_copy", name = name, app = app_label)
                        .to_string(),
                ),
                Ok(()) => Notification::success(
                    rust_i18n::t!("notice.open_with_done", name = name, app = app_label)
                        .to_string(),
                ),
                Err(error) => Notification::warning(
                    rust_i18n::t!("notice.open_with_failed", error = error.to_string()).to_string(),
                ),
            };
            window.push_notification(note, cx);
        });
    })
    .detach();
}

/// "Add to collection" submenu listing every root + nested collection.
fn build_collection_submenu(
    mut menu: PopupMenu,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let conn = controller.read(cx).library.store().conn();
    let mut items: Vec<(Uuid, String)> = Vec::new();
    if let Ok(roots) = collections::roots(conn) {
        for root in roots {
            items.push((root.id, root.name.clone()));
            if let Ok(children) = collections::children_of(conn, Some(root.id)) {
                for child in children {
                    items.push((child.id, child.name.clone()));
                }
            }
        }
    }
    if items.is_empty() {
        menu = menu.item(PopupMenuItem::label(
            rust_i18n::t!("workspace.no_collections").to_string(),
        ));
    }
    for (cid, cname) in items {
        let controller = controller.clone();
        menu = menu.item(PopupMenuItem::new(cname).on_click(move |_, _, cx| {
            controller.update(cx, move |ctl, cx| {
                let ids = ctl.action_targets(asset_id);
                let _ = ctl.library.add_assets_to_collection(cid, &ids);
                ctl.generation += 1;
                cx.notify();
            });
        }));
    }
    menu
}

/// Drag ghost shown while dragging assets.
pub(crate) struct AssetsDragPreview {
    pub count: usize,
}
impl Render for AssetsDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().primary)
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().primary_foreground)
            .child(if self.count == 1 {
                rust_i18n::t!("workspace.drag_one").to_string()
            } else {
                rust_i18n::t!("workspace.drag_many", count = self.count).to_string()
            })
    }
}
