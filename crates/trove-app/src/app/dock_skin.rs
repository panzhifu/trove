//! Trove's dock appearance.
//!
//! `gpui-base` owns every behaviour a dock has — the pane tree, tab
//! membership, the displayed tab, drag hit-testing, resize state — and draws
//! none of it. Upstream `gpui-kit` ships one appearance on top of it
//! (`component::dock::DockSkin`), and that appearance always draws a trailing
//! "⋯" menu button. Neither layer exposes a switch for it, and the request was
//! closed as *not planned*, with the maintainer pointing at `gpui-base` for a
//! bespoke title bar (longbridge/gpui-kit#2983).
//!
//! This module is that title bar. It is a renderer, not a fork: [`dock_area`]
//! installs it through the public `DockArea::with_renderer`, so `gpui-kit`
//! still comes from upstream `main` untouched.
//!
//! The panels are unchanged. `PanelHandle::of` recovers the component
//! presentation handle from base's erased `PanelView` — the only recovery Rust
//! allows, since the sub-trait object cannot be rebuilt from base's — so
//! `title`, `title_suffix` and `toolbar_buttons` keep working here.
//!
//! Rewritten rather than reused, because `component::dock` keeps them private:
//! the tab bar, the dock collapse affordances, the drop placeholder, the
//! styled drag preview, and the dock resize handle. `component::dock`'s own
//! `PanelHandle`, `ToggleZoom`/`ClosePanel` actions and theme tokens *are*
//! public, and are reused.

use std::{cell::Cell, rc::Rc, sync::Arc};

use gpui_kit::base::spring;
use gpui_kit::component::dock::{
    AnyDrag, BasePanelView, DockArea, DockAreaRenderer, DockContext, DockPlacement, DragPanel,
    DropIndicator, NodeId, PaneNode, PaneRef, PanelHandle, TabGroupContext, TabGroupRenderer,
};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    tab::{Tab, TabBar},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The size the drag preview occupies, reported to base so a drop placeholder
/// knows where to fly in from.
const DRAG_PREVIEW_SIZE: Size<Pixels> = size(px(96.), px(30.));

/// Build a dock area wearing Trove's appearance.
///
/// Mirrors `DockSkin::dock_area`: the renderer needs the area's weak handle
/// (the collapse affordances live in the tab bar but act on the area) and the
/// area needs the renderer, so both are built inside the constructor.
pub fn dock_area(
    id: impl Into<SharedString>,
    version: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DockArea> {
    cx.new(|cx| {
        let skin = Rc::new(TroveDockSkin::new(cx.weak_entity()));
        DockArea::new(id, version, window, cx).with_renderer(skin)
    })
}

/// The area's appearance: a background for the splits, and the resize handle
/// a dock is dragged by.
pub struct TroveDockSkin {
    /// The area's own weak handle. Only the area can say whether a dock is
    /// collapsible or open, and only the area can toggle it.
    area: WeakEntity<DockArea>,
    /// The dock whose handle is being dragged, if any. One at a time.
    resizing: Rc<Cell<Option<DockPlacement>>>,
}

impl TroveDockSkin {
    fn new(area: WeakEntity<DockArea>) -> Self {
        Self {
            area,
            resizing: Rc::new(Cell::new(None)),
        }
    }

    /// The four-pixel strip along a dock's inner edge that resizes it.
    ///
    /// Base has no hook for this: a handle has to be positioned against the
    /// dock it resizes, so the skin draws it here and drives it through
    /// `DockContext::resize_to`.
    fn resize_handle(&self, dock: &DockContext) -> Option<AnyElement> {
        let placement = dock.placement();
        let resizing = self.resizing.clone();

        // One id per placement: every dock renders under the same stateful
        // ancestor, so a shared literal would collapse the handles into one
        // element id and GPUI would silently share their element state — a
        // press on the left handle would start the right handle's drag.
        let handle = match placement {
            DockPlacement::Left => div()
                .id("dock-resize-left")
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(4.))
                .cursor(CursorStyle::ResizeLeftRight),
            DockPlacement::Right => div()
                .id("dock-resize-right")
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(4.))
                .cursor(CursorStyle::ResizeLeftRight),
            DockPlacement::Bottom => div()
                .id("dock-resize-bottom")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(4.))
                .cursor(CursorStyle::ResizeUpDown),
            // The centre is sized by what the side docks leave; it has no edge
            // of its own to drag.
            DockPlacement::Center => return None,
        };

        Some(
            handle
                .on_mouse_down(MouseButton::Left, move |_, _, _| {
                    resizing.set(Some(placement));
                })
                .into_any_element(),
        )
    }
}

