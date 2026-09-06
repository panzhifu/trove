//! Tags panel: filter by tag. Click to toggle the filter, right-click for
//! filter/delete.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelEvent};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::ActiveTheme;
use gpui_kit::*;

use trove_core::store::tags;
use uuid::Uuid;

use crate::state::LibraryController;

use super::common::{AssetsDrag, observe_controller};


// =========================== Tags panel ======================================

panel!(TagsPanel, "Tags");

impl TagsPanel {
    pub fn new(cx: &mut Context<Self>, controller: Entity<LibraryController>) -> Self {
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
        };
        observe_controller(cx, &this.controller);
        this
    }
}

impl Render for TagsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();
        let active = ctl.active_tag;
        let all_tags = tags::list(conn).unwrap_or_default();

        v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .px_1()
                    .child("All tags"),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .w_full()
                            .children(all_tags.into_iter().map(|tag| {
                                let id = tag.id;
                                let count = tags::count_assets(conn, id).unwrap_or(0);
                                let controller = self.controller.clone();
                                let mut row = div()
                                    .id(format!("tag-row-{id}"))
                                    .cursor_pointer()
                                    .w_full()
                                    .px_2()
                                    .py_1()
                                    .rounded(cx.theme().radius)
                                    .on_click(move |_ev: &ClickEvent, _window, cx| {
                                        controller.update(cx, move |ctl, cx| {
                                            if ctl.active_tag == Some(id) {
                                                ctl.select_tag(None);
                                            } else {
                                                ctl.select_tag(Some(id));
                                            }
                                            cx.notify();
                                        });
                                    })
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_sm()
                                                    .text_color(cx.theme().foreground)
                                                    .child(tag.name),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(count.to_string()),
                                            ),
                                    );
                                if active == Some(id) {
                                    row = row.bg(cx.theme().secondary);
                                }
                                let ctl_tag = self.controller.clone();
                                row = row
                                    .drag_over::<AssetsDrag>(|this, _, _, cx| {
                                        this.bg(cx.theme().secondary)
                                    })
                                    .on_drop(move |payload: &AssetsDrag, _window, cx| {
                                        ctl_tag.update(cx, move |ctl, cx| {
                                            let conn = ctl.library.store().conn();
                                            for aid in &payload.0 {
                                                let _ = tags::add_to_asset(conn, *aid, id);
                                            }
                                            ctl.generation += 1;
                                            cx.notify();
                                        });
                                    });
                                let controller = self.controller.clone();
                                row.context_menu(move |menu, _window, cx| {
                                    tag_context_menu(menu, cx, &controller, id)
                                })
                                .into_any_element()
                            })),
                    ),
            )
    }
}

/// Right-click menu for a tag row.
fn tag_context_menu(
    menu: PopupMenu,
    _cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    tag_id: Uuid,
) -> PopupMenu {
    let ctl_filter = controller.clone();
    let ctl_del = controller.clone();
    menu.min_w(px(160.))
        .item(
            PopupMenuItem::new("Filter by tag").on_click(move |_, _, cx| {
                ctl_filter.update(cx, move |ctl, cx| {
                    if ctl.active_tag == Some(tag_id) {
                        ctl.select_tag(None);
                    } else {
                        ctl.select_tag(Some(tag_id));
                    }
                    cx.notify();
                });
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new("Delete tag").on_click(move |_, _, cx| {
                ctl_del.update(cx, move |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    let _ = tags::delete(conn, tag_id);
                    if ctl.active_tag == Some(tag_id) {
                        ctl.select_tag(None);
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }),
        )
}

