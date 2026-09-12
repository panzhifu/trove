//! Cell rendering: one grid tile (thumbnail / live font preview /
//! kind-icon fallback) with its click, drag and context-menu
//! wiring, plus the list-view row variant.

use super::*;

// ============================ cell rendering =================================

/// One cell thumbnail with click / drag / context-menu behavior, rendered at
/// the exact pixel size the row layout assigned to it. Selection is read live
/// from the controller. Clicking focuses the panel so grid keyboard
/// navigation (arrows / Delete / Enter) applies.
pub(super) fn build_cell_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    cell: &Cell,
    w: f32,
    h: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let is_sel = controller.read(cx).selected_assets.contains(&id);

    // Fonts render live — the sample text set in the font itself, one row —
    // with the static specimen card as fallback (unparseable font / no
    // metadata).
    let live_font: Option<AnyElement> = if kind == AssetKind::Font {
        cell.font_family.as_ref().and_then(|family| {
            crate::panels::common::ensure_font_registered(family, cell.font_blob.as_deref(), cx)
                .then(|| {
                    crate::panels::common::font_live_preview(family, cx)
                        .size_full()
                        .text_size(px((h * 0.42).clamp(16.0, 72.0)))
                        .into_any_element()
                })
        })
    } else {
        None
    };
    let preview: AnyElement = if let Some(live) = live_font {
        live
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
    let base = div()
        .id(format!("cell-{id}"))
        .cursor_pointer()
        .flex_none()
        .w(px(w))
        .h(px(h))
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(if is_sel {
            cx.theme().primary
        } else {
            cx.theme().border
        })
        .overflow_hidden()
        .child(preview);

    let ctl_click = controller.clone();
    let id_click = id;
    let focus = focus_handle.clone();
    let base = base.on_click(move |event: &ClickEvent, window, _cx| {
        // Focus the grid so keyboard navigation applies right away.
        window.focus(&focus, _cx);
        // A double click on a model is the mouse way of saying "preview this
        // one"; the grid handles the action, and only opens the viewport for
        // a mesh.
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
    // managers). Stored assets first get their named working copy
    // (open_with), so the receiver sees `photo.jpg`, not a content hash.
    // Must be registered AFTER on_drag with the same payload type.
    let base = base.external_drag_payload({
        let controller = controller.clone();
        move |_: &AssetsDrag, _, cx| {
            let target = {
                let ctl = controller.read(cx);
                crate::library::open_with::target(ctl, id)
            };
            target.and_then(|target| {
                crate::library::open_with::publish(&target).ok()?;
                Some(gpui_kit::ExternalDragPayload::Files(
                    gpui_kit::FileDragPaths::new([(target.path, false)]),
                ))
            })
        }
    });

    let ctl_menu = controller.clone();
    base.context_menu(move |menu, window, cx| {
        asset_context_menu(menu, window, cx, &ctl_menu, id, trashed)
    })
    .into_any_element()
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

pub(super) fn build_list_row_element(
    cx: &mut App,
    controller: &Entity<LibraryController>,
    focus_handle: &FocusHandle,
    cell: &Cell,
    w: f32,
) -> AnyElement {
    let (kind, thumb, id, trashed) = (cell.kind, cell.thumb.clone(), cell.id, cell.trashed);
    let (name, size, added) = (cell.name.clone(), cell.size_bytes, cell.added.clone());
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
                        .child(name),
                )
                .child(
                    div()
                        .w(px(64.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!(kind_key(kind)).to_string()),
                )
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
        // A double click on a model is the mouse way of saying "preview this
        // one"; the grid handles the action, and only opens the viewport for
        // a mesh.
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
    // managers). Stored assets first get their named working copy
    // (open_with), so the receiver sees `photo.jpg`, not a content hash.
    // Must be registered AFTER on_drag with the same payload type.
    let base = base.external_drag_payload({
        let controller = controller.clone();
        move |_: &AssetsDrag, _, cx| {
            let target = {
                let ctl = controller.read(cx);
                crate::library::open_with::target(ctl, id)
            };
            target.and_then(|target| {
                crate::library::open_with::publish(&target).ok()?;
                Some(gpui_kit::ExternalDragPayload::Files(
                    gpui_kit::FileDragPaths::new([(target.path, false)]),
                ))
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
        Origin::Linked => PathBuf::from(asset.extra.get("source_path")?.as_str()?),
        _ => root.join(asset.rel_path.as_ref()?),
    };
    path.is_file().then(|| (display_name(&asset), path))
}
