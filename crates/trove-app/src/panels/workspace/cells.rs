//! Cell rendering: one grid tile (thumbnail / live font preview /
//! kind-icon fallback) with its click, drag and context-menu
//! wiring, plus the list-view row variant.

use super::*;
use gpui_kit::base::{Align, Placement, Positioner};

/// Longest edge of the larger view a live still or specimen gets. The
/// thumbnail it shows is capped at [`trove_core::media::thumb::THUMB_MAX`], and
/// that is the ceiling this is measured against: past it the view would be
/// upscaling the cached picture rather than revealing it. 480 still clears the
/// tallest tile the zoom slider offers, so it is a genuine enlargement at every
/// zoom the grid can reach.
const LOUPE_MAX: f32 = 480.;

/// Gap kept between a tile and its larger view.
const LOUPE_GAP: f32 = 10.;

/// Deferred paint priority. Below Base's dialogs (`10 + layer`) and popups
/// (`100`), so a menu or a sheet always covers the view rather than fighting it.
const LOUPE_PRIORITY: usize = 5;

// ============================ cell rendering =================================

/// One cell thumbnail with click / drag / context-menu behavior, rendered at
/// the exact pixel size the row layout assigned to it. Selection is read live
/// from the controller. Clicking focuses the tiles so the grid keyboard
/// navigation (arrows / Delete / Enter) and the space bar apply.
pub(super) fn build_cell_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    quick_look: &Entity<LiveCard>,
    cell: &Cell,
    w: f32,
    h: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let is_sel = controller.read(cx).selected_assets.contains(&id);
    // The live card, if this is the one. Reading it is a borrow of another
    // entity on the paint path, which is exactly what selection already does —
    // and it is the only way a card can repaint on its own without rebuilding
    // the frozen row it sits in.
    let is_live = quick_look.read(cx).is_live(id);
    let picture = is_live.then(|| quick_look.read(cx).picture(id)).flatten();
    let playhead = is_live.then(|| quick_look.read(cx).playhead(id)).flatten();
    let looked = quick_look.read(cx).looked(id);

    // Fonts render live — the sample text set in the font itself, one row —
    // with the static specimen card as fallback (unparseable font / no
    // metadata).
    let live_font: Option<AnyElement> = if kind == AssetKind::Font {
        cell.font_family.as_ref().and_then(|family| {
            crate::panels::common::ensure_font_registered(family, cell.font_blob.as_deref(), cx)
                .then(|| {
                    // Subtitled specimen card (fontmatrix style): three
                    // stacked rows (Latin / CJK / digits) under the label.
                    crate::panels::common::font_specimen_card(family, cx)
                        .size_full()
                        .text_size(px(font_specimen_size(h)))
                        .into_any_element()
                })
        })
    } else {
        None
    };
    let preview: AnyElement = if let Some(live) = live_font {
        live
    } else if let Some(frame) = picture {
        // The decoded frame, not the still: this is the tile the pointer chose.
        img(ImageSource::Render(frame))
            .size_full()
            .object_fit(gpui_kit::ObjectFit::Contain)
            .into_any_element()
    } else {
        match &thumb {
            Some(path) => img(path.clone())
                .size_full()
                .object_fit(gpui_kit::ObjectFit::Contain)
                .into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Icon::new(kind_icon(kind)).size_8())
                .into_any_element(),
        }
    };
    // The larger view, and only for the one card that earned it. Its anchor is
    // the tile's box *from the last paint*, because a deferred surface is placed
    // while it is being built — before this frame has measured anything. And
    // deciding a font's view asks whether the face registered, which is a probe
    // the other hundred tiles must not pay for.
    let loupe = looked
        .then(|| quick_look.read(cx).anchor(id))
        .flatten()
        .and_then(|anchor| loupe_for(cell, anchor, cx));
    let base = div()
        .cursor_pointer()
        .flex_none()
        .w(px(w))
        .h(px(h))
        .relative()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(if is_sel {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .overflow_hidden()
        // `on_prepaint` belongs on the plain div, before the id (same contract
        // as the preview stage). The live tile measures itself so the larger view
        // has a box to hang off, and the first measurement of a tile asks for one
        // more frame — the view is built *from* the box, so the frame that finds
        // it cannot be the frame that draws it.
        .when(is_live, {
            let quick_look = quick_look.clone();
            move |cell| {
                cell.on_prepaint(move |measured: Bounds<Pixels>, window, cx| {
                    if quick_look.update(cx, |cards, _| cards.set_anchor(id, measured)) {
                        window.request_animation_frame();
                    }
                })
            }
        })
        .id(format!("cell-{id}"))
        .child(preview)
        .when_some(playhead, |cell, ratio| {
            // The card's own progress bar: a live video has no transport,
            // and this is the least intrusive thing that says where it is.
            cell.child(
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .h(px(2.))
                    .w(px(ratio.clamp(0., 1.) * w))
                    .bg(cx.theme().primary),
            )
        })
        .when_some(loupe, |cell, loupe| cell.child(loupe))
        // Not a looked-at card: a still and a specimen have no timeline for the
        // pointer to be a shuttle across, so that tile gets no move listener.
        .when(is_live && !looked, {
            let quick_look = quick_look.clone();
            move |cell| {
                cell.on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
                    let Some(anchor) = quick_look.read(cx).anchor(id) else {
                        return;
                    };
                    let ratio =
                        (f32::from(event.position.x) - f32::from(anchor.origin.x)) / w.max(1.);
                    quick_look.update(cx, |cards, cx| cards.scrubbed(id, ratio, cx));
                })
            }
        });

    let ctl_click = controller.clone();
    let id_click = id;
    let focus = focus_handle.clone();
    let base = base.on_click(move |event: &ClickEvent, window, _cx| {
        // Focus the grid so keyboard navigation applies right away.
        window.focus(&focus, _cx);
        // A double click on a model is the mouse way of saying "preview
        // this one"; the grid handles the action, and only opens the
        // viewport for a mesh.
        if kind == AssetKind::Model && event.click_count() == 2 {
            window.dispatch_action(Box::new(OpenPreview), _cx);
            return;
        }
        let m = event.modifiers();
        let multi = m.control || m.platform;
        ctl_click.update(_cx, move |ctl, _| {
            if m.shift {
                // Range select: anchor (last plain click) to this cell in
                // display order, replacing the selection.
                ctl.select_range_to(id_click);
            } else if multi {
                ctl.toggle_asset(id_click);
            } else {
                ctl.select_asset(Some(id_click));
            }
        });
    });

    // Drag source: drags the clicked asset, or the whole selection when it
    // includes this one. Borrow before copying: this runs per visible cell
    // per frame, so cloning the whole selection unconditionally would cost
    // O(visible × selection) on every render (notably during a resize).
    let ids_for_drag: Vec<Uuid> = {
        let selected = controller.read(cx).selected_assets.as_slice();
        if selected.contains(&id) {
            selected.to_vec()
        } else {
            vec![id]
        }
    };
    let base = base.on_drag(AssetsDrag(ids_for_drag), move |payload, _offset, _, cx| {
        cx.new(|_cx| AssetsDragPreview {
            count: payload.0.len(),
        })
    });

    // Drag the cell out of the window: promote the in-app drag to a native
    // file drag handed to the OS (droppable into editors, chats, file
    // managers). The real file is handed over: the blob for stored assets
    // (the receiver sees the content-hash name), the linked original for
    // linked ones. Must be registered AFTER on_drag with the same payload type.
    let base = base.external_drag_payload({
        let controller = controller.clone();
        move |_: &AssetsDrag, _, cx| {
            let path = controller.read(cx).library.asset_file(id);
            path.map(|path| {
                gpui_kit::ExternalDragPayload::Files(gpui_kit::FileDragPaths::new([(path, false)]))
            })
        }
    });

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
}

