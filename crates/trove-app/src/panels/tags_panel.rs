//! Tags panel: filter by tag. Click to toggle the filter, right-click for
//! filter/delete.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelEvent};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::store::tags;
use uuid::Uuid;

use crate::library::LibraryController;

use super::common::{AssetsDrag, hex_to_rgb, observe_controller};

// =========================== Tags panel ======================================

panel!(TagsPanel, rust_i18n::t!("panel.tags").to_string());

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

        // Build the hierarchy: roots first, children nested under parents
        // (sorted by name at every level).
        let children_of: std::collections::HashMap<Uuid, Vec<&trove_core::model::Tag>> = {
            let mut map: std::collections::HashMap<Uuid, Vec<&trove_core::model::Tag>> =
                Default::default();
            let mut roots: Vec<&trove_core::model::Tag> = Vec::new();
            for tag in &all_tags {
                match tag.parent_id {
                    Some(pid) => map.entry(pid).or_default().push(tag),
                    None => roots.push(tag),
                }
            }
            map.insert(Uuid::nil(), roots);
            map
        };
        fn tag_rows<'a>(
            parent: Uuid,
            children_of: &'a std::collections::HashMap<Uuid, Vec<&'a trove_core::model::Tag>>,
            depth: usize,
            out: &mut Vec<(&'a trove_core::model::Tag, usize)>,
        ) {
            if let Some(children) = children_of.get(&parent) {
                let mut children = children.clone();
                children.sort_by_key(|tag| tag.name.to_lowercase());
                for tag in children {
                    out.push((tag, depth));
                    tag_rows(tag.id, children_of, depth + 1, out);
                }
            }
        }
        let mut flat: Vec<(&trove_core::model::Tag, usize)> = Vec::new();
        tag_rows(Uuid::nil(), &children_of, 0, &mut flat);

        v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .child(
                // Section header: title on the left, "+" to create a tag on
                // the right (same dialog flow as the rename menu item).
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .pr_0p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().muted_foreground)
                            .px_1()
                            .child(rust_i18n::t!("tags.all_tags").to_string()),
                    )
                    .child(
                        Button::new("add-tag")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Plus)
                            .tooltip(rust_i18n::t!("tags.add_tag").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                open_create_dialog(window, cx, &this.controller, None);
                            })),
                    ),
            )
            .child(
                div().flex_1().min_h_0().overflow_y_scrollbar().child(
                    v_flex()
                        .gap_0p5()
                        .w_full()
                        .children(flat.into_iter().map(|(tag, depth)| {
                            let id = tag.id;
                            let count = tags::count_assets(conn, id).unwrap_or(0);
                            let color = tag.color.clone();
                            let name = tag.name.clone();
                            let name_for_menu = name.clone();
                            let controller = self.controller.clone();
                            let mut row = div()
                                .id(format!("tag-row-{id}"))
                                .ml(px(14. * depth as f32))
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
                                        .gap_1p5()
                                        .when_some(color, |row, hex| {
                                            // Small color dot when the tag has one.
                                            let rgb = hex_to_rgb(&hex);
                                            row.child(
                                                div()
                                                    .size_2()
                                                    .rounded_full()
                                                    .when_some(rgb, |dot, rgb| {
                                                        dot.bg(gpui::rgb(rgb))
                                                    }),
                                            )
                                        })
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
                                        let _ = ctl.library.tag_assets(&payload.0, id, true);
                                        ctl.generation += 1;
                                        cx.notify();
                                    });
                                });
                            let controller = self.controller.clone();
                            row.context_menu(move |menu, _window, cx| {
                                tag_context_menu(
                                    menu,
                                    _window,
                                    cx,
                                    &controller,
                                    id,
                                    name_for_menu.clone(),
                                )
                            })
                            .into_any_element()
                        })),
                ),
            )
    }
}

