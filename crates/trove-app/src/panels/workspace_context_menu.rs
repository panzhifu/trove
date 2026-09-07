//! Right-click context menu for assets and drag preview.

use gpui_kit::base::h_flex;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::ActiveTheme;
use gpui_kit::*;
use uuid::Uuid;

use crate::panels::workspace_search::open_image_search;
use crate::state::LibraryController;
use trove_core::store::{assets, collections};

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
    let favorite = assets::get(conn, asset_id)
        .ok()
        .flatten()
        .map(|a| a.is_favorite)
        .unwrap_or(false);
    let browsed_collection = controller.read(cx).current_collection;

    let ctl_build = controller.clone();
    let c_fav = controller.clone();
    let c_trash = controller.clone();
    let c_remove = controller.clone();
    let c_search = controller.clone();

    let add_submenu = PopupMenu::build(_window, cx, move |menu, _window, cx| {
        build_collection_submenu(menu, &ctl_build, asset_id, cx)
    });

    let mut menu = menu
        .min_w(px(200.))
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
        .separator()
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
