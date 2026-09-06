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
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;

use trove_core::model::NewCollection;
use trove_core::store::{collections, smart_collections};
use uuid::Uuid;

use crate::state::LibraryController;

use super::common::{live_count, observe_controller, selectable_row, separator_label, trash_count, AssetsDrag};

/// What the single inline editor is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    None,
    /// New collection. `parent`: browse a folder? + adds under it; a
    /// right-click "New collection inside" sets it to that folder.
    Adding { parent: Option<Uuid> },
    Renaming(Uuid),
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
        let editor_input = cx.new(|cx| InputState::new(window, cx).placeholder(rust_i18n::t!("explorer.name_placeholder").to_string()));
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
    fn open_add(
        &mut self,
        parent: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// Right-click → Rename: the row becomes a prefilled, focused editor.
    fn begin_rename(&mut self, id: Uuid, name: String, window: &mut Window, cx: &mut Context<Self>) {
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
                        &NewCollection { parent_id: parent, name, position },
                    ) {
                        ctl.select_collection(Some(c.id));
                    }
                    cx.notify();
                });
            }
            EditorMode::Renaming(id) => {
                self.controller.update(cx, |ctl, cx| {
                    let _ = collections::rename(ctl.library.store().conn(), id, &name);
                    cx.notify();
                });
            }
            EditorMode::None => return,
        }
        self.mode = EditorMode::None;
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — smart collection, collection (with its parent
    /// prefix when nested), trash, or the all-assets fallback.
    fn title_label(&self, cx: &Context<Self>) -> String {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if let Some(sid) = ctl.active_smart {
            if let Ok(Some(sc)) = smart_collections::get(conn, sid) {
                return sc.name;
            }
        }
        if let Some(cid) = ctl.current_collection {
            if let Ok(Some(c)) = collections::get(conn, cid) {
                if let Some(pid) = c.parent_id {
                    if let Ok(Some(p)) = collections::get(conn, pid) {
                        return format!("{} / {}", p.name, c.name);
                    }
                }
                return c.name;
            }
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
        let (current, showing_trash, active_smart, all_count, trash_total, rows, smart_rows) = {
            let ctl = self.controller.read(cx);
            let conn = ctl.library.store().conn();
            let mut rows: Vec<Row> = Vec::new();
            if let Ok(roots) = collections::roots(conn) {
                for root in roots {
                    let count = collections::count_assets(conn, root.id).unwrap_or(0);
                    rows.push(Row { id: root.id, name: root.name.clone(), is_root: true, count });
                    if let Ok(children) = collections::children_of(conn, Some(root.id)) {
                        for child in children {
                            let count = collections::count_assets(conn, child.id).unwrap_or(0);
                            rows.push(Row { id: child.id, name: child.name.clone(), is_root: false, count });
                        }
                    }
                }
            }
            let mut smart_rows: Vec<(Uuid, String)> = Vec::new();
            if let Ok(list) = smart_collections::list(conn) {
                smart_rows = list.into_iter().map(|sc| (sc.id, sc.name)).collect();
            }
            (
                ctl.current_collection,
                ctl.showing_trash,
                ctl.active_smart,
                live_count(ctl),
                trash_count(ctl),
                rows,
                smart_rows,
            )
        };

        let mut items: Vec<AnyElement> = Vec::new();

        // Pseudo rows: All assets and Trash. Same shape, no management menu.
        let trash_selected = showing_trash;
        let all_selected = current.is_none() && !showing_trash;
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
        let trash_count = trash_total;
        let controller_trash_click = self.controller.clone();
        let controller_trash = self.controller.clone();
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
        items.push(separator_label(cx, rust_i18n::t!("panel.smart").to_string()).into_any_element());
        for (sid, sname) in smart_rows {
            let controller = self.controller.clone();
            items.push(
                selectable_row(
                    cx,
                    &format!("smart-row-{sid}"),
                    sname,
                    active_smart == Some(sid),
                    px(0.),
                    Box::new(move |_ev, _window, cx| {
                        controller.update(cx, |ctl, _| ctl.select_smart(Some(sid)));
                    }),
                    Some({
                        let controller = self.controller.clone();
                        Box::new(move |menu, window, cx| {
                            smart_menu(menu, window, cx, &controller, sid)
                        })
                    }),
                )
                .into_any_element(),
            );
        }

        v_flex()
            .size_full()
            .gap_1()
            .p_1()
            .child(div().flex_1().child(v_flex().gap_0p5().children(items).w_full()))
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
    let row_id = id.map(|v| v.to_string()).unwrap_or_else(|| "all".to_string());

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

    // Drop target: add dragged assets to this collection.
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
    }

    row
}

/// Right-click menu for a smart collection.
fn smart_menu(
    menu: PopupMenu,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
    controller: &Entity<LibraryController>,
    id: Uuid,
) -> PopupMenu {
    let ctl_delete = controller.clone();
    menu.min_w(px(160.))
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(move |_, _, cx| {
                ctl_delete.update(cx, move |ctl, cx| {
                    let _ = smart_collections::delete(ctl.library.store().conn(), id);
                    if ctl.active_smart == Some(id) {
                        ctl.select_smart(None);
                    }
                    ctl.generation += 1;
                    cx.notify();
                });
            }),
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
            PopupMenuItem::new(rust_i18n::t!("explorer.new_collection_inside").to_string()).on_click(move |_, window, cx| {
                explorer_new.update(cx, |this, cx| this.add_inside(id, window, cx));
            }),
        )
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.rename").to_string()).on_click(move |_, window, cx| {
                explorer_rename.update(cx, |this, cx| {
                    this.begin_rename(id, name.clone(), window, cx);
                });
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(rust_i18n::t!("explorer.delete").to_string()).on_click(move |_, _, cx| {
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
            }),
        )
}
