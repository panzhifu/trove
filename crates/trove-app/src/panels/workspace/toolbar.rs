//! Title-bar filter controls (kind / favorites / view mode / sort)
//! and the floating batch-action bar shown while several assets
//! are selected.

use super::*;
use trove_core::config::AppConfig;

// ============================ filter controls ================================

/// Type + favorites grid filters for the title bar: a kind dropdown, a
/// heart toggle and a clear button when anything is active. The filters
/// compose with every view (collection, search, smart collection) and are
/// also how the favorites view is entered.
/// The icon cluster for the panel title bar: view toggle, sort, favorites.
pub(super) fn title_controls(controller: &Entity<LibraryController>, cx: &App) -> Div {
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

/// The kind dropdown for the in-panel toolbar row, styled like the
/// "colour" tab: an icon + fixed label. The active kind is marked inside
/// the menu, and the button reads as active while a kind filter is set.
pub(super) fn kind_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
    let kind = controller.read(cx).filter_kind;
    let t = |k: &str| rust_i18n::t!(k).to_string();

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
    Button::new("filter-kind")
        .ghost()
        .xsmall()
        .icon(IconName::File)
        .label(t("workspace.kind_filter"))
        .selected(kind.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(150.));
                for (value, label) in &options {
                    let checked = *value == kind;
                    let value = *value;
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
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
        })
}

// ============================ filter tools ==================================

/// The tag filter: every known tag plus a clear entry. Selecting one browses
/// assets carrying it (the whole subtree, via `active_tag`).
pub(super) fn tag_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
    let active = controller.read(cx).active_tag;
    let tags: Vec<(Uuid, String)> = {
        let conn = controller.read(cx).library.store().conn();
        trove_core::store::tags::list(conn)
            .unwrap_or_default()
            .into_iter()
            .map(|t| (t.id, t.name))
            .collect()
    };
    let t = |k: &str| rust_i18n::t!(k).to_string();

    Button::new("filter-tag")
        .ghost()
        .xsmall()
        .icon(IconName::Frame)
        .label(t("workspace.filter_tag"))
        .selected(active.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(160.));
                menu = menu.item(
                    PopupMenuItem::new(t("workspace.filter_all_tags")).checked(active.is_none()),
                );
                for (id, name) in &tags {
                    let checked = active == Some(*id);
                    let (id, name) = (*id, name.clone());
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                        move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                ctl.select_tag(Some(id));
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            }
        })
}

/// The shape filter: image orientation (landscape / portrait / square),
/// derived from width vs height. Assets without dimensions match nothing.
pub(super) fn shape_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
    use trove_core::model::Orientation;
    let current = controller.read(cx).filter_orientation;
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let options: Vec<(Option<Orientation>, String)> = vec![
        (None, t("workspace.filter_all_shapes")),
        (Some(Orientation::Landscape), t("workspace.shape_landscape")),
        (Some(Orientation::Portrait), t("workspace.shape_portrait")),
        (Some(Orientation::Square), t("workspace.shape_square")),
    ];
    Button::new("filter-shape")
        .ghost()
        .xsmall()
        .icon(IconName::Maximize)
        .label(t("workspace.filter_shape"))
        .selected(current.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(150.));
                for (value, label) in &options {
                    let checked = *value == current;
                    let value = *value;
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                        move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                ctl.set_filter_orientation(value);
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            }
        })
}

/// The rating filter: minimum star rating (unrated assets match nothing).
pub(super) fn rating_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
    let current = controller.read(cx).filter_min_rating;
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let mut options: Vec<(Option<u8>, String)> = vec![(None, t("workspace.filter_all_ratings"))];
    for stars in 1..=5u8 {
        options.push((Some(stars), format!("★ {}+", stars)));
    }
    let label = match current {
        Some(n) => format!("★ {}+", n),
        None => t("workspace.filter_rating"),
    };
    Button::new("filter-rating")
        .ghost()
        .xsmall()
        .icon(IconName::Star)
        .label(label)
        .selected(current.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(140.));
                for (value, option_label) in &options {
                    let checked = *value == current;
                    let value = *value;
                    let controller = controller.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label.clone())
                            .checked(checked)
                            .on_click(move |_, _, cx| {
                                controller.update(cx, |ctl, cx| {
                                    ctl.set_filter_min_rating(value);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            }
        })
}

/// The format filter: distinct file extensions among live assets.
///
/// `exts` is the caller's cached list — reading it means a full scan of the
/// live rows (`DISTINCT LOWER(ext)` cannot use `idx_assets_ext`), which is
/// 32 ms on a 100k library and far too much to pay on every frame. It only
/// changes when assets do, so the panel caches it per controller generation.
pub(super) fn format_filter(
    exts: &[String],
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let current = controller.read(cx).filter_ext.clone();
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let options: Vec<(Option<String>, String)> =
        std::iter::once((None, t("workspace.filter_all_formats")))
            .chain(exts.iter().map(|e| {
                let label = e.to_uppercase();
                (Some(e.clone()), label)
            }))
            .collect();
    let label = current
        .as_deref()
        .map(|e| e.to_uppercase())
        .unwrap_or_else(|| t("workspace.filter_format"));
    Button::new("filter-format")
        .ghost()
        .xsmall()
        .icon(IconName::Asterisk)
        .label(label)
        .selected(current.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(140.));
                for (value, option_label) in &options {
                    let checked = *value == current;
                    let value = value.clone();
                    let controller = controller.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label.clone())
                            .checked(checked)
                            .on_click(move |_, _, cx| {
                                controller.update(cx, |ctl, cx| {
                                    ctl.set_filter_ext(value.clone());
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            }
        })
}

/// The "+" button: toggles which filter tools are visible in the toolbar
/// row. The set persists in the app config.
pub(super) fn add_filter_button(controller: &Entity<LibraryController>) -> impl IntoElement {
    let enabled: Vec<String> = AppConfig::load().filter_tools();
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let controller = controller.clone();
    Button::new("add-filter")
        .ghost()
        .xsmall()
        .icon(IconName::Plus)
        .tooltip(t("workspace.add_filter_tooltip"))
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
            let mut menu = menu.min_w(px(170.));
            for tool in trove_core::config::FILTER_TOOLS {
                let tool = tool.to_string();
                let checked = enabled.iter().any(|e| e == &tool);
                let label_key = format!("workspace.filter_tool_{tool}");
                let controller = controller.clone();
                let tool_click = tool.clone();
                menu = menu.item(PopupMenuItem::new(t(&label_key)).checked(checked).on_click(
                    move |_, _, cx| {
                        let mut config = AppConfig::load();
                        let _ = config.toggle_filter_tool(&tool_click);
                        // The toolbar row reads the config every render.
                        controller.update(cx, |_, cx| cx.notify());
                    },
                ));
            }
            menu
        })
}
