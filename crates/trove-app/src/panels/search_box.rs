//! Floating search box: a magnifier trigger with a pill-shaped popover.
//!
//! Self-contained component owning its input state and the popover's
//! controlled open flag. Commits the query on Enter through the
//! [`LibraryController`]; the ✕ inside the popover clears the query and
//! dismisses the popover.
//!
//! Every committed query is recorded in the library's own
//! [`LibraryConfig::search_history`] and offered back from a list under the
//! pill — see [`SearchBox::commit`], which is the one path both Enter and a
//! click on a past query go through.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::Selectable as _;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::components::controls::icon_button;
use crate::library::LibraryController;
use trove_core::config::LibraryConfig;

/// Floating asset search, rendered in the workspace title bar.
pub struct SearchBox {
    controller: Entity<LibraryController>,
    input: Entity<InputState>,
    /// Controlled popover visibility. Shared via `Rc<Cell<bool>>` because
    /// the ✕ handler runs with a popover context, not `Context<Self>`.
    open: Rc<Cell<bool>>,
    /// Whether the syntax reference is showing under the pill. Plain state:
    /// only this component's own handlers flip it, and they all run with a
    /// `Context<Self>`.
    help: bool,
    /// This library's settled queries, newest first. Read once at open and
    /// updated here whenever one is committed: this component is the only
    /// writer, so re-reading `library.json` on every repaint — the box
    /// re-renders per keystroke — would buy nothing.
    history: Vec<String>,
}

impl SearchBox {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        controller: Entity<LibraryController>,
    ) -> Self {
        let history = LibraryConfig::load(controller.read(cx).library.root())
            .search_history
            .clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("workspace.search_placeholder").to_string())
        });
        cx.subscribe_in(&input, window, |this, _, event, _window, cx| {
            match event {
                InputEvent::PressEnter { .. } => {
                    let text = this.input.read(cx).value().trim().to_string();
                    this.commit(&text, cx);
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
            help: false,
            history,
        }
    }

    /// Run a query, and remember it.
    ///
    /// The single path both Enter and a click on a past query take, so the two
    /// cannot drift into searching one way and recording another.
    fn commit(&mut self, text: &str, cx: &mut Context<Self>) {
        // Part of the box may not have parsed. The search still runs with what
        // did — a query that silently ignores a fragment is a wrong answer with
        // no visible cause — so every complaint goes to the status bar, not just
        // the first: a box with two mistakes in it has two fragments the user
        // needs to see vanish.
        let errors = trove_core::search::expression::parse(text).errors;
        if !errors.is_empty() {
            let listed = errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            let msg = rust_i18n::t!("workspace.search_syntax", error = listed).to_string();
            let controller = self.controller.clone();
            controller.update(cx, |ctl, cx| {
                if ctl.report_error(msg) {
                    cx.notify();
                }
            });
        }
        let controller = self.controller.clone();
        controller.update(cx, |ctl, _| ctl.set_search(text.to_string()));
        // Search tiers: fetch whatever the enabled tier needs in the
        // background. The grid paints the text ranking now and re-runs the
        // query when a leg lands (both are no-ops when the tier is off or
        // unconfigured).
        crate::library::jobs::request_query_embedding_app(&self.controller, cx);
        crate::library::jobs::request_ai_plan_app(&self.controller, cx);

        let dir = self.controller.read(cx).library.root().to_path_buf();
        let mut config = LibraryConfig::load(&dir);
        if config.remember_query(&dir, text).unwrap_or_else(|error| {
            // A history list that will not save is not a reason to lose the
            // search the user just ran.
            tracing::warn!(%error, "search history could not be written");
            false
        }) {
            self.history = config.search_history;
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
                    // A fresh open starts clean: the syntax reference is a
                    // per-look thing, not something to still be reading after
                    // the popover has been away.
                    this.update(cx, |this, cx| {
                        if !*is_open && this.help {
                            this.help = false;
                        }
                        cx.notify();
                    });
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
                let history = self.history.clone();
                let help = self.help;
                move |_, _, cx| {
                    // The pill keeps its own border and shadow; the recall list
                    // and the syntax reference are separate blocks under it
                    // rather than more controls crammed into the pill.
                    let pill = h_flex()
                        .w_full()
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
                            // The syntax reference: what the box understands,
                            // one toggle away while the box has focus.
                            icon_button(
                                "search-syntax-help",
                                IconName::Info,
                                rust_i18n::t!("workspace.search_help_title").to_string(),
                            )
                            .selected(help)
                            .on_click({
                                let this = this.clone();
                                move |_, _, cx| {
                                    this.update(cx, |this, cx| {
                                        this.help = !this.help;
                                        cx.notify();
                                    });
                                }
                            }),
                        )
                        .child(
                            // Always visible: with text it clears + closes,
                            // when empty it just dismisses the popover.
                            icon_button(
                                "clear-search",
                                IconName::Close,
                                rust_i18n::t!("workspace.clear_search").to_string(),
                            )
                            .on_click({
                                let input = input.clone();
                                let ctl = ctl.clone();
                                let open = open.clone();
                                let this = this.clone();
                                move |_, window, cx| {
                                    input.update(cx, |state, cx| state.set_value("", window, cx));
                                    ctl.update(cx, |ctl, _| ctl.set_search(String::new()));
                                    open.set(false);
                                    this.update(cx, |_, cx| cx.notify());
                                }
                            }),
                        );

                    v_flex()
                        .w(px(260.))
                        .gap_1()
                        .child(pill)
                        .when(help, |column| column.child(syntax_panel(cx)))
                        .when(!history.is_empty(), |column| {
                            column.child(history_list(&history, &this, &input, cx))
                        })
                        .into_any_element()
                }
            })
    }
}

