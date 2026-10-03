//! Tags panel: filter by tag. Click to toggle the filter, right-click for
//! filter/delete. The "+" (and "New child tag") appends an inline editor row
//! — the same in-list editor the collections panel uses — rather than
//! opening a modal.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use std::ops::Range;

use uuid::Uuid;

use crate::components::controls::{fold_disclosure, muted_label};
use crate::components::scrollbar::ScrollableElement as _;
use crate::library::LibraryController;

use super::common::{AssetsDrag, hex_to_rgb, observe_controller, separator_label};

// =========================== Tags panel ======================================

/// What the tags panel's single inline editor is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagEditor {
    /// No editor row is shown.
    Closed,
    /// Adding a tag: `parent` is the tag it lands under, or `None` for a
    /// root tag (the title-bar "+").
    Adding { parent: Option<Uuid> },
}

/// One row of the tags list, flattened for the virtualized list.
///
/// Everything the panel shows — the frequent section (header + rows + a
/// divider), the full tree, and the inline add editor — goes through this one
/// row stream, because `uniform_list` renders a single flat sequence.
struct TagRow {
    kind: TagRowKind,
    id: Uuid,
    name: String,
    color: Option<String>,
    count: u64,
    depth: usize,
    has_children: bool,
    active: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagRowKind {
    /// The "frequent tags" section header, drawn below the tree.
    FrequentHeader,
    Tag,
    Editor,
}

impl TagRow {
    fn header() -> Self {
        Self {
            kind: TagRowKind::FrequentHeader,
            id: Uuid::nil(),
            name: String::new(),
            color: None,
            count: 0,
            depth: 0,
            has_children: false,
            active: false,
        }
    }

    fn editor(depth: usize) -> Self {
        Self {
            kind: TagRowKind::Editor,
            depth,
            ..Self::header()
        }
    }

