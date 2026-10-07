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
                                    query: trove_core::model::SmartNode::Match {
                                        field: trove_core::model::SmartField::Text,
                                        op: trove_core::model::SmartCompare::Eq,
                                        value: json!(search),
                                    },
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
        empty_trash_on(&self.controller, cx);
    }

    /// Wipe the recently-viewed history (title-bar button of that view).
    pub(super) fn clear_view_history(&mut self, cx: &mut Context<Self>) {
        let controller = self.controller.clone();
        controller.update(cx, |ctl, cx| {
            ctl.notice = match ctl.library.clear_view_history() {
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
            && let Ok(Some(sc)) = ctl.library.get_smart_collection(sid)
        {
            sc.name
        } else if let Some(cid) = ctl.current_collection
            && let Ok(Some(c)) = ctl.library.collection(cid)
        {
            match c
                .parent_id
                .and_then(|pid| ctl.library.collection(pid).ok().flatten())
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
        // A genuine from-nothing open: the surface plays its arrival. Steps
        // and stage round-trips do not come through here, which is the whole
        // gate — see `preview_entrance_pending`.
        self.begin_preview_entrance(cx);
        // A live card is a tile, and every tile is about to be gone: put the card
        // out rather than leave a clip playing behind a full-size preview.
        let quick_look = self.quick_look.clone();
        quick_look.update(cx, |cards, cx| cards.off(cx));
        // A mesh is worth more than a picture of a mesh: the viewport lets it
        // be turned and zoomed, and a static picture is no way to look at one.
        if let Some((name, path)) = model_source(self.controller.read(cx), id) {
            self.open_model_preview(name, path, id, window, cx);
            return;
        }
        // Only a subtitle sidecar itself opens the subtitle editor here. An
        // audio or video asset plays: Enter is its playback preview, and its
        // subtitle is reached through the asset menu or by opening the `.srt`
        // asset on its own.
        let open_subtitles = {
            let ctl = self.controller.read(cx);
            ctl.library
                .asset(id)
                .ok()
                .flatten()
                .is_some_and(|a| a.ext.eq_ignore_ascii_case("srt"))
        };
        if open_subtitles {
            self.open_subtitles(id, window, cx);
            return;
        }
        // Flatten the row layout into a plain ID list so left/right arrows
        // can step through the grid order while the preview is open.
        let asset_ids: Vec<Uuid> = self
            .rows
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.id))
            .collect();
        self.open_asset_preview(id, &asset_ids, window, cx);
    }

    /// Raise the arrival flag and arm the timer that lowers it. The render
    /// wraps the surface in its fade-and-rise only while the flag holds, and
    /// the timer takes it down once that entrance could have finished — so
    /// the wrapper (and its animation state) exists for exactly one arrival,
    /// and a later re-mount of the same surface replays nothing.
    fn begin_preview_entrance(&mut self, cx: &mut Context<Self>) {
        self.preview_entrance_pending = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(PREVIEW_ENTRANCE_TIME + std::time::Duration::from_millis(60))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_entrance_pending {
                    this.preview_entrance_pending = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The kind of the loaded cell `id`, or `None` when the grid holds no tile
    /// for it. Quick look is about a tile — it lights one up, or enlarges the
    /// picture it was already painting — so an asset outside the loaded window
    /// has nothing here to bring to life.
    pub(super) fn cell_kind(&self, id: Uuid) -> Option<AssetKind> {
        self.rows
            .iter()
            .find_map(|row| row.cells.iter().find(|cell| cell.id == id))
            .map(|cell| cell.kind)
    }

    /// Space: make the selected card live, or put it away if it already is.
    ///
    /// This is the whole trigger now. Nothing comes alive because the pointer
    /// happens to cross it, so there is no debounce to wait out, no switch to
    /// turn the surprise off, and no sound nobody asked for.
    pub(super) fn toggle_quick_look(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.controller.read(cx).primary() else {
            return;
        };
        let Some(kind) = self.cell_kind(id) else {
            return;
        };
        let (quick_look, controller) = (self.quick_look.clone(), self.controller.clone());
        quick_look.update(cx, |cards, cx| {
            cards.toggle(id, kind, &controller, cx);
        });
    }

    /// The main area is a different keyboard surface: while it holds one asset
    /// whole, the arrows belong to stepping through the preview, not to the
    /// grid. Focus goes back to the panel, which is the node that always exists
    /// — the grid area is not rendered at all while a preview is up.
    fn focus_preview_keys(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
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
        self.focus_preview_keys(window, cx);
        cx.notify();
    }

    /// Show any non-model asset full-size in the main area, replacing
    /// whatever was there. No-op when the asset no longer exists.
    /// `pub(super)`: the preview toolbar re-opens the same asset after a
    /// quick edit, so the edited result replaces the picture on screen.
    /// `asset_ids` is the flat ordered list of assets for left/right navigation;
    /// pass an empty vec to keep the existing list (e.g. after an edit refresh).
    pub(super) fn open_asset_preview(
        &mut self,
        id: Uuid,
        asset_ids: &[Uuid],
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
        if !asset_ids.is_empty() {
            self.preview_asset_ids = asset_ids.to_vec();
            self.preview_index = asset_ids.iter().position(|&x| x == id).unwrap_or(0);
        }
        self.focus_preview_keys(window, cx);
        cx.notify();
    }

    /// Show an audio/video asset's subtitle editor in the main area. Like the
    /// asset preview, it replaces whatever was on screen; unlike it, the arrow
    /// keys do not step through the grid (see [`Self::navigate_preview`]) —
    /// there is no second subtitle to step to, and leaving silently would drop
    /// unsaved edits.
    pub(super) fn open_subtitles(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = SubtitleEditor::spawn(&self.controller, id, cx) else {
            return;
        };
        let subscription = cx.subscribe(&editor, |this, _, event: &SubtitleEvent, cx| {
            if *event == SubtitleEvent::Closed {
                this.forget_preview(cx);
            }
        });
        // The title-bar controls read the editor's state (edit toggled, the
        // copy buffer), so a change there must repaint this panel too — the
        // editor's own notify only repaints its content area.
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        if let Some(previous) = self.preview.replace(MainPreview::Subtitle(editor)) {
            previous.release(window, cx);
        }
        self.viewport_backend = None;
        self.viewport_observer = None;
        self.preview_subscription = Some(subscription);
        self.preview_asset_ids.clear();
        self.preview_index = 0;
        self.focus_preview_keys(window, cx);
        cx.notify();
    }

    /// Step the preview to the next or previous asset in the frozen row
    /// order. `forward` = true moves right/down, false moves left/up.
    pub(super) fn navigate_preview(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The subtitle editor is not one of a run: there is nothing to step
        // to, and stepping would leave its edits behind.
        if matches!(self.preview, Some(MainPreview::Subtitle(_))) {
            return;
        }
        if self.preview_asset_ids.is_empty() {
            return;
        }
        let new_index = if forward {
            self.preview_index
                .saturating_add(1)
                .min(self.preview_asset_ids.len() - 1)
        } else {
            self.preview_index.saturating_sub(1)
        };
        if new_index == self.preview_index {
            return;
        }
        // Stepping replaces one preview with another in place — no arrival
        // plays, or flipping through a run would strobe.
        self.preview_entrance_pending = false;
        let id = self.preview_asset_ids[new_index];
        self.preview_index = new_index;
        // Route by kind, exactly as Enter does (see `open_preview`): a model
        // navigated onto takes the interactive viewport, not the flat still
        // of its thumbnail. The run list survives both paths — neither open
        // touches it when handed an empty slice — so stepping continues
        // across kinds in both directions.
        if let Some((name, path)) = model_source(self.controller.read(cx), id) {
            self.open_model_preview(name, path, id, window, cx);
            return;
        }
        self.open_asset_preview(id, &[], window, cx);
    }

    /// Leave the preview, giving its frame back to the window first.
    pub(super) fn dismiss_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(preview) = self.preview.take() {
            preview.release(window, cx);
            self.preview_subscription = None;
            self.preview_asset_ids.clear();
            self.preview_index = 0;
            self.viewport_backend = None;
            self.viewport_observer = None;
            // Back to the tiles: the grid is about to be rendered again, and the
            // keys that belong to it — the space bar among them — should work
            // without the user having to click a card first.
            window.focus(&self.grid_focus, cx);
            cx.notify();
        }
    }

    /// Drop the panel's handle on the preview. The preview itself has
    /// already released its frame by the time it announces that it closed,
    /// so this only has to forget it.
    fn forget_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            self.preview_subscription = None;
            self.preview_asset_ids.clear();
            self.preview_index = 0;
            self.viewport_backend = None;
            self.viewport_observer = None;
            cx.notify();
        }
    }
}