/// How much of a past query a recall row shows.
///
/// The popover is 260 px wide and a row is one button, so a long query would
/// otherwise push the list's right edge out or wrap the row. Cutting on chars
/// (not bytes) keeps CJK queries from being split mid-codepoint, and the
/// elision mark is added only when something was actually dropped — the full
/// string is still what runs.
const HISTORY_ROW_CHARS: usize = 40;

fn shorten_query(query: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    if chars.len() <= HISTORY_ROW_CHARS {
        return query.to_string();
    }
    let mut head: String = chars[..HISTORY_ROW_CHARS].iter().collect();
    head.push('…');
    head
}

/// The recall list: past settled queries, newest first, each one a row that
/// re-runs it.
///
/// Renders nothing at all when there is no history, so a library that has never
/// been searched shows just the pill it always showed.
fn history_list(
    history: &[String],
    this: &Entity<SearchBox>,
    input: &Entity<InputState>,
    cx: &mut App,
) -> AnyElement {
    let rows = v_flex()
        .w_full()
        .children(history.iter().cloned().enumerate().map(|(ix, query)| {
            h_flex().w_full().px_1().child(
                Button::new(format!("search-history-{ix}"))
                    .ghost()
                    .xsmall()
                    .label(shorten_query(&query))
                    .tooltip(query.clone())
                    .on_click({
                        let this = this.clone();
                        let input = input.clone();
                        move |_, window, cx| {
                            // Put it in the box as well as running it, so a past
                            // query can be edited rather than only repeated.
                            let shown = this.read(cx).history.get(ix).cloned();
                            let Some(query) = shown else { return };
                            input.update(cx, |state, cx| state.set_value(&query, window, cx));
                            this.update(cx, |this, cx| {
                                this.commit(&query, cx);
                                cx.notify();
                            });
                        }
                    }),
            )
        }));

    v_flex()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .shadow_sm()
        .child(
            h_flex()
                .w_full()
                .px_3()
                .pt_2()
                .pb_1()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!("workspace.search_history").to_string()),
                )
                .child(
                    Button::new("clear-search-history")
                        .ghost()
                        .xsmall()
                        .label(rust_i18n::t!("workspace.clear_search_history").to_string())
                        .on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| {
                                    let dir = this.controller.read(cx).library.root().to_path_buf();
                                    let mut config = LibraryConfig::load(&dir);
                                    if let Err(error) = config.clear_search_history(&dir) {
                                        tracing::warn!(%error, "search history could not be cleared");
                                    }
                                    this.history.clear();
                                    cx.notify();
                                });
                            }
                        }),
                ),
        )
        // Bounded and scrollable, because the cap is 24 queries and a popover
        // that tall would cover the grid it is searching.
        .child(crate::components::scrollbar::vertical(
            v_flex().w_full().max_h(px(160.)).child(rows),
        ))
        .into_any_element()
}