/// Font size for a specimen card of the given height: the height actually left
/// for the rows — card height minus the label strip, over the three-line
/// line-height factor. Derived from height rather than width so the zoom slider
/// keeps steering it across its whole travel and only the very top of the range
/// clamps out. Shared by the tile and by its larger view, which is what makes
/// the two differ in nothing but size.
fn font_specimen_size(height: f32) -> f32 {
    ((height - 24.) / 3.6).clamp(12., 72.)
}

/// The larger view of a live tile: the very picture the card is already painting,
/// at a size a face or a picture can be judged by.
///
/// It shows the library **thumbnail**, never the original, and that is the same
/// bargain the main-area preview strikes: a 50-megapixel original would decode
/// into hundreds of megabytes of pixels for one glance. The cached 512px JPEG is
/// at least as sharp as this box and costs nothing to get, which is why adding a
/// view here needed no decoder at all — unlike a played card, where the whole
/// mechanism is the decoding.
fn loupe_for(cell: &Cell, anchor: Bounds<Pixels>, cx: &mut App) -> Option<AnyElement> {
    // A font whose file registers shows live text; one that does not, like any
    // other kind, shows the rasterized card — which is exactly what its tile is
    // already painting, so the view enlarges the tile rather than replacing it.
    let family = cell.font_family.as_ref().filter(|family| {
        crate::panels::common::ensure_font_registered(family, cell.font_blob.as_deref(), cx)
    });
    // A font card is 512×256 in both of its forms, live and rasterized, so its
    // proportions come from the card rather than from a face with no dimensions
    // of its own; and the height is the one that puts the rows at the largest
    // size this element reaches at all (`font_specimen_size` clamps at 72), so
    // the view differs from the tile by having room for the whole sample rather
    // than by glyphs no tile could show.
    let (width, height) = if cell.kind == AssetKind::Font {
        (LOUPE_MAX, 300.)
    } else {
        let aspect = cell.aspect();
        if aspect >= 1. {
            (LOUPE_MAX, LOUPE_MAX / aspect)
        } else {
            (LOUPE_MAX * aspect, LOUPE_MAX)
        }
    };
    let content: AnyElement = if let Some(family) = family {
        crate::panels::common::font_specimen_card(family, cx)
            .size_full()
            .text_size(px(font_specimen_size(height)))
            .into_any_element()
    } else {
        let thumb = cell.thumb.clone()?;
        img(thumb)
            .size_full()
            .object_fit(gpui_kit::ObjectFit::Contain)
            .into_any_element()
    };
    let surface = div()
        .w(px(width))
        .h(px(height))
        .overflow_hidden()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .shadow_lg()
        .child(content);
    // Deferred, so the view escapes every clip its tile sits under: the cell's
    // own `overflow_hidden`, the grid area's, and the virtualized list's.
    // `Positioner` owns the rest — the preferred side, the flip to the other one
    // when it will not fit, and the clamp inside the window.
    Some(
        deferred(
            Positioner::side(anchor)
                .placement(Placement::Right)
                .align(Align::Center)
                .offset(px(LOUPE_GAP))
                .child(surface),
        )
        .with_priority(LOUPE_PRIORITY)
        .into_any_element(),
    )
}