impl WorkspacePanel {
    // ============================ rubber band ============================

    /// The tiles the list has measured, in display order, each with its window
    /// rectangle as `(left, top, right, bottom)`. Rows the virtualized list has
    /// not laid out are not here — they have no position to compare against —
    /// which is what the band's edge auto-scroll exists to bring into the
    /// measurement rather than a guess at where they would be.
    fn measured_cells(&self) -> Vec<(Uuid, (f32, f32, f32, f32))> {
        let mut out = Vec::new();
        let first = self.list_state.logical_scroll_top().item_ix;
        for (ix, row) in self.rows.iter().enumerate().skip(first) {
            let Some(bounds) = self.list_state.bounds_for_item(ix) else {
                break;
            };
            // A timeline day header is a row with no cells in it.
            if row.header.is_some() {
                continue;
            }
            let left = f32::from(bounds.origin.x);
            let top = f32::from(bounds.origin.y);
            let height = f32::from(bounds.size.height);
            for (cell, (cell_left, cell_width)) in row.cells.iter().zip(row.spans()) {
                out.push((
                    cell.id,
                    (
                        left + cell_left,
                        top,
                        left + cell_left + cell_width,
                        top + height,
                    ),
                ));
            }
        }
        out
    }

    /// Which tile covers a window point, if any. A band asks this before it
    /// starts, because a press on a tile is that tile's own gesture — it is how
    /// a click selects and how a drag carries files out of the window — and gpui
    /// bubbles a mouse event innermost-first, so the grid area is never told
    /// about the press before the tile is. The one rule that cannot be argued
    /// with is not to start there.
    pub(super) fn cell_at(&self, at: Point<Pixels>) -> Option<Uuid> {
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        self.measured_cells()
            .into_iter()
            .find(|(_, (left, top, right, bottom))| {
                x >= *left && x <= *right && y >= *top && y <= *bottom
            })
            .map(|(id, _)| id)
    }

