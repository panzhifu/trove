//! Panel interactions: keyboard selection movement, the 3D model
//! viewport lifecycle, trash/history actions and smart-collection
//! creation. Methods on [`WorkspacePanel`]; they run from the
//! action handlers in `mod.rs` and the title-bar buttons.

use super::*;

impl WorkspacePanel {
    /// Save the active full-text search as a smart collection. The stored
    /// query tree uses the same `fts_query` the live search runs, so the
    /// saved results match 1:1 and track future imports.
    pub(super) fn save_search_as_smart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.controller.read(cx).search_text.trim().to_string();
        if search.is_empty() {
            return;
        }
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("explorer.name_placeholder").to_string())
        });
        let ctl = self.controller.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(rust_i18n::t!("workspace.save_as_smart").to_string())
                .width(px(380.))
                .child(Input::new(&name_input).small().appearance(true))
                .on_ok({
                    let name_input = name_input.clone();
                    let ctl = ctl.clone();
                    let search = search.clone();
                    move |_, _, cx| {
                        let name: String = name_input.read(cx).value().trim().to_string();
                        if !name.is_empty() {
                            ctl.update(cx, |ctl, cx| {
                                let input = NewSmartCollection {
                                    name,
                                    query: json!({
                                        "op": "match",
                                        "field": "text",
                                        "value": search,
                                    }),
                                    color: None,
                                    position: 0,
                                };
                                match ctl.library.create_smart_collection(&input) {
                                    Ok(_) => {
                                        ctl.generation += 1;
                                        cx.notify();
                                    }
                                    Err(e) => {
                                        ctl.notice = Some(
                                            rust_i18n::t!(
                                                "workspace.smart_create_failed",
                                                error = e.to_string()
                                            )
                                            .to_string(),
                                        );
                                        cx.notify();
                                    }
                                }
                            });
                        }
                        true
                    }
                })
        });
    }

    pub(super) fn empty_trash(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            ctl.notice = match ctl.library.empty_trash() {
                Ok(n) => Some(rust_i18n::t!("workspace.trash_emptied", count = n).to_string()),
                Err(e) => Some(
                    rust_i18n::t!("workspace.trash_empty_failed", error = e.to_string())
                        .to_string(),
                ),
            };
            ctl.selected_assets.clear();
            ctl.generation += 1;
            cx.notify();
        });
    }

    /// Wipe the recently-viewed history (title-bar button of that view).
    pub(super) fn clear_view_history(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            let conn = ctl.library.store().conn();
            ctl.notice = match trove_core::store::view_history::clear(conn) {
                Ok(_) => Some(rust_i18n::t!("workspace.history_cleared").to_string()),
                Err(e) => Some(
                    rust_i18n::t!("workspace.history_clear_failed", error = e.to_string())
                        .to_string(),
                ),
            };
            ctl.selected_assets.clear();
            ctl.generation += 1;
            cx.notify();
        });
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — smart collection, collection (with its parent
    /// prefix when nested), trash, recently viewed, or the all-assets
    /// fallback.
    pub(super) fn title_label(&self, cx: &Context<Self>) -> String {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        if ctl.showing_trash {
            return rust_i18n::t!("app.trash").to_string();
        }
        if ctl.showing_recent {
            return rust_i18n::t!("app.recent_viewed").to_string();
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
        // The favorites toggle turns the unfiltered "all assets" view into
        // the favorites view; named views keep their names.
        if ctl.filter_favorite {
            return rust_i18n::t!("workspace.title_favorites").to_string();
        }
        rust_i18n::t!("app.all_assets").to_string()
    }

    // -- keyboard navigation ---------------------------------------------------

    /// Move the selection one step in `toward`, following the frozen row
    /// geometry: left/right step through the flat order (wrapping across row
    /// edges); up/down lands on the nearest column center of the adjacent
    /// row. The moved-to row is revealed in the virtualized list.
    pub(super) fn move_selection(&mut self, toward: Direction, cx: &mut Context<Self>) {
        let rows = self.rows.clone();
        if rows.is_empty() {
            return;
        }

        // Locate the primary selection within the frozen rows.
        let primary = self.controller.read(cx).primary();
        let locate = |id: Option<Uuid>| -> Option<(usize, usize)> {
            rows.iter().enumerate().find_map(|(r, row)| {
                row.cells
                    .iter()
                    .position(|cell| Some(cell.id) == id)
                    .map(|c| (r, c))
            })
        };

        // Timeline day headers hold no cells; every step has to land on a row
        // that does, or the move would silently do nothing.
        let target: (usize, usize) = match locate(primary) {
            // Nothing selected: start from the grid edge in the move's
            // direction.
            None => match toward {
                Direction::Left | Direction::Up => match prev_cell_row(&rows, rows.len() - 1) {
                    Some(last) => (last, rows[last].cells.len() - 1),
                    None => return,
                },
                Direction::Right | Direction::Down => match next_cell_row(&rows, 0) {
                    Some(first) => (first, 0),
                    None => return,
                },
            },
            Some((r, c)) => match toward {
                Direction::Left => {
                    if c > 0 {
                        (r, c - 1)
                    } else {
                        match r.checked_sub(1).and_then(|r| prev_cell_row(&rows, r)) {
                            Some(pr) => (pr, rows[pr].cells.len() - 1),
                            None => (r, c),
                        }
                    }
                }
                Direction::Right => {
                    if c + 1 < rows[r].cells.len() {
                        (r, c + 1)
                    } else {
                        match next_cell_row(&rows, r + 1) {
                            Some(nr) => (nr, 0),
                            None => (r, c),
                        }
                    }
                }
                Direction::Up | Direction::Down => {
                    let neighbour = if toward == Direction::Up {
                        r.checked_sub(1).and_then(|r| prev_cell_row(&rows, r))
                    } else {
                        next_cell_row(&rows, r + 1)
                    };
                    match neighbour {
                        None => (r, c),
                        Some(nr) => {
                            let centers = rows[r].centers();
                            let x = centers.get(c).copied().unwrap_or(0.0);
                            let best = rows[nr]
                                .centers()
                                .iter()
                                .enumerate()
                                .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
                                .map(|(i, _)| i)
                                .unwrap_or(0);
                            (nr, best)
                        }
                    }
                }
            },
        };

        let Some(cell) = rows.get(target.0).and_then(|row| row.cells.get(target.1)) else {
            return;
        };
        let (id, row_ix) = (cell.id, target.0);
        self.controller.update(cx, |ctl, cx| {
            ctl.select_asset(Some(id));
            cx.notify();
        });
        self.list_state.scroll_to_reveal_item(row_ix);
    }

    /// Enter: open a large preview of the primary selected asset. A 3D model
    /// takes over the main area with the interactive viewport; everything
    /// else opens the standalone `dialogs::preview` component.
    pub(super) fn open_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.controller.read(cx).primary() else {
            return;
        };
        // A mesh is worth more than a picture of a mesh: the viewport lets it
        // be turned and zoomed, and a dialog is no way to look at one.
        if let Some((name, path)) = model_source(self.controller.read(cx), id) {
            self.open_viewport(name, path, window, cx);
            return;
        }
        crate::dialogs::preview::open_asset_preview(&self.controller, id, window, cx);
    }

    /// Show `path` in the main-area viewport, replacing whatever was there.
    fn open_viewport(
        &mut self,
        name: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = ModelViewport::spawn(name, path, cx);
        let subscription = cx.subscribe(&viewport, |this, _, event: &ModelViewportEvent, cx| {
            if *event == ModelViewportEvent::Closed {
                this.forget_viewport(cx);
            }
        });
        // Enter on another model while one is already showing: let the old
        // viewport hand its frame back before it is dropped.
        if let Some(previous) = self.viewport.replace(viewport) {
            previous.update(cx, |viewport, _| viewport.release(window));
        }
        self.viewport_subscription = Some(subscription);
        cx.notify();
    }

    /// Leave the viewport, giving its frame back to the window first.
    pub(super) fn dismiss_viewport(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(viewport) = self.viewport.take() {
            viewport.update(cx, |viewport, _| viewport.release(window));
            self.viewport_subscription = None;
            cx.notify();
        }
    }

    /// Drop the panel's handle on the viewport. The viewport itself has
    /// already released its frame by the time it announces that it closed, so
    /// this only has to forget it.
    fn forget_viewport(&mut self, cx: &mut Context<Self>) {
        if self.viewport.take().is_some() {
            self.viewport_subscription = None;
            cx.notify();
        }
    }

    /// Select an asset and scroll the grid so its row is visible. Used by
    /// the similar-images / colour-search dialog: clicking a hit should
    /// land the user on that asset. No-op scrolling when the asset is not
    /// part of the loaded rows (it may live outside the browsed view).
    pub(super) fn reveal_asset(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let row_ix = self
            .rows
            .iter()
            .position(|row| row.cells.iter().any(|cell| cell.id == id));
        self.controller.update(cx, |ctl, cx| {
            ctl.select_asset(Some(id));
            if row_ix.is_none() {
                ctl.notice = Some(rust_i18n::t!("workspace.reveal_not_in_view").to_string());
            }
            cx.notify();
        });
        if let Some(ix) = row_ix {
            self.list_state.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }
}