/// One full-width info row for list view: small thumbnail (or kind icon),
/// name, kind label, size and import date, with the same click / drag /
/// context-menu behavior as the grid cells.
/// List-row lead element fallback: thumbnail when one exists, kind icon
/// otherwise.
fn list_lead_fallback(cx: &App, kind: AssetKind, thumb: Option<&PathBuf>) -> AnyElement {
    match thumb {
        Some(path) => img(path.clone())
            .w(px(60.))
            .h(px(36.))
            .object_fit(gpui_kit::ObjectFit::Cover)
            .rounded(cx.theme().radius)
            .into_any_element(),
        None => div()
            .w(px(60.))
            .h(px(36.))
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .bg(cx.theme().secondary)
            .child(Icon::new(kind_icon(kind)).size_5())
            .into_any_element(),
    }
}

/// The row's name as one text layout with the search hits coloured, or `None`
/// when nothing is marked — a plain string is the cheaper element, and most
/// rows in a library that is not being searched are unmarked.
///
/// One element rather than a flex row of spans: the name truncates with an
/// ellipsis, and spans laid out side by side are each measured on their own, so
/// the cut lands in the wrong place as soon as a marked word is the one that
/// runs off.
fn marked(name: &str, ranges: &[std::ops::Range<usize>], color: gpui::Hsla) -> Option<AnyElement> {
    if ranges.is_empty() {
        return None;
    }
    let highlights = ranges.iter().map(|range| {
        (
            range.clone(),
            gpui::HighlightStyle {
                color: Some(color),
                ..Default::default()
            },
        )
    });
    Some(
        gpui::StyledText::new(name)
            .with_highlights(highlights)
            .into_any_element(),
    )
}

