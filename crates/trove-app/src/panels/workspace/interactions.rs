//! Panel interactions: keyboard selection movement, the main-area
//! preview lifecycle (3D viewport + full-size asset preview), trash/history
//! actions and smart-collection creation. Methods on [`WorkspacePanel`];
//! they run from the action handlers in `mod.rs` and the title-bar buttons.

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
                                    parent_id: None,
                                    name,
                                    query: json!({
                                        "op": "match",
                                        "field": "text",
                                        "value": search,
                                    }),
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
            ctl.selected_assets = Rc::new(Vec::new());
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
            ctl.selected_assets = Rc::new(Vec::new());
            ctl.generation += 1;
            cx.notify();
        });
    }

    /// Title-bar label: the name of whatever the library is currently
    /// browsed through — the active visual search, smart collection,
    /// collection (with its parent prefix when nested), trash, recently
    /// viewed, or the all-assets fallback.
    ///
    /// The smart-collection and collection names come from SQLite, and the
    /// label is rebuilt on every frame (the dock calls it from `title`, which
    /// is outside the workspace's own render). `rename_collection` /
    /// `rename_smart_collection` bump the generation, so keying the cache on
    /// it is enough to keep a rename from going stale.
    pub(super) fn title_label(&mut self, cx: &Context<Self>) -> String {
        // The whole label is cached, not just the store lookups: `title` is
        // rebuilt outside this panel's own render, and every branch below
        // allocates, so a per-frame recompute is wasted work even for the
        // branches that never reach SQLite. Everything the label depends on
        // is covered by the two counters — browse switches (trash / recent /
        // collection / smart) and renames bump the data generation, and the
        // favorites label reads the favorite filter, which bumps the filter
        // generation. The remaining filters change neither.
        let (generation, filter_generation) = {
            let ctl = self.controller.read(cx);
            (ctl.generation, ctl.filter_generation)
        };
        if let Some((cached_gen, cached_filter, label)) = &self.title_cache
            && *cached_gen == generation
            && *cached_filter == filter_generation
        {
            return label.clone();
        }

        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();

        let label = if let Some(visual) = &ctl.visual_results {
            format!(
                "{} · {}",
                rust_i18n::t!("workspace.visual_title"),
                visual.label
            )
        } else if ctl.showing_trash {
            rust_i18n::t!("app.trash").to_string()
        } else if ctl.showing_recent {
            rust_i18n::t!("app.recent_viewed").to_string()
        } else if let Some(sid) = ctl.active_smart
            && let Ok(Some(sc)) = smart_collections::get(conn, sid)
        {
            sc.name
        } else if let Some(cid) = ctl.current_collection
            && let Ok(Some(c)) = collections::get(conn, cid)
        {
            match c
                .parent_id
                .and_then(|pid| collections::get(conn, pid).ok().flatten())
            {
                Some(p) => format!("{} / {}", p.name, c.name),
                None => c.name,
            }
        } else if ctl.filter_favorite {
            // The favorites toggle turns the unfiltered "all assets" view
            // into the favorites view; named views keep their names.
            rust_i18n::t!("workspace.title_favorites").to_string()
        } else {
            rust_i18n::t!("app.all_assets").to_string()
        };
        self.title_cache = Some((generation, filter_generation, label.clone()));
        label
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

    /// Enter: preview the primary selected asset full-size in the main
    /// area. A 3D model takes over the main area with the interactive
    /// viewport; everything else shows the full-size still, live video or
    /// large font specimen from `components::preview`.
    pub(super) fn open_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.controller.read(cx).primary() else {
            return;
        };
        // A mesh is worth more than a picture of a mesh: the viewport lets it
        // be turned and zoomed, and a static picture is no way to look at one.
        if let Some((name, path)) = model_source(self.controller.read(cx), id) {
            self.open_model_preview(name, path, id, window, cx);
            return;
        }
        self.open_asset_preview(id, window, cx);
    }

    /// Show a 3D model in the main-area viewport, replacing whatever was
    /// there.
    fn open_model_preview(
        &mut self,
        name: String,
        path: PathBuf,
        asset: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tasks = self.controller.read(cx).library.tasks().clone();
        let saved = self.controller.read(cx).model_look(asset);
        let viewport = ModelViewport::spawn(name, path, tasks, Some(asset), saved, window, cx);
        let subscription = cx.subscribe(&viewport, move |this, _, event, cx| match event {
            ModelViewportEvent::Closed => this.forget_preview(cx),
            // The look is the model's own, so it lands on the asset row rather
            // than only in the app-wide config.
            ModelViewportEvent::LookChanged(look) => this
                .controller
                .update(cx, |ctl, _| ctl.remember_model_look(asset, look.clone())),
        });
        // The status bar shows which renderer is painting the model. The
        // viewport notifies on every frame, so this watcher only carries the
        // change over when the description it renders actually moved on —
        // during a drag that check runs every frame but almost never fires.
        let observer = cx.observe(&viewport, |this, viewport, cx| {
            let backend = viewport.read(cx).backend_text();
            if this.viewport_backend.as_deref() != Some(backend.as_str()) {
                this.viewport_backend = Some(backend);
                cx.notify();
            }
        });
        // Enter on another asset while one is already showing: let the old
        // preview hand its frame back before it is dropped.
        self.viewport_backend = Some(viewport.read(cx).backend_text());
        if let Some(previous) = self.preview.replace(MainPreview::Model(viewport)) {
            previous.release(window, cx);
        }
        self.preview_subscription = Some(subscription);
        self.viewport_observer = Some(observer);
        cx.notify();
    }

    /// Show any non-model asset full-size in the main area, replacing
    /// whatever was there. No-op when the asset no longer exists.
    /// `pub(super)`: the preview toolbar re-opens the same asset after a
    /// quick edit, so the edited result replaces the picture on screen.
    pub(super) fn open_asset_preview(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = AssetPreviewPanel::spawn(&self.controller, id, cx) else {
            return;
        };
        let subscription = cx.subscribe(&preview, |this, _, event: &AssetPreviewEvent, cx| {
            if *event == AssetPreviewEvent::Closed {
                this.forget_preview(cx);
            }
        });
        if let Some(previous) = self.preview.replace(MainPreview::Asset(preview)) {
            previous.release(window, cx);
        }
        self.viewport_backend = None;
        self.viewport_observer = None;
        self.preview_subscription = Some(subscription);
        cx.notify();
    }

    /// Leave the preview, giving its frame back to the window first.
    pub(super) fn dismiss_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(preview) = self.preview.take() {
            preview.release(window, cx);
            self.preview_subscription = None;
            self.viewport_backend = None;
            self.viewport_observer = None;
            cx.notify();
        }
    }

    /// Drop the panel's handle on the preview. The preview itself has
    /// already released its frame by the time it announces that it closed,
    /// so this only has to forget it.
    fn forget_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            self.preview_subscription = None;
            self.viewport_backend = None;
            self.viewport_observer = None;
            cx.notify();
        }
    }
}
