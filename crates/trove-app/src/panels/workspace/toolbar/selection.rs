//! The floating batch-action bar shown over the grid while two or more
//! assets are selected.

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use gpui_kit::{Anchor, App, Div, Entity};

use trove_core::store::{assets, collections};

use crate::library::LibraryController;
use uuid::Uuid;

/// Floating batch-action bar over the grid while two or more assets are
/// selected. Every action hits the existing batch APIs, then deselects.
pub(crate) fn selection_toolbar(
    controller: &Entity<LibraryController>,
    in_trash: bool,
    ids: Vec<Uuid>,
    cx: &App,
) -> Div {
    let count = ids.len();
    let all_favorite = if in_trash {
        false
    } else {
        let conn = controller.read(cx).library.store().conn();
        assets::by_ids(conn, &ids)
            .map(|list| list.iter().all(|a| a.is_favorite))
            .unwrap_or(false)
    };
    let ctl_fav = controller.clone();
    let ctl_trash = controller.clone();
    let ctl_restore = controller.clone();
    let ctl_purge = controller.clone();
    let ctl_add = controller.clone();
    let ctl_clear = controller.clone();

    let mut bar = h_flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_lg()
        .child(
            div()
                .px_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("workspace.selected_many", count = count).to_string()),
        );

    if in_trash {
        bar = bar
            .child(
                Button::new("sel-restore")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Undo)
                    .tooltip(rust_i18n::t!("workspace.restore").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_restore.update(cx, |ctl, cx| {
                            let ids = std::mem::take(&mut ctl.selected_assets);
                            let _ = ctl.library.restore_assets(&ids);
                            ctl.selection_anchor = None;
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("sel-purge")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip(rust_i18n::t!("workspace.delete_forever").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_purge.update(cx, |ctl, cx| {
                            let ids = std::mem::take(&mut ctl.selected_assets);
                            if let Err(e) = ctl.library.purge_assets(&ids) {
                                ctl.notice = Some(
                                    rust_i18n::t!("workspace.purge_failed", error = e.to_string())
                                        .to_string(),
                                );
                            }
                            ctl.selection_anchor = None;
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            );
    } else {
        bar = bar
            .child(
                Button::new("sel-rename")
                    .xsmall()
                    .ghost()
                    .icon(IconName::CaseSensitive)
                    .tooltip(rust_i18n::t!("workspace.batch_rename").to_string())
                    .on_click({
                        let controller = controller.clone();
                        move |_, window, cx| {
                            crate::dialogs::rename::RenameDialog::open(
                                window,
                                cx,
                                controller.clone(),
                            );
                        }
                    }),
            )
            .child(
                Button::new("sel-fav")
                    .xsmall()
                    .ghost()
                    .icon(if all_favorite {
                        IconName::HeartOff
                    } else {
                        IconName::Heart
                    })
                    .tooltip(
                        rust_i18n::t!(if all_favorite {
                            "workspace.remove_from_favorites"
                        } else {
                            "workspace.add_to_favorites"
                        })
                        .to_string(),
                    )
                    .on_click(move |_, _, cx| {
                        ctl_fav.update(cx, |ctl, cx| {
                            let ids = ctl.selected_assets.clone();
                            let _ = ctl.library.set_assets_favorite(&ids, !all_favorite);
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("sel-add")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip(rust_i18n::t!("workspace.add_to_collection").to_string())
                    .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, cx| {
                        let conn = ctl_add.read(cx).library.store().conn();
                        let mut items: Vec<(Uuid, String)> = Vec::new();
                        if let Ok(roots) = collections::roots(conn) {
                            for root in roots {
                                items.push((root.id, root.name.clone()));
                                if let Ok(children) = collections::children_of(conn, Some(root.id))
                                {
                                    for child in children {
                                        items.push((child.id, child.name.clone()));
                                    }
                                }
                            }
                        }
                        let mut menu = menu.min_w(px(180.));
                        if items.is_empty() {
                            menu = menu.item(PopupMenuItem::label(
                                rust_i18n::t!("workspace.no_collections").to_string(),
                            ));
                        }
                        for (cid, cname) in items {
                            let ctl = ctl_add.clone();
                            menu =
                                menu.item(PopupMenuItem::new(cname).on_click(move |_, _, cx| {
                                    ctl.update(cx, |ctl, cx| {
                                        let ids = ctl.selected_assets.clone();
                                        let _ = ctl.library.add_assets_to_collection(cid, &ids);
                                        ctl.generation += 1;
                                        cx.notify();
                                    });
                                }));
                        }
                        menu
                    }),
            )
            .child(
                Button::new("sel-trash")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip(rust_i18n::t!("app.move_to_trash").to_string())
                    .on_click(move |_, _, cx| {
                        ctl_trash.update(cx, |ctl, cx| {
                            ctl.trash_or_purge_selection();
                            ctl.selection_anchor = None;
                            cx.notify();
                        });
                    }),
            );
    }

    let bar = bar.child(
        Button::new("sel-clear")
            .xsmall()
            .ghost()
            .label("×")
            .tooltip(rust_i18n::t!("app.clear_selection").to_string())
            .on_click(move |_, _, cx| {
                ctl_clear.update(cx, |ctl, cx| {
                    ctl.clear_selection();
                    cx.notify();
                });
            }),
    );

    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_3()
        .flex()
        .justify_center()
        .child(bar)
}
