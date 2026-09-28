//! Title-bar filter controls (view toggle, sort, favorites) and the
//! in-panel filter tools (kind / tag / shape / rating / format / +).

use gpui_kit::base::h_flex;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{IconName, Selectable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::{Anchor, App};

use trove_core::config::{AppConfig, FILTER_TOOLS};
use trove_core::model::{AspectPreset, AssetKind, AssetSort, Orientation, ResolutionBand};
use trove_core::store::facets::{FacetCounts, FacetValue};

use crate::components::controls::icon_button;
use crate::library::{LibraryController, ViewMode};
use uuid::Uuid;

// ======================== title-bar controls ================================

/// Type + favorites grid filters for the title bar: a kind dropdown, a
/// heart toggle and a clear button when anything is active. The filters
/// compose with every view (collection, search, smart collection) and are
/// also how the favorites view is entered.
/// The icon cluster for the panel title bar: view toggle, sort, favorites.
pub(crate) fn title_controls(controller: &Entity<LibraryController>, cx: &App) -> Div {
    let (kind, favorite, view_mode, sort, sort_desc, offer_favorites) = {
        let ctl = controller.read(cx);
        (
            ctl.filter_kind,
            ctl.filter_favorite,
            ctl.view_mode,
            ctl.sort,
            ctl.sort_desc,
            // The favourites toggle belongs to the plain listings. The trash
            // and the recent list are not places anyone curates from, so the
            // control is left out there rather than offering a filter that is
            // never reached for in either.
            !ctl.showing_trash && !ctl.showing_recent,
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
        icon_button("view-toggle", toggle_icon, t(toggle_tip)).on_click({
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
        icon_button(
            "sort-menu",
            if sort_desc {
                IconName::SortDescending
            } else {
                IconName::SortAscending
            },
            t("workspace.sort"),
        )
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(170.));
                for (value, desc, label) in &sort_options {
                    let checked = *value == sort && *desc == sort_desc;
                    let (value, desc) = (*value, *desc);
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
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
    // Absent in the trash and the recent list — see `offer_favorites`.
    if offer_favorites {
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
    }

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

/// Look up the count for a value in a facet slice. Returns `None` when the
/// facet data is absent (the listing has not been counted yet) or when the
/// value does not appear (zero assets carry it).
fn facet_count(facets: Option<&[FacetValue]>, value: &str) -> Option<u64> {
    facets.and_then(|fvs| fvs.iter().find(|fv| fv.value == value).map(|fv| fv.count))
}

/// The DB string for each kind, matching `kind_str` in `store/assets.rs`.
/// Facet values are stored as these lowercase English labels.
fn kind_db_key(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Image => "image",
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Document => "document",
        AssetKind::Archive => "archive",
        AssetKind::Font => "font",
        AssetKind::Model => "model",
        AssetKind::Other => "other",
    }
}

/// Append a facet count to a label when the data is available: `"PNG" → "PNG (42)"`.
fn with_count(label: &str, count: Option<u64>) -> String {
    match count {
        Some(n) => format!("{label} ({n})"),
        None => label.to_string(),
    }
}

/// The kind dropdown for the in-panel toolbar row.
pub(crate) fn kind_filter(
    facets: Option<&FacetCounts>,
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let kind = controller.read(cx).filter_kind;
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let kind_facets = facets.map(|f| f.kinds.as_slice());

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
                .map(|k| {
                    let label = t(kind_key(k));
                    let counted = with_count(&label, facet_count(kind_facets, kind_db_key(k)));
                    (Some(k), counted)
                }),
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
pub(crate) fn tag_filter(
    facets: Option<&FacetCounts>,
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let active = controller.read(cx).active_tag;
    let tags: Vec<(Uuid, String)> = {
        controller
            .read(cx)
            .library
            .list_tags()
            .unwrap_or_default()
            .into_iter()
            .map(|t| (t.id, t.name))
            .collect()
    };
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let tag_facets = facets.map(|f| f.tags.as_slice());

    // Pre-compute labels with counts before the closure so the facet borrow
    // does not need to escape the function.
    let options: Vec<(Uuid, String)> = tags
        .iter()
        .map(|(id, name)| {
            let counted = with_count(name, facet_count(tag_facets, name));
            (*id, counted)
        })
        .collect();

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
                for (id, label) in &options {
                    let checked = active == Some(*id);
                    let (id, label) = (*id, label.clone());
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
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

/// The shape filter: coarse orientation plus media aspect-ratio presets
/// (WeChat cover, 4:3 photo, …) in one single-choice menu. The controller
/// keeps the two mutually exclusive; "all shapes" clears both.
pub(crate) fn shape_filter(
    facets: Option<&FacetCounts>,
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let (orientation, aspect) = {
        let ctl = controller.read(cx);
        (ctl.filter_orientation, ctl.filter_aspect)
    };
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let orient_facets = facets.map(|f| f.orientations.as_slice());

    let shape_options: Vec<(Option<Orientation>, String)> = vec![
        (
            Some(Orientation::Landscape),
            with_count(
                &t("workspace.shape_landscape"),
                facet_count(orient_facets, "landscape"),
            ),
        ),
        (
            Some(Orientation::Portrait),
            with_count(
                &t("workspace.shape_portrait"),
                facet_count(orient_facets, "portrait"),
            ),
        ),
        (
            Some(Orientation::Square),
            with_count(
                &t("workspace.shape_square"),
                facet_count(orient_facets, "square"),
            ),
        ),
    ];
    let aspect_presets = [
        (AspectPreset::WechatCover, "workspace.aspect_wechat_cover"),
        (AspectPreset::VideoWide, "workspace.aspect_video_wide"),
        (
            AspectPreset::VideoVertical,
            "workspace.aspect_video_vertical",
        ),
        (
            AspectPreset::PhotoLandscape,
            "workspace.aspect_photo_landscape",
        ),
        (
            AspectPreset::PhotoPortrait,
            "workspace.aspect_photo_portrait",
        ),
        (AspectPreset::Square, "workspace.aspect_square"),
    ];
    Button::new("filter-shape")
        .ghost()
        .xsmall()
        .icon(IconName::Maximize)
        .label(t("workspace.filter_shape"))
        .selected(orientation.is_some() || aspect.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(180.));
                // "All shapes" clears both shape filters.
                let all_clear = orientation.is_none() && aspect.is_none();
                let all_controller = controller.clone();
                menu = menu.item(
                    PopupMenuItem::new(t("workspace.filter_all_shapes"))
                        .checked(all_clear)
                        .on_click(move |_, _, cx| {
                            all_controller.update(cx, |ctl, cx| {
                                ctl.set_filter_orientation(None);
                                ctl.set_filter_aspect(None);
                                cx.notify();
                            });
                        }),
                );
                for (value, label) in &shape_options {
                    let checked = *value == orientation;
                    let value = *value;
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked).on_click(
                        move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                ctl.set_filter_orientation(value);
                                // A concrete shape replaces a preset; "all
                                // shapes" also drops the preset.
                                if value.is_none() {
                                    ctl.set_filter_aspect(None);
                                }
                                cx.notify();
                            });
                        },
                    ));
                }
                menu = menu.separator();
                menu = menu.item(PopupMenuItem::label(t("workspace.aspect_section")));
                for (preset, key) in &aspect_presets {
                    let checked = aspect == Some(*preset);
                    let preset = *preset;
                    let label = t(key);
                    let controller = controller.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            controller.update(cx, |ctl, cx| {
                                // The setter clears the orientation filter
                                // (a preset supersedes a coarse shape).
                                ctl.set_filter_aspect(Some(preset));
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            }
        })
}

/// The short token a band is called on the button: `1K` / `2K` / `4K`. Locale
/// text would be noise here — the three names are the same everywhere — and
/// the ranges belong in the menu, where there is room for them.
fn band_token(band: ResolutionBand) -> &'static str {
    match band {
        ResolutionBand::OneK => "1K",
        ResolutionBand::TwoK => "2K",
        ResolutionBand::FourK => "4K",
    }
}

/// The resolution filter: the longer edge banded into 1K / 2K / 4K.
///
/// Its own button rather than a third section of the shape menu, because the
/// two answer different questions and *compose*: shape compares proportions and
/// says nothing about size, so a 1920×1080 frame and a 7680×4320 one are the
/// same shape, and "4K and 16:9" is a meaningful pair to ask for at once.
pub(crate) fn resolution_filter(
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let current = controller.read(cx).filter_resolution;
    let t = |k: &str| rust_i18n::t!(k).to_string();

    let options: Vec<(Option<ResolutionBand>, String)> = vec![
        (None, t("workspace.filter_all_resolutions")),
        (Some(ResolutionBand::OneK), t("workspace.resolution_1k")),
        (Some(ResolutionBand::TwoK), t("workspace.resolution_2k")),
        (Some(ResolutionBand::FourK), t("workspace.resolution_4k")),
    ];
    let label = match current {
        Some(band) => band_token(band).to_string(),
        None => t("workspace.filter_resolution"),
    };

    Button::new("filter-resolution")
        .ghost()
        .xsmall()
        .icon(IconName::ResizeCorner)
        .label(label)
        .selected(current.is_some())
        .dropdown_menu_with_anchor(Anchor::TopLeft, {
            let controller = controller.clone();
            move |menu, _, _| {
                let mut menu = menu.min_w(px(180.));
                for (value, option_label) in &options {
                    let checked = *value == current;
                    let value = *value;
                    let controller = controller.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option_label.clone())
                            .checked(checked)
                            .on_click(move |_, _, cx| {
                                controller.update(cx, |ctl, cx| {
                                    // A band is its own filter: it neither sets
                                    // nor clears the two shape filters.
                                    ctl.set_filter_resolution(value);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            }
        })
}

/// The rating filter.
pub(crate) fn rating_filter(
    facets: Option<&FacetCounts>,
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let current = controller.read(cx).filter_min_rating;
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let rating_facets = facets.map(|f| f.ratings.as_slice());

    let mut options: Vec<(Option<u8>, String)> = vec![(None, t("workspace.filter_all_ratings"))];
    for stars in 1..=5u8 {
        let label = format!("★ {}+", stars);
        // Sum counts for all ratings >= this threshold.
        let count = rating_facets.map(|fvs| {
            fvs.iter()
                .filter_map(|fv| {
                    let n = fv.value.trim_end_matches('★').parse::<u8>().ok()?;
                    if n >= stars { Some(fv.count) } else { None }
                })
                .sum::<u64>()
        });
        options.push((Some(stars), with_count(&label, count)));
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
    facets: Option<&FacetCounts>,
    controller: &Entity<LibraryController>,
    cx: &App,
) -> impl IntoElement {
    let current = controller.read(cx).filter_ext.clone();
    let t = |k: &str| rust_i18n::t!(k).to_string();
    let ext_facets = facets.map(|f| f.exts.as_slice());

    let options: Vec<(Option<String>, String)> =
        std::iter::once((None, t("workspace.filter_all_formats")))
            .chain(exts.iter().map(|e| {
                let label = e.to_uppercase();
                let counted = with_count(&label, facet_count(ext_facets, e));
                (Some(e.clone()), counted)
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
    icon_button(
        "add-filter",
        IconName::Plus,
        t("workspace.add_filter_tooltip"),
    )
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
