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

use super::open_with_apps::discover_apps;
use super::purge_gated;

/// Build the right-click context menu for an asset cell.
pub(crate) fn asset_context_menu(
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    trashed: bool,
) -> PopupMenu {
    if trashed {
        return trash_menu(menu, controller, asset_id);
    }

    let (favorite, current_status, current_clearance, is_image, font_file) = controller
        .read(cx)
        .library
        .asset(asset_id)
        .ok()
        .flatten()
        .map(|a| {
            let font_file = if a.kind == AssetKind::Font {
                a.content_hash.clone().map(|hash| {
                    // Same blob resolution the Inspector uses: the stored
                    // blob, or the linked original for linked fonts.
                    let blob =
                        trove_core::media::thumb::blob_path(controller.read(cx).library.root(), &a);
                    (hash, blob)
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

    let add_submenu = PopupMenu::build(window, cx, move |menu, _window, cx| {
        build_collection_submenu(menu, &ctl_build, asset_id, cx)
    });
    let status_submenu = PopupMenu::build(window, cx, {
        let c_status = c_status.clone();
        move |menu, _window, _cx| {
            build_usage_status_submenu(menu, &c_status, asset_id, current_status)
        }
    });
    let commercial_submenu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
        build_commercial_use_submenu(menu, &c_commercial, asset_id, current_clearance)
    });

    let disk_path = {
        let ctl = controller.read(cx);
        ctl.library
            .asset(asset_id)
            .ok()
            .flatten()
            .and_then(|a| trove_core::media::thumb::blob_path(ctl.library.root(), &a))
    };

    let mut menu = menu
        .min_w(px(200.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("autotag.menu").to_string()).on_click({
                let controller = controller.clone();
                move |_, window, cx| {
                    crate::library::jobs::start_analysis_app(
                        &controller,
                        crate::library::jobs::AnalysisTarget::Selection,
                        window,
                        cx,
                    );
                }
            }),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("transcribe.menu").to_string()).on_click({
                let controller = controller.clone();
                move |_, window, cx| {
                    crate::library::jobs::start_transcription_app(
                        &controller,
                        crate::library::jobs::TranscribeTarget::Selection,
                        window,
                        cx,
                    );
                }
            }),
        )
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
                    if let Err(error) = ctl.library.set_assets_favorite(&ids, !favorite) {
                        ctl.report_error(
                            rust_i18n::t!("workspace.favorite_failed", error = error.to_string())
                                .to_string(),
                        );
                    }
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
            PopupMenuItem::new(rust_i18n::t!("workspace.search_by_image").to_string()).on_click(
                move |_, window, cx| {
                    let ctl = c_search.clone();
                    let ids = ctl.read(cx).action_targets(asset_id);
                    if let Some(id) = ids.first() {
                        open_image_search(*id, &ctl, window, cx);
                    }
                },
            ),
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
        let reveal_path = path.clone();
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.reveal_in_file_manager").to_string())
                .on_click(move |_, _, _cx| {
                    crate::panels::common::reveal_path(&reveal_path);
                }),
        );
        // --- Open With submenu ---
        let open_with_menu =
            build_open_with_submenu(window, cx, controller, asset_id, path.clone());
        menu = menu.item(PopupMenuItem::submenu(
            rust_i18n::t!("workspace.open_with").to_string(),
            open_with_menu,
        ));
        menu = menu.separator();
    }
    // Export: hand the originals — or converted copies — to a folder the
    // user picks. Every kind is exportable (images and videos take format
    // targets, the rest copies as-is), so the entry is unconditional
    // outside the trash.
    {
        let ctl_export = controller.clone();
        menu = menu
            .item(
                PopupMenuItem::new(rust_i18n::t!("workspace.export").to_string()).on_click(
                    move |_, window, cx| {
                        let ids = ctl_export.read(cx).action_targets(asset_id);
                        crate::dialogs::export::ExportDialog::open(
                            window,
                            cx,
                            ctl_export.clone(),
                            ids,
                        );
                    },
                ),
            )
            .separator();
    }
    // Fonts: system-level install / uninstall right from the grid, same
    // user-level mechanism as the Inspector button.
    if let Some((hash, blob)) = font_file {
        let installed = crate::fonts::is_installed(&hash);
        let ctl_font = controller.clone();
        let hash_font = hash.clone();
        let blob_font = blob.clone();
        menu = menu.item(
            PopupMenuItem::new(if installed {
                rust_i18n::t!("inspector.font_uninstall").to_string()
            } else {
                rust_i18n::t!("inspector.font_install").to_string()
            })
            .on_click(move |_, _, cx| {
                let outcome = if installed {
                    crate::fonts::uninstall(&hash_font).map(|_| ())
                } else {
                    match &blob_font {
                        Some(blob) => crate::fonts::install(blob, &hash_font).map(|_| ()),
                        None => Err(trove_core::Error::NotFound("font file")),
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

    // Image sequences. The store owns every rule — at least three frames, one
    // folder, matching sizes, not already a member of another run — and each
    // refusal names what went wrong, so the menu decides only whether an item is
    // worth showing and lets the status-bar notice carry the answer. Grouping
    // uses the default rate; a run's rate is one change away and the CLI takes
    // `--fps`.
    let targets = controller.read(cx).action_targets(asset_id);
    let in_sequence = controller
        .read(cx)
        .library
        .sequence_of(asset_id)
        .ok()
        .flatten()
        .is_some();
    if is_image && !in_sequence && targets.len() >= trove_core::media::sequence::MIN_FRAMES {
        let c_sequence = controller.clone();
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.create_sequence").to_string()).on_click(
                move |_, _, cx| {
                    c_sequence.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        match ctl
                            .library
                            .create_sequence(&ids, trove_core::media::sequence::DEFAULT_FPS)
                        {
                            Ok(_) => ctl.generation += 1,
                            Err(error) => {
                                ctl.notice = Some(
                                    rust_i18n::t!(
                                        "workspace.sequence_failed",
                                        error = error.to_string()
                                    )
                                    .to_string(),
                                );
                            }
                        }
                        cx.notify();
                    });
                },
            ),
        );
    }
    if in_sequence {
        let c_dissolve = controller.clone();
        menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.dissolve_sequence").to_string()).on_click(
                move |_, _, cx| {
                    c_dissolve.update(cx, move |ctl, cx| {
                        let ids = ctl.action_targets(asset_id);
                        match ctl.library.dissolve_for_assets(&ids) {
                            Ok(0) => {
                                ctl.notice = Some(
                                    rust_i18n::t!("workspace.sequence_none_to_dissolve")
                                        .to_string(),
                                );
                            }
                            Ok(_) => ctl.generation += 1,
                            Err(error) => {
                                ctl.notice = Some(
                                    rust_i18n::t!(
                                        "workspace.sequence_failed",
                                        error = error.to_string()
                                    )
                                    .to_string(),
                                );
                            }
                        }
                        cx.notify();
                    });
                },
            ),
        );
        menu = menu.separator();
    }
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
                    match ctl.library.trash_assets(&ids) {
                        // The selection only moves when the trash did: a
                        // silent failure used to leave the user reading a
                        // "gone" grid while the rows were still in the
                        // database.
                        Ok(_) => ctl.deselect(&ids),
                        Err(error) => {
                            ctl.report_error(
                                rust_i18n::t!("workspace.trash_failed", error = error.to_string())
                                    .to_string(),
                            );
                        }
                    }
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
                        if let Err(error) = ctl.library.restore_assets(&ids) {
                            ctl.report_error(
                                rust_i18n::t!(
                                    "workspace.restore_failed",
                                    error = error.to_string()
                                )
                                .to_string(),
                            );
                        }
                        ctl.deselect(&ids);
                        cx.notify();
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("workspace.delete_forever").to_string()).on_click(
                move |_, window, cx| {
                    let ids = ctl_purge.read(cx).action_targets(asset_id);
                    purge_gated(&ctl_purge, ids, window, cx);
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
    let mut items: Vec<(Uuid, String)> = Vec::new();
    if let Ok(roots) = controller.read(cx).library.collection_roots() {
        for root in roots {
            items.push((root.id, root.name.clone()));
            if let Ok(children) = controller
                .read(cx)
                .library
                .collection_children(Some(root.id))
            {
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
                let outcome = ctl.library.add_assets_to_collection(cid, &ids);
                ctl.report_failed("adding assets to a folder", outcome);
                ctl.generation += 1;
                cx.notify();
            });
        }));
    }
    menu
}

