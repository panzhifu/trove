//! Right-click context menu for assets and drag preview.

use std::path::PathBuf;

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::workspace_search::open_image_search;
use trove_core::model::{AssetKind, AssetPatch, UsageStatus};
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
    let (favorite, current_status, current_clearance, is_image, font_file) =
        assets::get(conn, asset_id)
            .ok()
            .flatten()
            .map(|a| {
                let font_file = if a.kind == AssetKind::Font {
                    a.sha256.clone().map(|sha| {
                        // Same blob resolution the Inspector uses: the stored
                        // blob, or the linked original for linked fonts.
                        let blob = if a.origin == trove_core::model::Origin::Linked {
                            a.facts.source_path.as_ref().map(PathBuf::from)
                        } else {
                            a.rel_path
                                .as_ref()
                                .map(|rel| controller.read(cx).library.root().join(rel))
                        };
                        (sha, blob)
                    })
                } else {
                    None
                };
                (
                    a.is_favorite,
                    a.usage_status,
                    a.commercial_use,
                    a.kind == AssetKind::Image,
                    font_file,
                )
            })
            .unwrap_or((false, UsageStatus::Unused, None, false, None));
    let browsed_collection = controller.read(cx).current_collection;

    let ctl_build = controller.clone();
    let c_fav = controller.clone();
    let c_trash = controller.clone();
    let c_remove = controller.clone();
    let c_search = controller.clone();
    let c_status = controller.clone();
    let c_commercial = controller.clone();

    let add_submenu = PopupMenu::build(_window, cx, move |menu, _window, cx| {
        build_collection_submenu(menu, &ctl_build, asset_id, cx)
    });
    let status_submenu = PopupMenu::build(_window, cx, {
        let c_status = c_status.clone();
        move |menu, _window, _cx| {
            build_usage_status_submenu(menu, &c_status, asset_id, current_status)
        }
    });
    let commercial_submenu = PopupMenu::build(_window, cx, move |menu, _window, _cx| {
        build_commercial_use_submenu(menu, &c_commercial, asset_id, current_clearance)
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
                rust_i18n::t!("workspace.usage_status").to_string(),
                status_submenu,
            ))
            .item(PopupMenuItem::submenu(
                rust_i18n::t!("workspace.commercial_use").to_string(),
                commercial_submenu,
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
    // Fonts: system-level install / uninstall right from the grid, same
    // user-level mechanism as the Inspector button.
    if let Some((sha, blob)) = font_file {
        let installed = crate::fonts::is_installed(&sha);
        let ctl_font = controller.clone();
        let sha_font = sha.clone();
        let blob_font = blob.clone();
        menu = menu.item(
            PopupMenuItem::new(if installed {
                rust_i18n::t!("inspector.font_uninstall").to_string()
            } else {
                rust_i18n::t!("inspector.font_install").to_string()
            })
            .on_click(move |_, _, cx| {
                let outcome = if installed {
                    crate::fonts::uninstall(&sha_font).map(|_| ())
                } else {
                    match &blob_font {
                        Some(blob) => crate::fonts::install(blob, &sha_font).map(|_| ()),
                        None => Err("font file not found".to_string()),
                    }
                };
                if let Err(e) = outcome {
                    ctl_font.update(cx, |ctl, cx| {
                        ctl.notice = Some(
                            rust_i18n::t!("notice.font_install_failed", error = e).to_string(),
                        );
                        cx.notify();
                    });
                }
                cx.refresh_windows();
            }),
        );
        if installed {
            menu = menu.item(PopupMenuItem::new(
                rust_i18n::t!("inspector.font_installed").to_string(),
            ));
        }
        menu = menu.separator();
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

/// Context menu for a virtual (not imported) system-font cell: import a
/// copy into the library, install it for the current user, or reveal the
/// file in the file manager.
pub(crate) fn virtual_font_context_menu(
    menu: PopupMenu,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    font_id: Uuid,
) -> PopupMenu {
    let Some(font) = controller.read(cx).virtual_fonts.get(&font_id).cloned() else {
        return menu;
    };
    let ctl_import = controller.clone();
    let ctl_install = controller.clone();
    let import_path = font.path.clone();
    let install_path = font.path.clone();
    let reveal_path = font.path.clone();
    menu.min_w(px(200.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("sysfonts.import").to_string()).on_click(
                move |_, window, cx| {
                    crate::library::jobs::import_paths_app(
                        &ctl_import,
                        vec![import_path.clone()],
                        window,
                        cx,
                    );
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("inspector.font_install").to_string()).on_click(
                move |_, _, cx| {
                    // The install destination is the content hash; system
                    // fonts have no stored sha, so compute it on click.
                    let outcome = trove_core::media::blob::hash_file(&install_path)
                        .map_err(|e| e.to_string())
                        .and_then(|(sha, _)| {
                            crate::fonts::install(&install_path, &sha).map(|_| ())
                        });
                    if let Err(e) = outcome {
                        ctl_install.update(cx, |ctl, cx| {
                            ctl.notice = Some(
                                rust_i18n::t!("notice.font_install_failed", error = e).to_string(),
                            );
                            cx.notify();
                        });
                    }
                    cx.refresh_windows();
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("workspace.reveal_in_file_manager").to_string())
                .on_click(move |_, _, _| {
                    crate::panels::common::reveal_path(&reveal_path);
                }),
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

/// "Usage status" submenu: mark the asset used / unused. Applies to the
/// whole selection (or just the clicked asset).
fn build_usage_status_submenu(
    mut menu: PopupMenu,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    current: UsageStatus,
) -> PopupMenu {
    for (status, key) in [
        (UsageStatus::Unused, "workspace.status_unused"),
        (UsageStatus::Used, "workspace.status_used"),
    ] {
        let controller = controller.clone();
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!(key).to_string())
                .checked(current == status)
                .on_click(move |_, _, cx| {
                    apply_patch(
                        &controller,
                        asset_id,
                        AssetPatch {
                            usage_status: Some(status),
                            ..Default::default()
                        },
                        cx,
                    );
                }),
        );
    }
    menu
}

/// "Commercial use" submenu: the license clearance tri-state (allowed /
/// forbidden / not verified).
fn build_commercial_use_submenu(
    mut menu: PopupMenu,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    current: Option<bool>,
) -> PopupMenu {
    for (clearance, key) in [
        (Some(true), "workspace.commercial_allowed"),
        (Some(false), "workspace.commercial_forbidden"),
        (None, "workspace.commercial_unverified"),
    ] {
        let controller = controller.clone();
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!(key).to_string())
                .checked(current == clearance)
                .on_click(move |_, _, cx| {
                    apply_patch(
                        &controller,
                        asset_id,
                        AssetPatch {
                            commercial_use: Some(clearance),
                            ..Default::default()
                        },
                        cx,
                    );
                }),
        );
    }
    menu
}

/// Patch the whole selection (or just the clicked asset) and refresh the
/// grid. Failures surface in the status bar, matching other bulk actions.
fn apply_patch(
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    patch: AssetPatch,
    cx: &mut App,
) {
    controller.update(cx, |ctl, cx| {
        let ids = ctl.action_targets(asset_id);
        for id in ids {
            if let Err(e) = ctl.library.patch_asset(id, &patch) {
                ctl.notice = Some(
                    rust_i18n::t!("workspace.trash_failed", error = e.to_string()).to_string(),
                );
            }
        }
        ctl.generation += 1;
        cx.notify();
    });
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
