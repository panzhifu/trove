//! Small shared controls: the one-line builders every panel and dialog kept
//! re-inlining, collected so the styling has one spelling.
//!
//! These are not stateful components — each returns the gpui-kit element the
//! caller finishes with its own id, i18n text and handlers. What they fix is
//! the drift that crept in between hand-rolled copies: the ghost/xsmall order
//! on icon buttons, four spellings of the same muted caption, four widths of
//! the same dropdown picker.

use gpui::Keystroke;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::*;

/// A muted caption: the label that sits above a field or beside a value,
/// saying what the thing is without competing with it. The most repeated
/// block in the app; every panel hand-rolled this one.
pub(crate) fn muted_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// An icon-only button at the toolbar sizes: ghost so it sits in a row of
/// siblings, xsmall so it fits a panel edge, with the tooltip every icon-only
/// button owes the user. The caller chains `.on_click`, `.disabled` and
/// friends onto the returned button.
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(icon)
        .tooltip(tooltip.into())
}

/// A button whose dropdown lets the user pick one of `options`
/// (`(value, label)`); the current value is check-marked, `on_pick` fires
/// with the picked value. `min_w` in pixels keeps the menu from hugging its
/// label; `anchor` decides which corner of the button it opens from.
pub(crate) fn dropdown_button<T: PartialEq + Clone + 'static>(
    id: impl Into<ElementId>,
    label: String,
    options: Vec<(T, String)>,
    current: T,
    on_pick: impl Fn(T, &mut App) + Clone + 'static,
    min_w: f32,
    anchor: Anchor,
) -> impl IntoElement {
    Button::new(id)
        .xsmall()
        .outline()
        .label(label)
        .dropdown_menu_with_anchor(anchor, move |menu, _, _| {
            let mut menu = menu.min_w(px(min_w));
            for (value, option_label) in &options {
                let value = value.clone();
                let checked = value == current;
                let on_pick = on_pick.clone();
                menu = menu.item(
                    PopupMenuItem::new(option_label.clone())
                        .checked(checked)
                        .on_click(move |_, _, cx| on_pick(value.clone(), cx)),
                );
            }
            menu
        })
}

/// A padded muted note where a list or a body would be: "nothing here yet",
/// "computing", that register.
pub(crate) fn empty_note(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px_3()
        .py_4()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A muted note centred over the whole area it decorates: the loading,
/// failed and "nothing to see" overlays. The caller positions the wrapper
/// (absolute over a canvas, or centred in a panel body).
pub(crate) fn centered_note(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A keyboard chord rendered as a `Kbd` pill, falling back to the raw text
/// when the string is not a parseable keystroke — user-stored bindings can
/// be anything, and a parse failure must not cost the row its label.
pub(crate) fn kbd_or_raw(key: &str, cx: &App) -> AnyElement {
    match Keystroke::parse(key) {
        Ok(stroke) => Kbd::new(stroke).into_any_element(),
        Err(_) => muted_label(key.to_string(), cx).into_any_element(),
    }
}

/// The fold disclosure at a tree row's leading edge, on the contract
/// Serpent's sidebar runs (reference/Serpent NavigationSidebar.tsx): a fixed
/// 16px slot that holds a rotating chevron when the row has children and an
/// empty spacer when it has none, so sibling labels align across both states
/// and a parent is recognisable at a glance. The chevron points down while
/// the subtree is open and right while it is folded. The click stops here —
/// the row's own handler never sees it — so folding never selects.
pub(crate) fn fold_disclosure(
    id: impl Into<ElementId>,
    has_children: bool,
    expanded: bool,
    toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let slot = div()
        .flex_none()
        .size_4()
        .flex()
        .items_center()
        .justify_center();
    if !has_children {
        return slot.into_any_element();
    }
    slot.id(id.into())
        .cursor_pointer()
        .rounded(cx.theme().radius)
        .hover(|this| this.bg(cx.theme().secondary))
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            toggle(event, window, cx);
        })
        .child(
            // percentage() panics on negatives — 0.75 (270° cw) turns the
            // down-chevron right, the same trick the inspector headers use.
            Icon::new(IconName::ChevronDown)
                .size_3()
                .text_color(cx.theme().muted_foreground)
                .rotate(gpui::percentage(if expanded { 0. } else { 0.75 })),
        )
        .into_any_element()
}
