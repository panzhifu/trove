//! Floating search box: a magnifier trigger with a pill-shaped popover.
//!
//! Self-contained component owning its input state and the popover's
//! controlled open flag. Commits the query on Enter through the
//! [`LibraryController`]; the ✕ inside the popover clears the query and
//! dismisses the popover.
//!
//! One instance per dock panel rather than one overall: the dock can be
//! rearranged and the workspace is not always the visible panel, so the
//! magnifier has to be within reach wherever the user is. The instances
//! share the *query* (through the controller) and nothing else — the
//! trigger's active tint reads `search_text`, and opening any of them loads
//! the committed term into its own input, so none of them can disagree about
//! what is being searched for. `panel` is what keeps their element ids
//! apart, since several are rendered in the same frame.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::h_flex;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::library::LibraryController;

/// Floating asset search, rendered in a panel's title bar.
pub struct SearchBox {
    controller: Entity<LibraryController>,
    input: Entity<InputState>,
    /// Controlled popover visibility. Shared via `Rc<Cell<bool>>` because
    /// the ✕ handler runs with a popover context, not `Context<Self>`.
    open: Rc<Cell<bool>>,
    /// The owning panel: the only thing that differs between instances, and
    /// the reason their element ids do not collide.
    panel: &'static str,
}

impl SearchBox {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
        panel: &'static str,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("workspace.search_placeholder").to_string())
        });
        cx.subscribe_in(&input, window, |this, _, event, _window, cx| {
            match event {
                InputEvent::PressEnter { .. } => {
                    let text = this.input.read(cx).value().trim().to_string();
                    this.controller.update(cx, |ctl, _| ctl.set_search(text));
                    // Hybrid ranking: fetch this term's embedding in the
                    // background. The grid paints the text ranking now and
                    // re-runs the query when the vector lands (a no-op when
                    // no embedding endpoint is configured).
                    crate::library::jobs::request_query_embedding_app(&this.controller, cx);
                    // Committing must NOT close the popover: pin the flag
                    // open and re-render so the controlled popover stays.
                    // The ✕ is the only way to close it.
                    this.open.set(true);
                    cx.notify();
                }
                // Re-render so the ✕ tracks the text while typing. Only this
                // component re-renders, not the whole workspace panel.
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        })
        .detach();
        // Keep the trigger tint in sync with committed searches.
        cx.observe(&controller, |_, _, cx| cx.notify()).detach();
        Self {
            controller,
            input,
            open: Rc::new(Cell::new(false)),
            panel,
        }
    }
}

impl Render for SearchBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_active = !self.controller.read(cx).search_text.trim().is_empty();
        let panel = self.panel;
        let this = cx.entity();
        let open = self.open.clone();
        let input = self.input.clone();
        let ctl = self.controller.clone();

        Popover::new(SharedString::from(format!("search-popover-{panel}")))
            .anchor(Anchor::TopRight)
            .open(self.open.get())
            .on_open_change({
                let open = open.clone();
                let this = this.clone();
                let input = input.clone();
                let ctl = ctl.clone();
                move |is_open: &bool, window, cx| {
                    open.set(*is_open);
                    if *is_open {
                        // Several boxes share one query: opening any of them
                        // has to show the term actually being searched for,
                        // not whatever this instance was last typed into.
                        let term = ctl.read(cx).search_text.clone();
                        input.update(cx, |state, cx| state.set_value(term, window, cx));
                    }
                    this.update(cx, |_, cx| cx.notify());
                }
            })
            // The pill-shaped input IS the surface: strip the popover's own
            // bg/border/shadow/padding while keeping overlay-click-to-close.
            .bg(gpui::transparent_black())
            .border_0()
            .shadow_none()
            .p_0()
            .trigger(
                Button::new(SharedString::from(format!("search-{panel}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Search)
                    .when(search_active, |b| b.primary())
                    .tooltip(rust_i18n::t!("workspace.search").to_string()),
            )
            .content({
                let input = input.clone();
                let ctl = ctl.clone();
                let open = open.clone();
                let this = this.clone();
                move |_, _, cx| {
                    h_flex()
                        .w(px(260.))
                        .h_7()
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().input)
                        .bg(cx.theme().background)
                        .px_3()
                        .gap_1()
                        .shadow_sm()
                        .child(Input::new(&input).appearance(false).small().w_full())
                        .child(
                            // Always visible: with text it clears + closes,
                            // when empty it just dismisses the popover.
                            Button::new(SharedString::from(format!("clear-search-{panel}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(rust_i18n::t!("workspace.clear_search").to_string())
                                .on_click({
                                    let input = input.clone();
                                    let ctl = ctl.clone();
                                    let open = open.clone();
                                    let this = this.clone();
                                    move |_, window, cx| {
                                        input.update(cx, |state, cx| {
                                            state.set_value("", window, cx)
                                        });
                                        ctl.update(cx, |ctl, _| ctl.set_search(String::new()));
                                        open.set(false);
                                        this.update(cx, |_, cx| cx.notify());
                                    }
                                }),
                        )
                        .into_any_element()
                }
            })
    }
}
