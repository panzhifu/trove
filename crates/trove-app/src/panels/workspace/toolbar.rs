//! Title-bar filter controls (kind / favorites / view mode / sort)
//! and the floating batch-action bar shown while several assets
//! are selected.

use super::*;

// ============================ filter controls ================================

/// Type + favorites grid filters for the title bar: a kind dropdown, a
/// heart toggle and a clear button when anything is active. The filters
/// compose with every view (collection, search, smart collection) and are
/// also how the favorites view is entered.
pub(super) fn filter_controls(controller: &Entity<LibraryController>, cx: &App) -> Div {
    let (kind, favorite, view_mode, sort, sort_desc) = {
        let ctl = controller.read(cx);
        (
            ctl.filter_kind,
            ctl.filter_favorite,
            ctl.view_mode,
            ctl.sort,
            ctl.sort_desc,
        )
    };
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let mut bar = h_flex().items_center().gap_1();

    // View toggle: grid → list → timeline, wrapping back to grid.
    let (next_mode, toggle_icon, toggle_tip) = match view_mode {
        ViewMode::Grid => (ViewMode::List, IconName::Menu, "workspace.view_list"),
        ViewMode::List => (
            ViewMode::Timeline,
            IconName::Calendar,
            "workspace.view_timeline",
        ),
        ViewMode::Timeline => (
            ViewMode::Grid,
            IconName::GalleryVerticalEnd,
            "workspace.view_grid",
        ),
    };
    bar = bar.child(
        Button::new("view-toggle")
            .xsmall()
            .ghost()
            .icon(toggle_icon)
            .tooltip(t(toggle_tip))
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    controller.update(cx, |ctl, cx| {
                        ctl.set_view_mode(next_mode);
                        cx.notify();
                    });
                }
            }),
    );

    // Sort dropdown: key + direction pairs.
    let sort_options: Vec<(AssetSort, bool, String)> = vec![
        (AssetSort::CreatedAt, true, t("workspace.sort_newest")),
        (AssetSort::CreatedAt, false, t("workspace.sort_oldest")),
        (AssetSort::Name, false, t("workspace.sort_name_asc")),
        (AssetSort::Name, true, t("workspace.sort_name_desc")),
        (AssetSort::SizeBytes, true, t("workspace.sort_size_desc")),
        (AssetSort::SizeBytes, false, t("workspace.sort_size_asc")),
        (AssetSort::Rating, true, t("workspace.sort_rating_desc")),
    ];
    bar = bar.child(
        Button::new("sort-menu")
            .xsmall()
            .ghost()
            .icon(if sort_desc {
                IconName::SortDescending
            } else {
                IconName::SortAscending
            })
            .tooltip(t("workspace.sort"))
            .dropdown_menu_with_anchor(Anchor::TopLeft, {
                let controller = controller.clone();
                move |menu, _, _| {
                    let mut menu = menu.min_w(px(170.));
                    for (value, desc, label) in &sort_options {
                        let checked = *value == sort && *desc == sort_desc;
                        let (value, desc) = (*value, *desc);
                        let controller = controller.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    controller.update(cx, |ctl, cx| {
                                        ctl.set_sort(value, desc);
                                        cx.notify();
                                    });
                                },
                            ));
                    }
                    menu
                }
            }),
    );

    // Kind dropdown: label shows the active kind, "all" when unset.
    let kind_label = match kind {
        Some(k) => t(kind_key(k)),
        None => t("workspace.filter_all_kinds"),
    };
    let options: Vec<(Option<AssetKind>, String)> =
        std::iter::once((None, t("workspace.filter_all_kinds")))
            .chain(
                [
                    AssetKind::Image,
                    AssetKind::Video,
                    AssetKind::Audio,
                    AssetKind::Document,
                    AssetKind::Archive,
                    AssetKind::Font,
                    AssetKind::Model,
                    AssetKind::Other,
                ]
                .into_iter()
                .map(|k| (Some(k), t(kind_key(k)))),
            )
            .collect();
    bar = bar.child(
        Button::new("filter-kind")
            .xsmall()
            .outline()
            .label(kind_label)
            .dropdown_menu_with_anchor(Anchor::TopLeft, {
                let controller = controller.clone();
                move |menu, _, _| {
                    let mut menu = menu.min_w(px(150.));
                    for (value, label) in &options {
                        let checked = *value == kind;
                        let value = *value;
                        let controller = controller.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    controller.update(cx, |ctl, cx| {
                                        ctl.set_filter_kind(value);
                                        cx.notify();
                                    });
                                },
                            ));
                    }
                    menu
                }
            }),
    );

    // Favorites toggle: the primary (filled) state marks the active filter.
    bar = bar.child(
        Button::new("filter-favorite")
            .xsmall()
            .when(favorite, |b| b.primary())
            .when(!favorite, |b| b.ghost())
            .icon(IconName::Heart)
            .tooltip(t("workspace.filter_favorite"))
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    controller.update(cx, |ctl, _| ctl.set_filter_favorite(!favorite));
                }
            }),
    );

    // Reset when anything is active.
    if kind.is_some() || favorite {
        bar = bar.child(
            Button::new("clear-filters")
                .xsmall()
                .ghost()
                .label("×")
                .tooltip(t("workspace.clear_filters"))
                .on_click({
                    let controller = controller.clone();
                    move |_, _, cx| controller.update(cx, |ctl, _| ctl.clear_filters())
                }),
        );
    }
    bar
}

pub(super) fn kind_key(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Image => "asset.kind.image",
        AssetKind::Video => "asset.kind.video",
        AssetKind::Audio => "asset.kind.audio",
        AssetKind::Document => "asset.kind.document",
        AssetKind::Archive => "asset.kind.archive",
        AssetKind::Font => "asset.kind.font",
        AssetKind::Model => "asset.kind.model",
        AssetKind::Other => "asset.kind.other",
    }
}

/// Floating batch-action bar over the grid while two or more assets are
/// selected. Every action hits the existing batch APIs, then deselects.
pub(super) fn selection_toolbar(
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