impl DockAreaRenderer for TroveDockSkin {
    fn split_frame(&self, node: NodeId, _: Axis, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        // Base sizes the split; the background between two groups is this
        // skin's, and is the only reason the hook is implemented.
        div()
            .id(("dock-split-frame", node.as_u64()))
            .bg(cx.theme().tokens.tab_bar)
    }

    fn render_dock(
        &self,
        dock: &DockContext,
        content: AnyElement,
        _: &mut Window,
        _: &mut App,
    ) -> AnyElement {
        // A dock's extent along its own axis is structural and applied by
        // base around this, so a renderer that draws no box still gets a dock
        // the right shape. What is added here is the draggable edge and the
        // pointer tracking that drives it.
        let resize_handle = self.resize_handle(dock);
        div()
            .flex()
            .size_full()
            .relative()
            .child(content)
            .children(resize_handle)
            .child(DockResizeTracker {
                dock: dock.clone(),
                resizing: self.resizing.clone(),
            })
            .into_any_element()
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(TroveTabs {
            area: self.area.clone(),
            scroll_handle: ScrollHandle::default(),
            last_active_ix: Cell::new(None),
        })
    }
}

/// Turns the window's mouse stream into dock resizing.
///
/// A resize is driven by pointer moves that land anywhere in the window, not
/// only on the four-pixel handle, so it cannot be a listener on the handle
/// itself. This element paints nothing and exists for its `paint` hook, which
/// is the only place a window-level mouse listener can be registered.
struct DockResizeTracker {
    dock: DockContext,
    resizing: Rc<Cell<Option<DockPlacement>>>,
}

impl IntoElement for DockResizeTracker {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for DockResizeTracker {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let placement = self.dock.placement();

        window.on_mouse_event({
            let dock = self.dock.clone();
            let resizing = self.resizing.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() || resizing.get() != Some(placement) {
                    return;
                }
                // Dragging a closed dock's edge reopens it, rather than
                // resizing something that is not on screen.
                if !dock.is_open() {
                    dock.toggle(window, cx);
                }
                dock.resize_to(event.position, window, cx);
            }
        });

        window.on_mouse_event({
            let resizing = self.resizing.clone();
            move |_: &MouseUpEvent, phase, _, _| {
                if phase.bubble() {
                    resizing.set(None);
                }
            }
        });
    }
}

/// One tab group's appearance: the tab strip, or the plain title bar a lone
/// panel gets instead.
struct TroveTabs {
    area: WeakEntity<DockArea>,
    scroll_handle: ScrollHandle,
    /// The displayed tab the last frame drew, so a change scrolls the new tab
    /// into view. The group owns selection, so the strip notices rather than
    /// being told.
    last_active_ix: Cell<Option<usize>>,
}