pub(super) fn build_list_row_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    cell: &Cell,
    w: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let (name, size, added) = (cell.name.clone(), cell.size_bytes, cell.added.clone());
    // A visual search's rank, as a percentage. `None` for every other view,
    // which is what keeps the column from being an empty gutter in a browse.
    let score = cell.score.map(|value| format!("{:.0}%", value * 100.0));
    let is_sel = controller.read(cx).selected_assets.contains(&id);

    let lead: AnyElement = if kind == AssetKind::Font
        && let Some(family) = cell.font_family.as_ref()
        && crate::panels::common::ensure_font_registered(family, cell.font_blob.as_deref(), cx)
    {
        crate::panels::common::font_live_preview(family, cx)
            .w(px(60.))
            .h(px(36.))
            .text_size(px(18.))
            .rounded(cx.theme().radius)
            .into_any_element()
    } else {
        list_lead_fallback(cx, kind, thumb.as_ref())
    };

    let base = div()
        .id(format!("row-{id}"))
        .cursor_pointer()
        .w(px(w))
        .h(px(LIST_ROW_HEIGHT))
        .px_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(if is_sel {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .child(
            h_flex()
                .w_full()
                .h_full()
                .items_center()
                .gap_3()
                .child(lead)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        // A search hit marks the bytes the query actually named,
                        // in the accent colour and nothing heavier: bold would
                        // change the metrics of a row that truncates.
                        .child(
                            match marked(&cell.name, &cell.name_marks, cx.theme().primary) {
                                Some(text) => text,
                                None => name.into_any_element(),
                            },
                        ),
                )
                .child(
                    div()
                        .w(px(64.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!(kind_key(kind)).to_string()),
                )
                // The similarity column only exists in a visual search, where
                // every row has a score; in a browse it is nothing at all, not
                // an empty gutter.
                .children(score.map(|value| {
                    div()
                        .w(px(48.))
                        .text_right()
                        .text_xs()
                        .text_color(cx.theme().primary)
                        .child(value)
                        .into_any_element()
                }))
                .child(
                    div()
                        .w(px(80.))
                        .text_right()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::panels::common::human_bytes(size)),
                )
                .child(
                    div()
                        .w(px(110.))
                        .text_right()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(added),
                ),
        );

    let ctl_click = controller.clone();
    let focus = focus_handle.clone();
    let base = base.on_click(move |event: &ClickEvent, window, _cx| {
        window.focus(&focus, _cx);
        // A double click on a model previews it (see the grid cell).
        if kind == AssetKind::Model && event.click_count() == 2 {
            window.dispatch_action(Box::new(OpenPreview), _cx);
            return;
        }
        let m = event.modifiers();
        let multi = m.control || m.platform;
        ctl_click.update(_cx, move |ctl, _| {
            if m.shift {
                ctl.select_range_to(id);
            } else if multi {
                ctl.toggle_asset(id);
            } else {
                ctl.select_asset(Some(id));
            }
        });
    });

    // Same per-frame cost note as the grid cell drag source above.
    let ids_for_drag: Vec<Uuid> = {
        let selected = controller.read(cx).selected_assets.as_slice();
        if selected.contains(&id) {
            selected.to_vec()
        } else {
            vec![id]
        }
    };
    let base = base.on_drag(AssetsDrag(ids_for_drag), move |payload, _offset, _, cx| {
        cx.new(|_cx| AssetsDragPreview {
            count: payload.0.len(),
        })
    });

    // Drag the cell out of the window: promote the in-app drag to a native
    // file drag handed to the OS (droppable into editors, chats, file
    // managers). The real file is handed over: the blob for stored assets
    // (the receiver sees the content-hash name), the linked original for
    // linked ones. Must be registered AFTER on_drag with the same payload type.
    let base = base.external_drag_payload({
        let controller = controller.clone();
        move |_: &AssetsDrag, _, cx| {
            let path = controller.read(cx).library.asset_file(id);
            path.map(|path| {
                gpui_kit::ExternalDragPayload::Files(gpui_kit::FileDragPaths::new([(path, false)]))
            })
        }
    });

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
}

// asset_context_menu, open_image_search and AssetsDragPreview moved to
// workspace_context_menu.rs / workspace_search.rs

/// The file a `Model` asset's geometry lives in, with the name to show for it.
///
/// `None` for any other kind, for an asset the library no longer has, and for
/// a linked model whose source has gone missing — in every one of those cases
/// the caller falls back to the full-size asset preview.
pub(super) fn model_source(controller: &LibraryController, id: Uuid) -> Option<(String, PathBuf)> {
    use trove_core::model::Origin;

    let root = controller.library.root().to_path_buf();
    let conn = controller.library.store().conn();
    let asset = trove_core::store::assets::get(conn, id).ok().flatten()?;
    if asset.kind != AssetKind::Model {
        return None;
    }
    // Imported models live in the library as a blob; linked ones stay where
    // they are and are read in place.
    let path = match asset.origin {
        Origin::Linked => asset.facts.source_path.as_deref()?.into(),
        _ => root.join(asset.rel_path.as_ref()?),
    };
    path.is_file().then(|| (display_name(&asset), path))
}