/// Right-click menu for a tag row: filter, rename (inline dialog), color,
/// delete.
fn tag_context_menu(
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    tag_id: Uuid,
    tag_name: String,
) -> PopupMenu {
    let ctl_filter = controller.clone();
    let ctl_del = controller.clone();
    let ctl_rename = controller.clone();
    let ctl_color = controller.clone();
    let ctl_child = controller.clone();
    let rename_name = tag_name.clone();
    let mut m = menu
        .min_w(px(160.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("tags.filter_by_tag").to_string()).on_click(
                move |_, _, cx| {
                    ctl_filter.update(cx, move |ctl, cx| {
                        if ctl.active_tag == Some(tag_id) {
                            ctl.select_tag(None);
                        } else {
                            ctl.select_tag(Some(tag_id));
                        }
                        cx.notify();
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("tags.new_child_tag").to_string()).on_click(
                move |_, window, cx| {
                    open_create_dialog(window, cx, &ctl_child, Some(tag_id));
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("tags.rename_tag").to_string()).on_click(
                move |_, window, cx| {
                    open_rename_dialog(window, cx, &ctl_rename, tag_id, rename_name.clone());
                },
            ),
        );

    // Color submenu: a preset palette plus "no color".
    let color_menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
        let mut menu = menu.min_w(px(130.));
        for hex in TAG_COLORS {
            let ctl = ctl_color.clone();
            let label = hex.to_string();
            let value = hex.to_string();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                let value = value.clone();
                ctl.update(cx, move |ctl, cx| {
                    let _ = ctl.library.set_tag_color(tag_id, Some(&value));
                    ctl.generation += 1;
                    cx.notify();
                });
            }));
        }
        let ctl_clear = ctl_color.clone();
        menu.item(
            PopupMenuItem::new(rust_i18n::t!("tags.no_color").to_string()).on_click(
                move |_, _, cx| {
                    ctl_clear.update(cx, move |ctl, cx| {
                        let _ = ctl.library.set_tag_color(tag_id, None);
                        ctl.generation += 1;
                        cx.notify();
                    });
                },
            ),
        )
    });

    m = m
        .item(PopupMenuItem::submenu(
            rust_i18n::t!("tags.color").to_string(),
            color_menu,
        ))
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("tags.delete_tag").to_string()).on_click(
                move |_, _, cx| {
                    ctl_del.update(cx, move |ctl, cx| {
                        let _ = ctl.library.delete_tag(tag_id);
                        if ctl.active_tag == Some(tag_id) {
                            ctl.select_tag(None);
                        }
                        ctl.generation += 1;
                        cx.notify();
                    });
                },
            ),
        );
    m
}

/// Preset tag colors (hex, no `#` — `set_color` normalizes).
const TAG_COLORS: [&str; 8] = [
    "#ef4444", "#f97316", "#eab308", "#22c55e", "#06b6d4", "#3b82f6", "#a855f7", "#ec4899",
];

/// Create a tag via a small modal dialog (same flow as the rename one; the
/// library deduplicates by name through `ensure_tag`).
fn open_create_dialog(
    window: &mut Window,
    cx: &mut App,
    controller: &Entity<LibraryController>,
    parent: Option<Uuid>,
) {
    let name_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
    });
    let ctl = controller.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(rust_i18n::t!("tags.create_tag").to_string())
            .width(px(340.))
            .child(Input::new(&name_input).small().appearance(true))
            .on_ok({
                let name_input = name_input.clone();
                let ctl = ctl.clone();
                move |_, _, cx| {
                    let name: String = name_input.read(cx).value().trim().to_string();
                    if !name.is_empty() {
                        ctl.update(cx, |ctl, cx| {
                            // Same name under a different parent is still the
                            // same tag (names stay globally unique); the
                            // library layer dedupes.
                            let _ = ctl.library.create_tag(&name, parent);
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }
                    true
                }
            })
    });
}

/// Rename a tag via a small modal dialog (the tags panel has no inline
/// editor row like the explorer does).
fn open_rename_dialog(
    window: &mut Window,
    cx: &mut App,
    controller: &Entity<LibraryController>,
    tag_id: Uuid,
    current_name: String,
) {
    let name_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
    });
    name_input.update(cx, |state, cx| {
        state.set_value(current_name.clone(), window, cx)
    });
    let ctl = controller.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(rust_i18n::t!("tags.rename_tag").to_string())
            .width(px(340.))
            .child(Input::new(&name_input).small().appearance(true))
            .on_ok({
                let name_input = name_input.clone();
                let ctl = ctl.clone();
                move |_, _, cx| {
                    let name: String = name_input.read(cx).value().trim().to_string();
                    if !name.is_empty() {
                        ctl.update(cx, |ctl, cx| {
                            let _ = ctl.library.rename_tag(tag_id, &name);
                            ctl.generation += 1;
                            cx.notify();
                        });
                    }
                    true
                }
            })
    });
}
