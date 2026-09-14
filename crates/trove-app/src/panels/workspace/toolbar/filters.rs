//! Title-bar filter controls (view toggle, sort, favorites) and the
//! in-panel filter tools (kind / tag / shape / rating / format / +).

use gpui_kit::base::h_flex;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{IconName, Selectable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Anchor, App};
use gpui_kit::*;

use trove_core::config::{AppConfig, FILTER_TOOLS};
use trove_core::model::{AssetKind, AssetSort, Orientation};
use trove_core::store::tags;

use crate::library::{LibraryController, ViewMode};
use uuid::Uuid;

// ======================== title-bar controls ================================

/// Type + favorites grid filters for the title bar: a kind dropdown, a
/// heart toggle and a clear button when anything is active. The filters
/// compose with every view (collection, search, smart collection) and are
/// also how the favorites view is entered.
/// The icon cluster for the panel title bar: view toggle, sort, favorites.
pub(crate) fn title_controls(controller: &Entity<LibraryController>, cx: &App) -> Div {
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

pub(crate) fn kind_key(kind: AssetKind) -> &'static str {
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

// ======================== in-panel filter tools ==============================

/// The kind dropdown for the in-panel toolbar row.
pub(crate) fn kind_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
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

/// The tag filter.
pub(crate) fn tag_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
    let active = controller.read(cx).active_tag;
    let tags: Vec<(Uuid, String)> = {
        let conn = controller.read(cx).library.store().conn();
        tags::list(conn)
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

/// The shape filter.
pub(crate) fn shape_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
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

/// The rating filter.
pub(crate) fn rating_filter(controller: &Entity<LibraryController>, cx: &App) -> impl IntoElement {
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
pub(crate) fn format_filter(
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

/// The "+" button: toggles which filter tools are visible in the toolbar row.
pub(crate) fn add_filter_button(controller: &Entity<LibraryController>) -> impl IntoElement {
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
            for tool in FILTER_TOOLS {
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