impl TroveTabs {
    /// Whether a dock's collapse affordance belongs in *this* group's tab bar,
    /// and which way it points. `None` means this group draws none.
    fn dock_toggle_button(
        &self,
        placement: DockPlacement,
        group: &TabGroupContext,
        cx: &mut App,
    ) -> Option<Button> {
        if group.is_zoomed() {
            return None;
        }

        let area = self.area.upgrade()?;
        // The dock that carries the affordance is the one at the matching
        // corner of the centre: its tab bar is the one that touches the edge
        // the dock folds away from.
        let (collapsible, is_open) = {
            let area = area.read(cx);
            if !area.is_dock_collapsible(placement) {
                return None;
            }
            let designated = match placement {
                DockPlacement::Left => area
                    .layout(DockPlacement::Center)
                    .and_then(|tree| left_top_group(tree.root())),
                DockPlacement::Right => area
                    .layout(DockPlacement::Center)
                    .and_then(|tree| right_top_group(tree.root())),
                DockPlacement::Bottom => area
                    .layout(DockPlacement::Bottom)
                    .and_then(|tree| left_top_group(tree.root())),
                DockPlacement::Center => None,
            };
            if designated != Some(group.node()) {
                return None;
            }
            (true, area.is_dock_open(placement))
        };
        if !collapsible {
            return None;
        }

        let icon = match (placement, is_open) {
            (DockPlacement::Left, true) => IconName::PanelLeft,
            (DockPlacement::Left, false) => IconName::PanelLeftOpen,
            (DockPlacement::Right, true) => IconName::PanelRight,
            (DockPlacement::Right, false) => IconName::PanelRightOpen,
            (DockPlacement::Bottom, true) => IconName::PanelBottom,
            (DockPlacement::Bottom, false) => IconName::PanelBottomOpen,
            (DockPlacement::Center, _) => return None,
        };

        Some(
            Button::new(SharedString::from(format!("toggle-dock:{placement:?}")))
                .icon(icon)
                .xsmall()
                .ghost()
                .tab_stop(false)
                .tooltip(match is_open {
                    true => rust_i18n::t!("dock.collapse").to_string(),
                    false => rust_i18n::t!("dock.expand").to_string(),
                })
                .on_click(move |_, window, cx| {
                    area.update(cx, |area, cx| area.toggle_dock(placement, window, cx));
                }),
        )
    }

    /// The trailing controls: whatever buttons the panel contributes. There is
    /// no zoom or close affordance, so there is no menu to hang them off — the
    /// whole point of this renderer.
    fn render_toolbar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        if group.is_collapsed() {
            return div().into_any_element();
        }

        let buttons = group
            .active_panel()
            .and_then(PanelHandle::of)
            .and_then(|handle| handle.toolbar_buttons(window, cx));

