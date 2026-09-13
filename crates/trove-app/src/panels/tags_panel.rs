//! Tags panel: filter by tag. Click to toggle the filter, right-click for
//! filter/delete.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::store::tags;
use uuid::Uuid;

use crate::library::LibraryController;

use super::common::{AssetsDrag, hex_to_rgb, observe_controller};

// =========================== Tags panel ======================================

pub struct TagsPanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    /// Parent tags whose children are currently folded away. The chevron on
    /// a parent row toggles membership; the set resets per session.
    collapsed: std::collections::HashSet<Uuid>,
    /// Per-tag asset counts, keyed by the controller generation they were read
    /// at. Every row shows one, and each is a recursive subtree walk plus a
    /// `COUNT(DISTINCT …)` — which `render` must not run, because `render` runs
    /// every frame. Cached the same way `ExplorerPanel` caches its counts.
    tag_counts: Option<(u64, std::collections::HashMap<Uuid, u64>)>,
}

impl BasePanel for TagsPanel {
    fn panel_name(&self) -> &'static str {
        "TagsPanel"
    }
    fn closable(&self, _: &App) -> bool {
        false
    }
}

impl DockPanel for TagsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rust_i18n::t!("panel.tags").to_string()
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    /// "+" pinned to the trailing edge of the title bar (same form as the
    /// explorer panel's add button).
    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let entity = cx.entity();
        Some(
            Button::new("add-tag-title")
                .ghost()
                .xsmall()
                .label("+")
                .tooltip(rust_i18n::t!("tags.add_tag").to_string())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        open_create_dialog(window, cx, &this.controller, None);
                    });
                }),
        )
    }
}

impl EventEmitter<PanelEvent> for TagsPanel {}

impl Focusable for TagsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TagsPanel {
    pub fn new(cx: &mut Context<Self>, controller: Entity<LibraryController>) -> Self {
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            collapsed: Default::default(),
            tag_counts: None,
        };
        observe_controller(cx, &this.controller);
        this
    }
}

impl Render for TagsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Counts first, while `self` is still free to be mutated: the cache is
        // only refilled when the controller generation moves, which is what
        // every mutation bumps. Measured on a 100k library: one count per tag
        // is 1.9 ms, so re-running them on each of the ~30 visible rows cost
        // ~58 ms per frame before they were cached.
        let generation = self.controller.read(cx).generation;
        let counts = match &self.tag_counts {
            Some((cached, counts)) if *cached == generation => counts.clone(),
            _ => {
                let conn = self.controller.read(cx).library.store().conn();
                let counts = tags::counts_by_tag(conn).unwrap_or_default();
                self.tag_counts = Some((generation, counts.clone()));
                counts
            }
        };

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
            collapsed: &std::collections::HashSet<Uuid>,
            out: &mut Vec<(&'a trove_core::model::Tag, usize)>,
        ) {
            if let Some(children) = children_of.get(&parent) {
                let mut children = children.clone();
                children.sort_by_key(|tag| tag.name.to_lowercase());
                for tag in children {
                    out.push((tag, depth));
                    // Children of a collapsed tag stay hidden (the tag's own
                    // row still renders, with the chevron pointing right).
                    if !collapsed.contains(&tag.id) {
                        tag_rows(tag.id, children_of, depth + 1, collapsed, out);
                    }
                }
            }
        }
        let mut flat: Vec<(&trove_core::model::Tag, usize)> = Vec::new();
        tag_rows(Uuid::nil(), &children_of, 0, &self.collapsed, &mut flat);

        v_flex().size_full().p_2().gap_1().child(
            div().flex_1().min_h_0().overflow_y_scrollbar().child(
                v_flex()
                    .gap_0p5()
                    .w_full()
                    .children(flat.into_iter().map(|(tag, depth)| {
                        let id = tag.id;
                        let count = counts.get(&id).copied().unwrap_or(0);
                        let color = tag.color.clone();
                        let name = tag.name.clone();
                        let name_for_menu = name.clone();
                        let controller = self.controller.clone();
                        // Only parents with children get the fold chevron.
                        let has_children =
                            children_of.get(&id).is_some_and(|kids| !kids.is_empty());
                        let is_folded = self.collapsed.contains(&id);
                        // No explicit width: the flex column stretches the
                        // row. `w_full` here would add the indent margin
                        // on top of 100% and push the count off-panel.
                        let mut row = div()
                            .id(format!("tag-row-{id}"))
                            .ml(px(14. * depth as f32))
                            .cursor_pointer()
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
                                    .when(has_children, |row| {
                                        row.child(
                                            div()
                                                .id(format!("tag-fold-{id}"))
                                                .cursor_pointer()
                                                .flex_none()
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    // Toggle membership: remove
                                                    // when folded, insert when open.
                                                    if !this.collapsed.remove(&id) {
                                                        this.collapsed.insert(id);
                                                    }
                                                    cx.notify();
                                                }))
                                                .child(
                                                    Icon::new(if is_folded {
                                                        IconName::ChevronRight
                                                    } else {
                                                        IconName::ChevronDown
                                                    })
                                                    .size_3()
                                                    .text_color(cx.theme().muted_foreground),
                                                ),
                                        )
                                    })
                                    .when_some(color, |row, hex| {
                                        // Small color dot when the tag has one.
                                        let rgb = hex_to_rgb(&hex);
                                        row.child(
                                            div()
                                                .size_2()
                                                .rounded_full()
                                                .when_some(rgb, |dot, rgb| dot.bg(gpui::rgb(rgb))),
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
                                            .flex_none()
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
                            .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
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