    /// The tiles a band covers, in display order — which is what makes the last
    /// one the primary rather than wherever the pointer happened to stop. A tile
    /// counts as soon as the band touches it: requiring full containment would
    /// make the band's own edge decide whether a tile is in, which reads as the
    /// gesture dropping tiles it plainly crossed.
    fn ids_in_rect(&self, rect: Bounds<Pixels>) -> Vec<Uuid> {
        let left = f32::from(rect.origin.x);
        let top = f32::from(rect.origin.y);
        let right = left + f32::from(rect.size.width);
        let bottom = top + f32::from(rect.size.height);
        self.measured_cells()
            .into_iter()
            .filter(|(_, (cell_left, cell_top, cell_right, cell_bottom))| {
                *cell_left <= right
                    && *cell_right >= left
                    && *cell_top <= bottom
                    && *cell_bottom >= top
            })
            .map(|(id, _)| id)
            .collect()
    }

    /// Open a band at a press on empty grid — or on a tile that is not
    /// selected, which lends its press to the band because it has no drag
    /// registered to claim it (see `build_cell_element`). `on_tile` says
    /// which press it was; the release treats the two differently, and the
    /// band's own work — thresholds, hit tests, the additive merge — is the
    /// same either way.
    pub(super) fn begin_marquee(
        &mut self,
        at: Point<Pixels>,
        modifiers: &Modifiers,
        on_tile: bool,
        cx: &mut Context<Self>,
    ) {
        let base = self.controller.read(cx).selected_assets.clone();
        // The band's modifiers follow the tile's own: Ctrl/Cmd (and the platform
        // key that means the same thing here) adds rather than replaces, and
        // Shift — which extends a range on a tile — adds here too, since a range
        // has no meaning across a rectangle.
        let additive = modifiers.control || modifiers.platform || modifiers.shift;
        self.marquee = Some(Marquee {
            origin: at,
            current: at,
            base,
            additive,
            dragged: false,
            on_tile,
        });
    }

