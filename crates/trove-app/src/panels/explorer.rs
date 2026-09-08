//! Collections panel: the folder tree.
//!
//! * Rows show the name on the left and the asset count on the right.
//! * "All assets" is a pseudo-collection row without a management menu.
//! * The "+" button appends an inline editor after the last row.
//! * Right-click a collection: New collection inside / Rename (the row
//!   becomes a prefilled editor) / Delete. Enter confirms inline edits.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::model::{AssetKind, NewCollection};
use trove_core::store::{assets, collections, smart_collections};
use uuid::Uuid;

use crate::library::LibraryController;

use super::common::{
    AssetsDrag, CollectionDrag, hex_to_rgb, live_count, observe_controller, separator_label,
    trash_count,
};

/// Live (non-trashed) entry count of the recently-viewed history.
fn recent_count(ctl: &LibraryController) -> u64 {
    trove_core::store::view_history::live_count(ctl.library.store().conn()).unwrap_or(0)
}

/// Live (non-trashed) font-asset count for the fonts pseudo-row.
fn fonts_count(ctl: &LibraryController) -> u64 {
    assets::query(
        ctl.library.store().conn(),
        &trove_core::model::AssetQuery {
            kind: Some(AssetKind::Font),
            ..Default::default()
        },
    )
    .map(|(total, _)| total)
    .unwrap_or(0)
}

/// What the single inline editor is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    None,
    /// New collection. `parent`: browse a folder? + adds under it; a
    /// right-click "New collection inside" sets it to that folder.
    Adding {
        parent: Option<Uuid>,
    },
    Renaming(Uuid),
    RenamingSmart(Uuid),
}

pub struct ExplorerPanel {
    focus_handle: FocusHandle,
    controller: Entity<LibraryController>,
    /// Reused inline editor for add / rename.
    editor_input: Entity<InputState>,
    mode: EditorMode,
}

impl ExplorerPanel {
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
            editor_input,
            mode: EditorMode::None,
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

    /// Open the editor, cleared and focused, for a fresh collection.
    fn open_add(&mut self, parent: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.mode = EditorMode::Adding { parent };
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// "+" clicked: create at the end, under the browsed collection when any.
    fn begin_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.controller.read(cx).current_collection;
        self.open_add(parent, window, cx);
    }

    /// Right-click → "New collection inside": immediately editable.
    fn add_inside(&mut self, parent: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.open_add(Some(parent), window, cx);
    }

    /// Right-click → Rename on a smart collection: same inline editor.
    pub fn begin_rename_smart(
        &mut self,
        id: Uuid,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.mode = EditorMode::RenamingSmart(id);
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// Right-click → Rename: the row becomes a prefilled, focused editor.
    fn begin_rename(
        &mut self,
        id: Uuid,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.mode = EditorMode::Renaming(id);
        self.focus_editor(window, cx);
        cx.notify();
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor_input.clone();
        editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Esc in the inline add/rename editor: drop the editor without
    /// committing. The input's own Escape handler propagates the key, so
    /// this fires only while the editor holds focus inside this panel.
    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == EditorMode::None {
            return;
        }
        self.mode = EditorMode::None;
        self.editor_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Enter pressed in the editor: create (with its parent) or rename.
    fn submit_editor(&mut self, cx: &mut Context<Self>) {
        let name: String = self.editor_input.read(cx).value().to_string();
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }

        match self.mode {
            EditorMode::Adding { parent } => {
                let controller = self.controller.clone();
                controller.update(cx, |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    let position = match parent {
                        Some(pid) => collections::children_of(conn, Some(pid))
                            .map(|c| c.len() as i64)
                            .unwrap_or(0),
                        None => collections::roots(conn)
                            .map(|r| r.len() as i64)
                            .unwrap_or(0),
                    };
                    if let Ok(c) = collections::create(
                        conn,
                        &NewCollection {
                            parent_id: parent,
                            name,
                            position,
                        },
                    ) {
                        ctl.select_collection(Some(c.id));
                    }
                    cx.notify();
                });
            }
            EditorMode::Renaming(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let _ = ctl.library.rename_collection(id, &name);
                    cx.notify();
                });
            }
            EditorMode::RenamingSmart(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let _ = ctl.library.rename_smart_collection(id, &name);
                    cx.notify();
                });
            }
            EditorMode::None => return,
        }
        self.mode = EditorMode::None;
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — smart collection, collection (with its parent
    /// prefix when nested), trash, recently viewed, or the all-assets
    /// fallback.
    fn title_label(&self, cx: &Context<Self>) -> String {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if ctl.showing_recent {
            return rust_i18n::t!("app.recent_viewed").to_string();
        }
        if ctl.filter_kind == Some(AssetKind::Font) {
            return rust_i18n::t!("app.fonts_view").to_string();
        }
        if let Some(sid) = ctl.active_smart
            && let Ok(Some(sc)) = smart_collections::get(conn, sid)
        {
            return sc.name;
        }
        if let Some(cid) = ctl.current_collection
            && let Ok(Some(c)) = collections::get(conn, cid)
        {
            if let Some(pid) = c.parent_id
                && let Ok(Some(p)) = collections::get(conn, pid)
            {
                return format!("{} / {}", p.name, c.name);
            }
            return c.name;
        }
        // The favorites toggle turns the "all assets" view into the
        // favorites view; named views keep their names.
        if ctl.filter_favorite {
            return rust_i18n::t!("workspace.title_favorites").to_string();
        }
        rust_i18n::t!("app.all_assets").to_string()
    }
}