    fn tag(
        tag: &trove_core::model::Tag,
        depth: usize,
        count: u64,
        has_children: bool,
        active: Option<Uuid>,
    ) -> Self {
        Self {
            kind: TagRowKind::Tag,
            id: tag.id,
            name: tag.name.clone(),
            color: tag.color.clone(),
            count,
            depth,
            has_children,
            active: active == Some(tag.id),
        }
    }
}

/// One fixed height every row in the virtualized list shares (`uniform_list`
/// requires it; the first row measured decides it).
const TAG_ROW_H: f32 = 28.;

/// Left padding of a top-level row; nested rows add one step per level — the
/// same metrics the collections panel uses, so the two trees read alike.
const TAG_ROW_PAD: f32 = 8.;
const TAG_ROW_INDENT: f32 = 14.;

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
    /// Reused inline editor for the add flow, mirroring `ExplorerPanel`'s.
    editor_input: Entity<InputState>,
    /// What the inline editor is doing; `Closed` hides the row.
    editor: TagEditor,
    /// Scroll position of the virtualized row list, kept on the panel so it
    /// survives across renders.
    scroll_handle: UniformListScrollHandle,
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
                        this.begin_add(None, window, cx);
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
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let editor_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
        });
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            collapsed: Default::default(),
            tag_counts: None,
            editor_input,
            editor: TagEditor::Closed,
            scroll_handle: UniformListScrollHandle::default(),
        };
        observe_controller(cx, &this.controller);
        this.subscribe_enter(window, cx);
        this
    }

    fn subscribe_enter(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor_input.clone();
        cx.subscribe_in(&editor, window, |this, _, event, _window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_editor(cx);
            }
        })
        .detach();
    }

    /// "+" clicked (or right-click → New child tag): open the inline editor,
    /// cleared and focused, for a new tag under `parent`.
    fn begin_add(&mut self, parent: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.editor = TagEditor::Adding { parent };
        let editor = self.editor_input.clone();
        editor.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Esc in the inline editor: drop it without creating anything. The
    /// input's own Escape handler propagates the key, so this fires only
    /// while the editor holds focus inside this panel.
    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor == TagEditor::Closed {
            return;
        }
        self.editor = TagEditor::Closed;
        self.editor_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Enter in the inline editor: create the tag under its parent. The
    /// library dedupes by name, so a repeat is a no-op rather than a second
    /// row.
    fn submit_editor(&mut self, cx: &mut Context<Self>) {
        let TagEditor::Adding { parent } = self.editor else {
            return;
        };
        let name = self.editor_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.controller.update(cx, |ctl, cx| {
            let outcome = ctl.library.create_tag(&name, parent);
            ctl.report_failed("creating a tag", outcome);
            ctl.generation += 1;
            cx.notify();
        });
        self.editor = TagEditor::Closed;
    }

    /// Fold or unfold a parent tag's children.
    fn toggle_fold(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        cx.notify();
    }

    /// Render one row of the virtualized list. Every arm must produce a row
    /// of exactly [`TAG_ROW_H`], or the uniform list misplaces everything
    /// below the offender.
    fn render_row(&mut self, row: &TagRow, cx: &mut Context<Self>) -> AnyElement {
        match row.kind {
            // The section header sits in the same row stream as the rows, at
            // the shared height, and uses the collections panel's separator
            // style so the two sidebars match.
            TagRowKind::FrequentHeader => div()
                .h(px(TAG_ROW_H))
                .w_full()
                .px_2()
                .flex()
                .items_center()
                .child(separator_label(
                    cx,
                    rust_i18n::t!("tags.frequent").to_string(),
                ))
                .into_any_element(),
            TagRowKind::Editor => h_flex()
                .h(px(TAG_ROW_H))
                .w_full()
                .pl(px(TAG_ROW_PAD + TAG_ROW_INDENT * row.depth as f32))
                .pr_2()
                .items_center()
                .child(Input::new(&self.editor_input).small())
                .into_any_element(),
            TagRowKind::Tag => self.render_tag_row(row, cx),
        }
    }

    /// One tag row (shared by the frequent section and the full tree).
    ///
    /// Interaction follows the app-wide tree contract: a single click selects
    /// (here: toggles the tag filter), a double click folds or unfolds a
    /// parent's subtree — and the leading disclosure makes a parent visible
    /// at a glance, folding on a single click of its own.
    fn render_tag_row(&mut self, row: &TagRow, cx: &mut Context<Self>) -> AnyElement {
        let id = row.id;
        let has_children = row.has_children;
        let expanded = !self.collapsed.contains(&id);
        let color = row.color.clone();
        let name = row.name.clone();
        let name_for_menu = name.clone();
        let controller = self.controller.clone();

        let mut el = div()
            .id(format!("tag-row-{id}"))
            .h(px(TAG_ROW_H))
            .w_full()
            .cursor_pointer()
            .px_2()
            .rounded(cx.theme().radius)
            // The row stretches to the panel edge; the indent is padding, not
            // a margin, so the count keeps its column — the same scheme the
            // collections panel uses.
            .when(row.depth > 0, |el| {
                el.pl(px(TAG_ROW_PAD + TAG_ROW_INDENT * row.depth as f32))
            })
            .when(row.active, |el| el.bg(cx.theme().secondary))
            .on_click(cx.listener(move |this, ev: &ClickEvent, _window, cx| {
                if has_children && ev.click_count() >= 2 {
                    // Double click: fold/unfold the subtree.
                    this.toggle_fold(id, cx);
                } else {
                    // Single click: toggle the filter on this tag.
                    this.controller.update(cx, move |ctl, cx| {
                        if ctl.active_tag == Some(id) {
                            ctl.select_tag(None);
                        } else {
                            ctl.select_tag(Some(id));
                        }
                        cx.notify();
                    });
                }
            }))
            .child(
                h_flex()
                    .h_full()
                    .w_full()
                    .items_center()
                    .gap_2()
                    // The fold disclosure: a chevron for a parent, an empty
                    // slot for a leaf, so the names below align either way.
                    .child(fold_disclosure(
                        format!("tag-fold-{id}"),
                        has_children,
                        expanded,
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.toggle_fold(id, cx);
                        }),
                        cx,
                    ))
                    // The leading slot a collection row carries (its glyph),
                    // here the tag's colour dot, so names line up across the
                    // two sidebars whether or not the tag has a colour.
                    .child(
                        div()
                            .flex_none()
                            .size_4()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when_some(color, |slot, hex| {
                                let rgb = hex_to_rgb(&hex);
                                slot.when_some(rgb, |slot, rgb| {
                                    slot.child(div().size_2().rounded_full().bg(gpui::rgb(rgb)))
                                })
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(name),
                    )
                    .child(muted_label(row.count.to_string(), cx)),
            );

        let ctl_tag = controller.clone();
        el = el
            .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
            .on_drop(move |payload: &AssetsDrag, _window, cx| {
                ctl_tag.update(cx, move |ctl, cx| {
                    let outcome = ctl.library.tag_assets(&payload.0, id, true);
                    ctl.report_failed("tagging dropped assets", outcome);
                    ctl.generation += 1;
                    cx.notify();
                });
            });

        // The panel itself, so "New child tag" can open its inline editor.
        let panel = cx.entity();
        el.context_menu(move |menu, window, cx| {
            tag_context_menu(
                menu,
                window,
                cx,
                &panel,
                &controller,
                id,
                name_for_menu.clone(),
                has_children,
            )
        })
        .into_any_element()
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
                let counts = self
                    .controller
                    .read(cx)
                    .library
                    .tag_counts()
                    .unwrap_or_default();
                self.tag_counts = Some((generation, counts.clone()));
                counts
            }
        };

        let ctl = self.controller.read(cx);
        let active = ctl.active_tag;
        let all_tags = ctl.library.list_tags().unwrap_or_default();

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

        // Frequent tags: the highest rows by the same recursive count the
        // rows display, flat (no nesting, no fold chevrons). Hidden entirely
        // when nothing is tagged yet.
        //
        // Roots only: the section is flat, so a child tag shown here would
        // read as a standalone tag beside the very parent it hangs under —
        // exactly what the tree below already shows it as. The counts are
        // recursive, so a parent carries its children's usage and nothing
        // genuinely frequent is lost by the filter.
        let mut frequent: Vec<(&trove_core::model::Tag, u64)> = all_tags
            .iter()
            .filter(|t| t.parent_id.is_none())
            .filter_map(|t| {
                counts
                    .get(&t.id)
                    .copied()
                    .filter(|c| *c > 0)
                    .map(|c| (t, c))
            })
            .collect();
        frequent.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.name.to_lowercase().cmp(&b.0.name.to_lowercase()))
        });
        frequent.truncate(FREQUENT_TAGS_LIMIT);

        // The inline add editor, when one is open. `Some(None)` is a new root
        // tag (the title-bar "+"); `Some(Some(id))` a child of `id`.
        let adding = match self.editor {
            TagEditor::Adding { parent } => Some(parent),
            TagEditor::Closed => None,
        };

        // Flatten everything the panel shows into one row stream — the full
        // tree, the inline add editor, and the "frequent tags" section at the
        // bottom — so a single virtualized list can render it. Every row
        // shares `TAG_ROW_H`, which is what `uniform_list` requires (it
        // measures the first row and positions the rest at that height).
        //
        // The section order matches the collections panel: the managed tree
        // first, the derived shortcut section last.
        let mut rows: Vec<TagRow> = Vec::new();
        for (tag, depth) in &flat {
            let id = tag.id;
            // A parent can be folded by a double click.
            let has_children = children_of.get(&id).is_some_and(|kids| !kids.is_empty());
            let count = counts.get(&id).copied().unwrap_or(0);
            rows.push(TagRow::tag(tag, *depth, count, has_children, active));
            // A "New child tag" editor lands directly under its parent row,
            // one indent step deeper.
            if adding == Some(Some(id)) {
                rows.push(TagRow::editor(*depth + 1));
            }
        }
        // A root "+" editor lands after the last tree row.
        if adding == Some(None) {
            rows.push(TagRow::editor(0));
        }

        // The frequent section closes the panel, under its own header. Its
        // rows are root tags, so they keep the real `has_children` — a double
        // click folds the very subtree the tree above shows.
        if !frequent.is_empty() {
            rows.push(TagRow::header());
            for (tag, count) in &frequent {
                let has_children = children_of
                    .get(&tag.id)
                    .is_some_and(|kids| !kids.is_empty());
                rows.push(TagRow::tag(tag, 0, *count, has_children, active));
            }
        }

        // `rows` is moved into the render closure; the handle is cloned so the
        // outer wrapper can keep one too.
        let row_count = rows.len();
        let scroll_handle = self.scroll_handle.clone();
        let list = uniform_list(
            "tags-rows",
            row_count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .map(|ix| this.render_row(&rows[ix], cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&scroll_handle)
        .size_full();

        v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .key_context("Tags")
            .on_action(
                cx.listener(|this, _: &crate::app::actions::Cancel, window, cx| {
                    this.cancel_editor(window, cx);
                }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .vertical_scrollbar(&scroll_handle),
            )
    }
}

/// How many tags the "frequent" section shows at most.
const FREQUENT_TAGS_LIMIT: usize = 8;

/// Right-click menu for a tag row: filter, new child tag (inline editor),
/// rename (inline dialog), color,
/// delete — plus, for a parent, deleting the whole subtree (the plain delete
/// on a parent leaves the children re-rooted, which is rarely what the click
/// meant).
#[allow(clippy::too_many_arguments)]
fn tag_context_menu(
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    panel: &Entity<TagsPanel>,
    controller: &Entity<LibraryController>,
    tag_id: Uuid,
    tag_name: String,
    has_children: bool,
) -> PopupMenu {
    let ctl_filter = controller.clone();
    let ctl_del = controller.clone();
    let ctl_rename = controller.clone();
    let ctl_color = controller.clone();
    let panel_child = panel.clone();
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
                    panel_child.update(cx, |this, cx| {
                        this.begin_add(Some(tag_id), window, cx);
                    });
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
                    let outcome = ctl.library.set_tag_color(tag_id, Some(&value));
                    ctl.report_failed("tag colour", outcome);
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
                        let outcome = ctl.library.set_tag_color(tag_id, None);
                        ctl.report_failed("tag colour cleared", outcome);
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
        .separator();
    if has_children {
        let ctl_sub = controller.clone();
        let sub_name = tag_name.clone();
        m = m.item(
            PopupMenuItem::new(rust_i18n::t!("tags.delete_subtree").to_string()).on_click(
                move |_, window, cx| {
                    let ids = ctl_sub
                        .read(cx)
                        .library
                        .tag_subtree_ids(tag_id)
                        .unwrap_or_default();
                    let children = ids.len().saturating_sub(1) as i64;
                    open_subtree_confirm(
                        window,
                        cx,
                        &ctl_sub,
                        tag_id,
                        sub_name.clone(),
                        ids,
                        children,
                    );
                },
            ),
        );
    }
    m = m.separator().item(
        PopupMenuItem::new(rust_i18n::t!("tags.delete_tag").to_string()).on_click(
            move |_, _, cx| {
                ctl_del.update(cx, move |ctl, cx| {
                    let outcome = ctl.library.delete_tag(tag_id);
                    ctl.report_failed("deleting a tag", outcome);
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

/// Rename a tag via a small modal dialog (renaming is a different act from
/// the inline add the "+" and "New child tag" use).
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
                let current_name = current_name.clone();
                move |_, window, cx| {
                    let name: String = name_input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return true;
                    }
                    // Renaming onto an existing name means merge, so that is
                    // asked once, with the numbers it changes, before the
                    // rename path runs.
                    let existing = ctl.read(cx).library.tag_by_name(&name).ok().flatten();
                    match existing {
                        Some(existing) if existing.id != tag_id => {
                            let count = ctl.read(cx).library.tag_asset_count(tag_id).unwrap_or(0);
                            open_merge_confirm(
                                window,
                                cx,
                                &ctl,
                                tag_id,
                                current_name.clone(),
                                existing.id,
                                existing.name,
                                count as i64,
                            );
                        }
                        _ => ctl.update(cx, |ctl, cx| {
                            let outcome = ctl.library.rename_tag(tag_id, &name);
                            ctl.report_failed("renaming a tag", outcome);
                            ctl.generation += 1;
                            cx.notify();
                        }),
                    }
                    true
                }
            })
    });
}

/// Confirm a rename-turned-merge: every asset carrying `source_id` is about
/// to carry `target_id` instead. The merge is undoable, so this names the
/// consequence rather than warning off an irrecoverable act.
#[allow(clippy::too_many_arguments)]
fn open_merge_confirm(
    window: &mut Window,
    cx: &mut App,
    controller: &Entity<LibraryController>,
    source_id: Uuid,
    source_name: String,
    target_id: Uuid,
    target_name: String,
    count: i64,
) {
    let ctl = controller.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let ctl = ctl.clone();
        let source = source_name.clone();
        let target = target_name.clone();
        dialog
            .title(rust_i18n::t!("tags.merge_confirm_title").to_string())
            .width(px(420.))
            .child(
                div().text_sm().p_1().child(
                    rust_i18n::t!(
                        "tags.merge_confirm_body",
                        source = source,
                        target = target,
                        count = count
                    )
                    .to_string(),
                ),
            )
            .on_ok(move |_, _, cx| {
                ctl.update(cx, |ctl, cx| {
                    let outcome = ctl.library.merge_tags(source_id, target_id);
                    ctl.report_failed("merging a tag", outcome);
                    if ctl.active_tag == Some(source_id) {
                        ctl.select_tag(None);
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
                true
            })
    });
}

/// Confirm a subtree delete: `ids` (the tag and everything under it) go with
/// every asset relation, and nothing comes back — the store has no tag-undo,
/// which is why this is a gate and not a notice.
fn open_subtree_confirm(
    window: &mut Window,
    cx: &mut App,
    controller: &Entity<LibraryController>,
    tag_id: Uuid,
    tag_name: String,
    ids: Vec<Uuid>,
    children: i64,
) {
    let ctl = controller.clone();
    // `Rc`: the dialog builder is `Fn` (it may be rebuilt), so the id list
    // has to be shared into the ok handler rather than moved through it.
    let ids = std::rc::Rc::new(ids);
    window.open_dialog(cx, move |dialog, _, _| {
        let ctl = ctl.clone();
        let ids = std::rc::Rc::clone(&ids);
        let name = tag_name.clone();
        dialog
            .title(rust_i18n::t!("tags.subtree_confirm_title").to_string())
            .width(px(420.))
            .close_button(false)
            .child(
                div().text_sm().p_1().child(
                    rust_i18n::t!("tags.subtree_confirm_body", name = name, count = children)
                        .to_string(),
                ),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("tags.delete_tag").to_string())
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                ctl.update(cx, |ctl, cx| {
                    let outcome = ctl.library.delete_tags_many(ids.as_slice());
                    ctl.report_failed("deleting tags", outcome);
                    if ids.contains(&tag_id) && ctl.active_tag.is_some() {
                        ctl.select_tag(None);
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
                true
            })
    });
}