    /// Move the band, re-run its hit test, write the selection.
    ///
    /// The two cost rules of a gesture that fires on every pixel the pointer
    /// travels: the controller is mutated *without* notifying, because its
    /// `observe` fan-out would wake every panel holding the library and let the
    /// inspector chase the band frame by frame; and only this panel repaints,
    /// which is what draws the tile borders and the rectangle. The rest of the
    /// dock is told once, when the band ends.
    pub(super) fn move_marquee(&mut self, to: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut band) = self.marquee.take() else {
            return;
        };
        band.current = to;
        // A press that has not travelled yet is still a press, not a gesture.
        let moved = (f32::from(to.x) - f32::from(band.origin.x)).abs()
            + (f32::from(to.y) - f32::from(band.origin.y)).abs();
        band.dragged |= moved > MARQUEE_THRESHOLD;
        let (dragged, additive, base, rect) =
            (band.dragged, band.additive, band.base.clone(), band.rect());
        self.marquee = Some(band);
        if !dragged {
            return;
        }

        let ids = merge_band_selection(&base, self.ids_in_rect(rect), additive);
        let controller = self.controller.clone();
        controller.update(cx, |ctl, _| ctl.set_selection(ids));

        // Reach past the viewport: while the pointer is inside the edge band the
        // list steps, and the tiles it reveals are hit-tested on the next frame
        // with the rectangle still where the pointer left it.
        let viewport = self.list_state.viewport_bounds();
        let top = f32::from(viewport.origin.y);
        let bottom = top + f32::from(viewport.size.height);
        let y = f32::from(to.y);
        if y > bottom - MARQUEE_EDGE {
            self.list_state.scroll_by(px(MARQUEE_SCROLL_STEP));
        } else if y < top + MARQUEE_EDGE {
            self.list_state.scroll_by(px(-MARQUEE_SCROLL_STEP));
        }
        cx.notify();
    }

    /// Close the band.
    ///
    /// A band that travelled wakes the rest of the dock once, and writes the
    /// selection once more rather than trusting what its last move left: the
    /// release can land back on the tile the press started on, that tile's
    /// click runs innermost-first — before this handler — and a plain click
    /// would otherwise write a one-tile selection over the band's work. The
    /// rectangle gets the last word.
    ///
    /// A press that never travelled is a click. On empty grid that is the one
    /// gesture every file manager agrees on — a click there drops the
    /// selection. On a tile the press was lent to the band only for the drag's
    /// sake, and the click has already spoken by now (select, toggle, range,
    /// the double click's preview); the band stays silent. Either way this is
    /// where the press is known not to have been on a *selected* tile:
    /// `cell_at` already turned those over to the tile on the way in.
    pub(super) fn end_marquee(&mut self, cx: &mut Context<Self>) {
        let Some(band) = self.marquee.take() else {
            return;
        };
        if band.dragged {
            let ids =
                merge_band_selection(&band.base, self.ids_in_rect(band.rect()), band.additive);
            self.controller.update(cx, |ctl, cx| {
                ctl.set_selection(ids);
                cx.notify();
            });
        } else if !band.on_tile && !self.controller.read(cx).selected_assets.is_empty() {
            // `clear_selection` only moves the state when there was something to
            // clear, and the fan-out is worth the same test: an empty grid clicked
            // a hundred times should not wake a hundred panel repaints.
            self.controller.update(cx, |ctl, cx| {
                ctl.clear_selection();
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Cancel the band under a live press: Escape's answer to a drag gone
    /// wrong.
    ///
    /// The selection goes back to what it was at the press — every frame of
    /// the band had been writing its own answer over it, so this is a real
    /// write, not a no-op — and the band vanishes now rather than waiting for
    /// the release. The release is still out there though, and over the tile
    /// the press started on it would read as a click; the controller absorbs
    /// that one click so an aborted gesture lands as nothing at all. The
    /// next grid mouse-down disarms the flag, so a fresh press clicks
    /// normally.
    pub(super) fn cancel_marquee(&mut self, cx: &mut Context<Self>) {
        let Some(band) = self.marquee.take() else {
            return;
        };
        let base = (*band.base).clone();
        self.controller.update(cx, |ctl, cx| {
            ctl.set_selection(base);
            ctl.suppress_next_click = true;
            cx.notify();
        });
        cx.notify();
    }
}