impl BasePanel for ExplorerPanel {
    fn panel_name(&self) -> &'static str {
        "ExplorerPanel"
    }
    fn closable(&self, _: &App) -> bool {
        false
    }
}
impl DockPanel for ExplorerPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .text_sm()
            .font_weight(FontWeight::BOLD)
            .text_color(cx.theme().foreground)
            .child(self.title_label(cx))
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    /// "+" pinned to the trailing edge of the title bar (where the removed
    /// ellipsis menu used to sit).
    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let entity = cx.entity();
        Some(
            Button::new("add-collection-title")
                .ghost()
                .xsmall()
                .label("+")
                .tooltip(rust_i18n::t!("explorer.add_collection").to_string())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.begin_add(window, cx));
                }),
        )
    }
}
impl EventEmitter<PanelEvent> for ExplorerPanel {}
impl Focusable for ExplorerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplorerPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let explorer = cx.entity();

        // Snapshot everything the rows need so the controller borrow ends
        // before we start building elements with `&mut cx`.
        struct Row {
            id: Uuid,
            name: String,
            is_root: bool,
            count: u64,
        }
        let (
            current,
            showing_trash,
            showing_recent,
            active_smart,
            all_count,
            trash_total,
            recent_total,
            ctl_filter_kind,
            rows,
            smart_rows,
        ) = {
            let ctl = self.controller.read(cx);
            let conn = ctl.library.store().conn();
            let mut rows: Vec<Row> = Vec::new();
            if let Ok(roots) = collections::roots(conn) {
                for root in roots {
                    let count = collections::count_assets(conn, root.id).unwrap_or(0);
                    rows.push(Row {
                        id: root.id,
                        name: root.name.clone(),
                        is_root: true,
                        count,
                    });
                    if let Ok(children) = collections::children_of(conn, Some(root.id)) {
                        for child in children {
                            let count = collections::count_assets(conn, child.id).unwrap_or(0);
                            rows.push(Row {
                                id: child.id,
                                name: child.name.clone(),
                                is_root: false,
                                count,
                            });
                        }
                    }
                }
            }
            let mut smart_rows: Vec<(Uuid, String, u64, Option<u32>)> = Vec::new();
            if let Ok(list) = smart_collections::list(conn) {
                smart_rows = list
                    .into_iter()
                    .map(|sc| {
                        let accent = sc.color.as_deref().and_then(hex_to_rgb);
                        let node = trove_core::store::smart::node_from_json(&sc.query).ok();
                        let count = node
                            .as_ref()
                            .and_then(|n| {
                                trove_core::store::smart::evaluate(conn, n, None, 0)
                                    .ok()
                                    .map(|(total, _)| total)
                            })
                            .unwrap_or(0);
                        (sc.id, sc.name, count, accent)
                    })
                    .collect();
            }
            (
                ctl.current_collection,
                ctl.showing_trash,
                ctl.showing_recent,
                ctl.active_smart,
                live_count(ctl),
                trash_count(ctl),
                recent_count(ctl),
                ctl.filter_kind,
                rows,
                smart_rows,
            )
        };

        let mut items: Vec<AnyElement> = Vec::new();

        // Pseudo rows: All assets, Recently viewed, Trash. Same shape, no
        // management menu.
        let trash_selected = showing_trash;
        let recent_selected = showing_recent;
        // The fonts view reuses the all-assets query with a Font kind
        // filter, so "All assets" must not highlight in that state — only
        // the fonts row does.
        let fonts_selected = current.is_none()
            && !showing_trash
            && !showing_recent
            && ctl_filter_kind == Some(AssetKind::Font);
        let all_selected =
            current.is_none() && !showing_trash && !showing_recent && !fonts_selected;
        items.push(
            collection_row(
                cx,
                self.controller.clone(),
                None,
                rust_i18n::t!("app.all_assets").to_string(),
                all_count,
                all_selected,
                true,
            )
            .into_any_element(),
        );
        // Fonts view: every live font asset (a kind-filtered all-assets
        // browse). Grid cells render live specimen cards.
        let controller_fonts_click = self.controller.clone();
        let fonts_total = fonts_count(self.controller.read(cx));
        items.push(
            div()
                .id("collection-row-fonts")
                .cursor_pointer()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .when(fonts_selected, |this| this.bg(cx.theme().secondary))
                .on_click(move |_ev: &ClickEvent, _window, cx| {
                    controller_fonts_click.update(cx, |ctl, _| ctl.select_fonts());
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
                                .child(rust_i18n::t!("app.fonts_view").to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(fonts_total.to_string()),
                        ),
                )
                .into_any_element(),
        );
        let trash_count = trash_total;
        let controller_trash_click = self.controller.clone();
        let controller_trash = self.controller.clone();
        // Recently viewed: history count, click browses the view. Dropping
        // assets here sends them to the trash like any other view.
        let controller_recent_click = self.controller.clone();
        let controller_recent = self.controller.clone();
        items.push(
            div()
                .id("collection-row-recent")
                .cursor_pointer()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .when(recent_selected, |this| this.bg(cx.theme().secondary))
                .on_click(move |_ev: &ClickEvent, _window, cx| {
                    controller_recent_click.update(cx, |ctl, _| ctl.select_recent());
                })
                .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                .on_drop(move |payload: &AssetsDrag, _window, cx| {
                    controller_recent.update(cx, move |ctl, cx| {
                        let _ = ctl.library.trash_assets(&payload.0);
                        ctl.deselect(&payload.0);
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
                                .child(rust_i18n::t!("app.recent_viewed").to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(recent_total.to_string()),
                        ),
                )
                .into_any_element(),
        );
        items.push(
            div()
                .id("collection-row-trash")
                .cursor_pointer()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .when(trash_selected, |this| this.bg(cx.theme().secondary))
                .on_click(move |_ev: &ClickEvent, _window, cx| {
                    controller_trash_click.update(cx, |ctl, _| ctl.select_trash());
                })
                .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
                .on_drop(move |payload: &AssetsDrag, _window, cx| {
                    controller_trash.update(cx, move |ctl, cx| {
                        let _ = ctl.library.trash_assets(&payload.0);
                        ctl.deselect(&payload.0);
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
                                .child(rust_i18n::t!("app.trash").to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(trash_count.to_string()),
                        ),
                )
                .into_any_element(),
        );

        let rename_target = match mode {
            EditorMode::Renaming(id) => Some(id),
            _ => None,
        };
        let adding = matches!(mode, EditorMode::Adding { .. });

        for row in rows {
            if rename_target == Some(row.id) {
                // The renamed row itself is replaced by the inline editor.
                items.push(editor_row(&self.editor_input, px(14.)));
                continue;
            }
            items.push(
                collection_row(
                    cx,
                    self.controller.clone(),
                    Some(row.id),
                    row.name.clone(),
                    row.count,
                    current == Some(row.id) && !showing_trash,
                    row.is_root,
                )
                .context_menu({
                    let explorer = explorer.clone();
                    let id = row.id;
                    let name = row.name.clone();
                    move |menu, window, cx| {
                        collection_menu(menu, window, cx, &explorer, id, name.clone())
                    }
                })
                .into_any_element(),
            );
        }

        if adding {
            // Editor appears after the last collection row.
            items.push(editor_row(&self.editor_input, px(0.)));
        }

        // Smart collections: saved searches, activated live against the library.
        // Smart-collection section header with its own "+" (new rule editor).
        {
            let controller = self.controller.clone();
            items.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(separator_label(
                        cx,
                        rust_i18n::t!("panel.smart").to_string(),
                    ))
                    .child(
                        Button::new("add-smart-title")
                            .ghost()
                            .xsmall()
                            .label("+")
                            .tooltip(rust_i18n::t!("rules.title_new").to_string())
                            .on_click(move |_, window, cx| {
                                crate::dialogs::rules::open_rule_editor(
                                    window,
                                    cx,
                                    controller.clone(),
                                    None,
                                );
                            }),
                    )
                    .into_any_element(),
            );
        }
        for (sid, sname, count, accent) in smart_rows {
            let menu_name = sname.clone();
            let controller = self.controller.clone();
            let sid_clone = sid;
            items.push(
                div()
                    .id(format!("smart-row-{sid}"))
                    .cursor_pointer()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active_smart == Some(sid), |this| {
                        this.bg(cx.theme().secondary)
                    })
                    .on_click(move |_ev: &ClickEvent, _window, cx| {
                        controller.update(cx, |ctl, _| ctl.select_smart(Some(sid_clone)));
                    })
                    .context_menu({
                        let controller = self.controller.clone();
                        let explorer = cx.entity();
                        let menu_name = menu_name.clone();
                        move |menu, window, cx| {
                            smart_menu(
                                menu,
                                window,
                                cx,
                                &controller,
                                &explorer,
                                sid,
                                menu_name.clone(),
                            )
                        }
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
                                    .when_some(accent, |this, rgb| {
                                        this.text_color(gpui_kit::rgb(rgb))
                                    })
                                    .child(sname),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(count.to_string()),
                            ),
                    )
                    .into_any_element(),
            );
        }

        v_flex()
            .size_full()
            .gap_1()
            .p_1()
            .key_context("Explorer")
            .on_action(
                cx.listener(|this, _: &crate::app::actions::CancelEditor, window, cx| {
                    this.cancel_editor(window, cx);
                }),
            )
            .child(
                div()
                    .flex_1()
                    .child(v_flex().gap_0p5().children(items).w_full()),
            )
    }
}

/// A full-width inline editor (add or rename).
fn editor_row(editor: &Entity<InputState>, indent: Pixels) -> AnyElement {
    h_flex()
        .w_full()
        .pl(indent)
        .px_1()
        .child(Input::new(editor).small())
        .into_any_element()
}

/// A single row in the collections list. `id == None` renders the
/// non-managed "All assets" pseudo-row.
fn collection_row(
    cx: &mut Context<ExplorerPanel>,
    controller: Entity<LibraryController>,
    id: Option<Uuid>,
    name: String,
    count: u64,
    selected: bool,
    is_root: bool,
) -> Stateful<Div> {
    let row_id = id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "all".to_string());

    let mut row = div()
        .id(format!("collection-row-{row_id}"))
        .cursor_pointer()
        .w_full()
        .px_2()
        .py_1()
        .rounded(cx.theme().radius)
        .on_click({
            let controller = controller.clone();
            move |_ev: &ClickEvent, _window, cx| {
                controller.update(cx, move |ctl, _| ctl.select_collection(id));
            }
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
                        .child(name),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(count.to_string()),
                ),
        );

    if selected {
        row = row.bg(cx.theme().secondary);
    }
    if !is_root {
        row = row.pl(px(22.));
    }

    // Drop target: add dragged assets to this collection. Managed rows are
    // also drag sources (reparent) and drop targets for other collections.
    if let Some(cid) = id {
        let controller_drop = controller.clone();
        row = row
            .drag_over::<AssetsDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
            .on_drop(move |payload: &AssetsDrag, _window, cx| {
                controller_drop.update(cx, move |ctl, cx| {
                    let _ = ctl.library.add_assets_to_collection(cid, &payload.0);
                    ctl.generation += 1;
                    cx.notify();
                });
            });

        let controller_move = controller.clone();
        row = row
            .on_drag(CollectionDrag(cid), |_, _, _, cx| {
                cx.new(|_| CollectionDragPreview)
            })
            .drag_over::<CollectionDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
            .on_drop(move |payload: &CollectionDrag, _window, cx| {
                controller_move.update(cx, move |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    let position = collections::children_of(conn, Some(cid))
                        .map(|c| c.len() as i64)
                        .unwrap_or(0);
                    if let Err(e) = ctl.library.move_collection(payload.0, Some(cid), position) {
                        ctl.notice = Some(
                            rust_i18n::t!("explorer.move_failed", error = e.to_string())
                                .to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            });
    } else {
        // "All assets" row: dropping a collection here moves it back to the
        // root level.
        let controller_root = controller.clone();
        row = row
            .drag_over::<CollectionDrag>(|this, _, _, cx| this.bg(cx.theme().secondary))
            .on_drop(move |payload: &CollectionDrag, _window, cx| {
                controller_root.update(cx, move |ctl, cx| {
                    let conn = ctl.library.store().conn();
                    let position = collections::roots(conn)
                        .map(|r| r.len() as i64)
                        .unwrap_or(0);
                    if let Err(e) = ctl.library.move_collection(payload.0, None, position) {
                        ctl.notice = Some(
                            rust_i18n::t!("explorer.move_failed", error = e.to_string())
                                .to_string(),
                        );
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            });
    }

    row
}

/// Drag ghost for a dragged collection row.
struct CollectionDragPreview;
impl Render for CollectionDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().primary)
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().primary_foreground)
            .child(Icon::new(IconName::Folder).size_4())
            .child(rust_i18n::t!("explorer.drag_collection").to_string())
    }
}

/// Right-click menu for a smart collection.
fn smart_menu(
    menu: PopupMenu,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    explorer: &Entity<ExplorerPanel>,
    id: Uuid,
    name: String,
) -> PopupMenu {
    let ctl_rename = explorer.clone();
    let ctl_edit = controller.clone();
    let ctl_delete = controller.clone();
    menu.min_w(px(160.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.rename").to_string()).on_click(
                move |_, window, cx| {
                    ctl_rename.update(cx, |this, cx| {
                        this.begin_rename_smart(id, name.clone(), window, cx);
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("rules.edit_rule").to_string()).on_click(
                move |_, window, cx| {
                    let editing = ctl_edit.read(cx).library.store().conn();
                    let editing = smart_collections::get(editing, id).ok().flatten();
                    crate::dialogs::rules::open_rule_editor(window, cx, ctl_edit.clone(), editing);
                },
            ),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(
                move |_, _, cx| {
                    ctl_delete.update(cx, move |ctl, cx| {
                        let _ = ctl.library.delete_smart_collection(id);
                        if ctl.active_smart == Some(id) {
                            ctl.select_smart(None);
                        }
                        ctl.generation += 1;
                        cx.notify();
                    });
                },
            ),
        )
}

/// Right-click menu for a managed collection.
fn collection_menu(
    menu: PopupMenu,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
    explorer: &Entity<ExplorerPanel>,
    id: Uuid,
    name: String,
) -> PopupMenu {
    let explorer_new = explorer.clone();
    let explorer_rename = explorer.clone();
    let explorer_delete = explorer.clone();

    menu.min_w(px(180.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.new_collection_inside").to_string())
                .on_click(move |_, window, cx| {
                    explorer_new.update(cx, |this, cx| this.add_inside(id, window, cx));
                }),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.rename").to_string()).on_click(
                move |_, window, cx| {
                    explorer_rename.update(cx, |this, cx| {
                        this.begin_rename(id, name.clone(), window, cx);
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(
                move |_, _, cx| {
                    explorer_delete.update(cx, |this, cx| {
                        let ctl = this.controller.clone();
                        ctl.update(cx, |ctl, cx| {
                            let conn = ctl.library.store().conn();
                            let _ = collections::delete(conn, id);
                            if ctl.current_collection == Some(id) {
                                ctl.select_collection(None);
                            }
                            ctl.generation += 1;
                            cx.notify();
                        });
                        cx.notify();
                    });
                },
            ),
        )
}