        h_flex()
            .gap_1()
            .occlude()
            .when_some(buttons, |this, buttons| {
                this.children(
                    buttons
                        .into_iter()
                        .map(|button| button.xsmall().ghost().tab_stop(false)),
                )
            })
            .into_any_element()
    }

    /// The one-panel title bar: no tabs, just the title and the controls.
    fn render_title(
        &self,
        group: &TabGroupContext,
        ix: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let panel = &group.panels()[ix];
        let left_button = self.dock_toggle_button(DockPlacement::Left, group, cx);
        let bottom_button = self.dock_toggle_button(DockPlacement::Bottom, group, cx);
        let right_button = self.dock_toggle_button(DockPlacement::Right, group, cx);
        let has_leading = left_button.is_some() || bottom_button.is_some();
        let handle = PanelHandle::of(panel);
        let title_style = handle.and_then(|handle| handle.title_style(cx));
        let drag = tab_drag(group, ix, cx);

        h_flex()
            .justify_between()
            .h(px(30.))
            .py_2()
            .pl_3()
            .pr_2()
            .when(left_button.is_some(), |this| this.pl_2())
            .when(right_button.is_some(), |this| this.pr_2())
            .when_some(title_style, |this, style| {
                this.bg(style.background).text_color(style.foreground)
            })
            .when(has_leading, |this| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .mr_1()
                        .gap_1()
                        .children(left_button)
                        .children(bottom_button),
                )
            })
            .child(
                div()
                    .id("tab")
                    .flex_1()
                    .min_w_16()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(panel_title(panel, window, cx))
                    .when_some(drag, |this, drag| this.on_drag(drag, drag_preview(panel))),
            )
            .children(handle.and_then(|handle| handle.title_suffix(window, cx)))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .ml_1()
                    .gap_1()
                    .child(self.render_toolbar(group, window, cx))
                    .children(right_button),
            )
            .into_any_element()
    }

    /// The full tab bar.
    fn render_tabs(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let left_button = self.dock_toggle_button(DockPlacement::Left, group, cx);
        let bottom_button = self.dock_toggle_button(DockPlacement::Bottom, group, cx);
        let right_button = self.dock_toggle_button(DockPlacement::Right, group, cx);
        let has_leading = left_button.is_some() || bottom_button.is_some();
        let is_bottom_dock = bottom_button.is_some();
        let collapsed = group.is_collapsed();

        let droppable = group.is_droppable();
        let tabs_count = group.panels().len();
        let active_ix = group.active_ix();
        let displayed = group.active_panel().map(|panel| panel.panel_id(cx));
        let visible: Vec<usize> = group
            .panels()
            .iter()
            .enumerate()
            .filter(|(_, panel)| panel.visible(cx))
            .map(|(ix, _)| ix)
            .collect();
        let displayed_ix = displayed.and_then(|displayed| {
            group
                .panels()
                .iter()
                .position(|panel| panel.panel_id(cx) == displayed)
        });

        if self.last_active_ix.replace(Some(active_ix)) != Some(active_ix)
            && let Some(visible_ix) = visible.iter().position(|ix| *ix == active_ix)
        {
            self.scroll_handle.scroll_to_item(visible_ix);
        }

        TabBar::new("tab-bar")
            .track_scroll(&self.scroll_handle)
            .when(has_leading, |this| {
                this.prefix(
                    h_flex()
                        .items_center()
                        .top_0()
                        // Right -1 to avoid border overlap with the first tab.
                        .right(-px(1.))
                        .border_r_1()
                        .border_b_1()
                        .h_full()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().tokens.tab_bar)
                        .px_2()
                        .children(left_button)
                        .children(bottom_button),
                )
            })
            .children(visible.into_iter().map(|ix| {
                let panel = &group.panels()[ix];
                let handle = PanelHandle::of(panel);
                let drag = tab_drag(group, ix, cx);

                // `TabBar` fills in the tab's index, variant and size; the
                // index is what it keys each tab by, and it is private.
                let tab = Tab::new()
                    // A collapsed group shows no tab as active: the strip is a
                    // way back in, not a selection.
                    .selected(!collapsed && Some(ix) == displayed_ix);
                let tab = match handle.and_then(|handle| handle.tab_name(cx)) {
                    Some(tab_name) => tab.child(tab_name),
                    None => tab.child(panel_title(panel, window, cx)),
                };

                tab.on_click({
                    let group = group.clone();
                    let area = self.area.clone();
                    move |_, window, cx| {
                        group.select_tab(ix, window, cx);
                        // Clicking the strip of a collapsed bottom dock is how
                        // it is opened again.
                        if is_bottom_dock && collapsed {
                            _ = area.update(cx, |area, cx| {
                                area.toggle_dock(DockPlacement::Bottom, window, cx)
                            });
                        }
                    }
                })
                // A collapsed group is a strip of tabs with no content, so
                // there is nothing in it to rearrange.
                .when(!collapsed, |this| {
                    this.when_some(drag, |this, drag| this.on_drag(drag, drag_preview(panel)))
                        .when(droppable, |this| {
                            this.drag_over::<DragPanel>(|this, _, _, cx| {
                                this.rounded_l_none()
                                    .border_l_2()
                                    .border_r_0()
                                    .border_color(cx.theme().drag_border)
                            })
                            // Dropping on a tab lands *before* it, which is what
                            // makes a tab-by-tab reorder possible.
                            .on_drop({
                                let group = group.clone();
                                move |drag: &DragPanel, window, cx| {
                                    group.drop_panel(drag.clone(), Some(ix), true, window, cx);
                                }
                            })
                            .drag_over::<AnyDrag>(|this, _, _, cx| {
                                this.rounded_l_none()
                                    .border_l_2()
                                    .border_r_0()
                                    .border_color(cx.theme().drag_border)
                            })
                            .on_drop({
                                let group = group.clone();
                                move |item: &AnyDrag, window, cx| {
                                    group.drop_item(item.clone(), None, window, cx);
                                }
                            })
                        })
                })
            }))
            .last_empty_space(
                // Empty space so a panel can be moved past the last tab.
                div()
                    .id("tab-bar-empty-space")
                    .h_full()
                    .flex_grow_1()
                    .min_w_16()
                    .when(droppable, |this| {
                        this.drag_over::<DragPanel>(|this, _, _, cx| {
                            this.bg(cx.theme().tokens.drop_target)
                        })
                        .on_drop({
                            let group = group.clone();
                            let node = group.node();
                            move |drag: &DragPanel, window, cx| {
                                // A panel dropped past its own last tab lands
                                // in the final slot; one from elsewhere is
                                // appended in the background.
                                let ix = (drag.source() == node).then(|| tabs_count - 1);
                                group.drop_panel(drag.clone(), ix, false, window, cx);
                            }
                        })
                        .drag_over::<AnyDrag>(|this, _, _, cx| {
                            this.bg(cx.theme().tokens.drop_target)
                        })
                        .on_drop({
                            let group = group.clone();
                            move |item: &AnyDrag, window, cx| {
                                group.drop_item(item.clone(), None, window, cx);
                            }
                        })
                    }),
            )
            .when(!collapsed, |this| {
                this.suffix(
                    h_flex()
                        .items_center()
                        .top_0()
                        .right_0()
                        .border_l_1()
                        .border_b_1()
                        .h_full()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().tokens.tab_bar)
                        .px_2()
                        .gap_1()
                        .children(
                            group
                                .active_panel()
                                .and_then(PanelHandle::of)
                                .and_then(|handle| handle.title_suffix(window, cx)),
                        )
                        .child(self.render_toolbar(group, window, cx))
                        .children(right_button),
                )
            })
            .into_any_element()
    }
}