/// What one syntax row shows: the literal to type, and the locale key naming
/// what it does. The literals are the parser's own shapes — a row that lied
/// here would be a query the box refuses.
const SYNTAX_ROWS: [(&str, &str); 8] = [
    ("word1 word2", "workspace.search_help_and"),
    ("word1 | word2", "workspace.search_help_or"),
    ("-word", "workspace.search_help_exclude"),
    ("\"two words\"", "workspace.search_help_phrase"),
    (
        "name: title: desc: tag: body:",
        "workspace.search_help_fields",
    ),
    (
        "camera: make: artist: album: font: color:",
        "workspace.search_help_meta",
    ),
    (
        "audio: sample_rate: channels: bit_depth: bitrate:",
        "workspace.search_help_audio",
    ),
    (
        "ext: kind: path: rating: fav:",
        "workspace.search_help_filters",
    ),
];

/// The syntax reference: one row per shape the parser understands, the token
/// over the description so a long qualifier list wraps without hiding what
/// to type. Bounded and scrollable for the same reason the history list is —
/// the popover sits over the grid it is searching.
fn syntax_panel(cx: &App) -> AnyElement {
    let rows = SYNTAX_ROWS
        .iter()
        .map(|(token, key)| {
            v_flex()
                .w_full()
                .gap_0p5()
                .px_3()
                .py_0p5()
                .child(div().text_xs().text_color(cx.theme().primary).child(*token))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!(*key).to_string()),
                )
        })
        .collect::<Vec<_>>();

    v_flex()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .shadow_sm()
        .child(
            div()
                .w_full()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("workspace.search_help_title").to_string()),
        )
        .child(crate::components::scrollbar::vertical(
            v_flex().w_full().max_h(px(220.)).children(rows),
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::shorten_query;

    /// A row must cut on characters, not bytes. The byte-slice version of this
    /// panics on the first CJK query long enough to need cutting at all, and
    /// 40 bytes of Chinese is 13 characters — so a test with ASCII only would
    /// have passed the broken version.
    #[test]
    fn a_long_query_is_cut_on_characters_and_marked_as_shortened() {
        let short = "tag:猫";
        assert_eq!(shorten_query(short), short, "a row that fits is untouched");

        let long = "description:sunset over the harbour at golden hour with clouds";
        let cut = shorten_query(long);
        assert_eq!(cut.chars().count(), 41, "40 chars plus one mark");
        assert!(cut.ends_with('…'));
        assert!(long.starts_with(&cut[..cut.len() - '…'.len_utf8()]));

        let cjk = "描述".repeat(40);
        let cut = shorten_query(&cjk);
        assert_eq!(cut.chars().count(), 41, "cut landed mid-codepoint");
        assert!(cut.starts_with(&"描述".repeat(20)));
    }

    /// Every row the panel shows must resolve to real copy in the current
    /// locale. `rust_i18n` answers a missing key with the key itself, so a
    /// typo'd key in [`SYNTAX_ROWS`] would render the token as its own
    /// description — and this test would be the only thing that noticed.
    #[test]
    fn every_syntax_row_resolves_to_copy() {
        use super::SYNTAX_ROWS;
        for (_, key) in SYNTAX_ROWS {
            let text = rust_i18n::t!(key).to_string();
            assert_ne!(text, *key, "{key} has no copy in this locale");
            assert!(!text.is_empty());
        }
        let title = rust_i18n::t!("workspace.search_help_title").to_string();
        assert_ne!(title, "workspace.search_help_title");
    }
}
