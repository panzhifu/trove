//! Floating search box: a magnifier trigger with a pill-shaped popover.
//!
//! Self-contained component owning its input state and the popover's
//! controlled open flag. Commits the query on Enter through the
//! [`LibraryController`]; the ✕ inside the popover clears the query and
//! dismisses the popover.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::h_flex;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::Sizable;
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::state::LibraryController;

/// Floating asset search, rendered in the workspace title bar.
pub struct SearchBox {
    controller: Entity<LibraryController>,
    input: Entity<InputState>,
    /// Controlled popover visibility. Shared via `Rc<Cell<bool>>` because
    /// the ✕ handler runs with a popover context, not `Context<Self>`.
    open: Rc<Cell<bool>>,
}

impl SearchBox {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
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
                }
                // Re-render so the ✕ tracks the text while typing. Only this
                // component re-renders, not the whole workspace panel.
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        })
        .detach();
        Self {
            controller,
            input,
            open: Rc::new(Cell::new(false)),
        }
    }
}

impl Render for SearchBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_active = !self.controller.read(cx).search_text.trim().is_empty();
        let this = cx.entity();
        let open = self.open.clone();
        let input = self.input.clone();
        let ctl = self.controller.clone();

        Popover::new("search-popover")
            .anchor(Anchor::TopRight)
            .open(self.open.get())
            .on_open_change({
                let open = open.clone();
                let this = this.clone();
                move |is_open: &bool, _, cx| {
                    open.set(*is_open);
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
                Button::new("search")
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
                    let has_text = !input.read(cx).value().trim().is_empty();
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
                        .when(has_text, |row| {
                            row.child(
                                Button::new("clear-search")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip(
                                        rust_i18n::t!("workspace.clear_search").to_string(),
                                    )
                                    .on_click({
                                        let input = input.clone();
                                        let ctl = ctl.clone();
                                        let open = open.clone();
                                        let this = this.clone();
                                        move |_, window, cx| {
                                            input.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            ctl.update(cx, |ctl, _| {
                                                ctl.set_search(String::new())
                                            });
                                            open.set(false);
                                            this.update(cx, |_, cx| cx.notify());
                                        }
                                    }),
                            )
                        })
                        .into_any_element()
                }
            })
    }
}