impl TabGroupRenderer for TroveTabs {
    fn frame(&self, _: &TabGroupContext, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        // The column, the fill and the clip are base's, applied around this.
        // What is left is the background.
        div().id("tab-panel").bg(cx.theme().tokens.background)
    }

    fn content_frame(
        &self,
        group: &TabGroupContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        let padded = group.panels().len() > 1
            && group
                .active_panel()
                .and_then(PanelHandle::of)
                .is_none_or(|handle| handle.inner_padding(cx));

        // The fill and the collapsed-group exception are base's; the padding
        // is this skin's, and is the only reason this hook is implemented.
        div().id("active-panel").when(padded, |this| this.pt_2())
    }

    fn render_tab_bar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let visible: Vec<usize> = group
            .panels()
            .iter()
            .enumerate()
            .filter(|(_, panel)| panel.visible(cx))
            .map(|(ix, _)| ix)
            .collect();

        match visible.as_slice() {
            [] => Empty.into_any_element(),
            // A lone panel gets a title bar rather than a one-tab strip, the
            // way `PanelStyle::Auto` does — unless it draws its own chrome.
            [ix] => {
                let panel = &group.panels()[*ix];
                if PanelHandle::of(panel).is_some_and(|handle| !handle.title_bar(cx)) {
                    return Empty.into_any_element();
                }
                self.render_title(group, *ix, window, cx)
            }
            _ => self.render_tabs(group, window, cx),
        }
    }

    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        _: &mut Window,
        _: &mut App,
    ) -> AnyElement {
        if group.is_collapsed() {
            return Empty.into_any_element();
        }

        div()
            .id("tab-content")
            .overflow_y_scroll()
            .overflow_x_hidden()
            .flex_1()
            .child(panel.cached(StyleRefinement::default().absolute().size_full()))
            .into_any_element()
    }

    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let to = indicator.to();
        // The placeholder chases the drop it would land in, so crossing
        // several drop zones in one drag carries it through instead of
        // restarting the walk at each one.
        let id = "drop-placeholder";
        let placeholder_spring = cx.theme().motion_tokens().spring_move.with_epsilon(0.5);
        let left = spring((id, "left"), to.origin().x, placeholder_spring, window, cx);
        let top = spring((id, "top"), to.origin().y, placeholder_spring, window, cx);
        let width = spring(
            (id, "width"),
            to.size().width,
            placeholder_spring,
            window,
            cx,
        );
        let height = spring(
            (id, "height"),
            to.size().height,
            placeholder_spring,
            window,
            cx,
        );

        Some(
            div()
                .absolute()
                .bg(cx.theme().tokens.drop_target)
                .left(left)
                .top(top)
                .w(width)
                .h(height)
                .into_any_element(),
        )
    }
}