/// "Open With" submenu: discover installed apps for this file type,
/// offer a "default" entry, and fall back to the platform chooser.
fn build_open_with_submenu(
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    file_path: PathBuf,
) -> Entity<PopupMenu> {
    let apps = discover_apps(&file_path);
    let ctl_default = controller.clone();
    let path_default = file_path.clone();

    PopupMenu::build(_window, cx, move |menu, _window, _cx| {
        // --- "Open with Default App" entry ---
        let ctl_d = ctl_default.clone();
        let path_d = path_default.clone();
        let mut menu = menu.item(
            PopupMenuItem::new(rust_i18n::t!("workspace.open_with_default").to_string()).on_click(
                move |_, _, cx| {
                    do_open_with(&ctl_d, asset_id, None, &path_d, cx);
                },
            ),
        );

        // --- Discovered apps ---
        if !apps.is_empty() {
            menu = menu.separator();
            for app in apps {
                if let Some(exec_path) = &app.exec_path {
                    let ctl = controller.clone();
                    let path = file_path.clone();
                    let name = app.name.clone();
                    let exec = exec_path.clone();
                    menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                        do_open_with(&ctl, asset_id, Some(&exec), &path, cx);
                    }));
                }
            }
        }

        menu
    })
}

/// Shared open-with action: resolve the asset, call the library to hand it
/// to the chosen (or default) application, and surface the result in the
/// status bar.
fn do_open_with(
    controller: &Entity<LibraryController>,
    asset_id: Uuid,
    app_path: Option<&std::path::Path>,
    file_path: &std::path::Path,
    cx: &mut App,
) {
    controller.update(cx, move |ctl, cx| {
        match ctl.library.open_in_external(asset_id, app_path) {
            Ok(_) => {
                let file_name = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let app_name = match app_path {
                    Some(p) => p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Unknown")
                        .to_string(),
                    None => rust_i18n::t!("workspace.open_with_default_app").to_string(),
                };
                ctl.notice = Some(
                    rust_i18n::t!("workspace.open_with_done", name = file_name, app = app_name)
                        .to_string(),
                );
            }
            Err(e) => {
                ctl.notice =
                    Some(rust_i18n::t!("workspace.open_with_failed", error = e).to_string());
            }
        }
        cx.notify();
    });
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