/// The payload for dragging the tab at `ix` out of its group, or `None` when
/// this group must not be rearranged.
fn tab_drag(group: &TabGroupContext, ix: usize, cx: &App) -> Option<DragPanel> {
    group
        .is_draggable()
        .then(|| group.drag_panel(ix, cx))
        .flatten()
}

/// The card that follows the cursor while a panel is dragged.
///
/// `gpui_base::dock::DragPanel` is the payload and draws nothing; this is the
/// appearance half, which `component::dock` keeps to itself.
struct TroveDragPreview {
    panel: Arc<dyn BasePanelView>,
}

impl Render for TroveDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().tokens.tab)
            .border_1()
            .border_color(cx.theme().border)
            .child(panel_title(&self.panel, window, cx))
    }
}

/// The drag builder both the tabs and the plain title bar install.
fn drag_preview(
    panel: &Arc<dyn BasePanelView>,
) -> impl Fn(&DragPanel, Point<Pixels>, &mut Window, &mut App) -> Entity<TroveDragPreview> + 'static
{
    let panel = panel.clone();
    move |drag, offset, _, cx| {
        cx.stop_propagation();
        drag.set_drag_offset(offset);
        drag.set_preview_size(DRAG_PREVIEW_SIZE);
        cx.new(|_| TroveDragPreview {
            panel: panel.clone(),
        })
    }
}

/// A panel's title, from the component handle when there is one.
///
/// Base holds the panel as an erased `PanelView`, so a panel registered
/// without `panel_handle` has no presentation hooks to call and falls back to
/// its name — which is what `component::dock` does too.
fn panel_title(panel: &Arc<dyn BasePanelView>, window: &mut Window, cx: &mut App) -> AnyElement {
    match PanelHandle::of(panel) {
        Some(handle) => handle.title(window, cx),
        None => SharedString::from(panel.panel_name(cx)).into_any_element(),
    }
}

/// The left-most, top-most tab group in a container — where a left dock's
/// collapse affordance goes.
fn left_top_group(node: &PaneNode) -> Option<NodeId> {
    match node.kind() {
        PaneRef::Tabs { .. } => Some(node.id()),
        PaneRef::Split { children, .. } => children.first().and_then(left_top_group),
    }
}

/// The right-most, top-most tab group. A vertical split stacks its children,
/// so its *first* child is the top one; a horizontal split's last child is the
/// right-most.
fn right_top_group(node: &PaneNode) -> Option<NodeId> {
    match node.kind() {
        PaneRef::Tabs { .. } => Some(node.id()),
        PaneRef::Split { axis, children, .. } => match axis {
            Axis::Vertical => children.first(),
            Axis::Horizontal => children.last(),
        }
        .and_then(right_top_group),
    }
}
