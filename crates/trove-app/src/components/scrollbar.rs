//! The scrollbar, as one component: the global styling every scrollbar in the
//! app inherits, and the scroll container the panels build their lists from.
//!
//! Two halves. [`init`] seats the app's scrollbar look — a hairline thumb that
//! widens slightly on hover — on the theme once at boot, where gpui-kit's
//! scrollbar layers read it. [`vertical`] is the container itself: every place
//! the app makes a region scroll goes through it, so scroll behaviour and
//! styling have one door to knock on.

use gpui_kit::component::scroll::Scrollable;
use gpui_kit::{App, px};

/// Seat the global scrollbar styling on the theme.
pub fn init(cx: &mut App) {
    use gpui_kit::base::{ScrollbarStyles, Theme};
    use gpui_kit::component::ActiveTheme as _;

    let mut thumb = cx.theme().muted_foreground;
    thumb.a = 0.35;
    let mut thumb_hover = thumb;
    thumb_hover.a = 0.6;
    let theme = Theme::global_mut(cx);
    theme.scrollbar = theme.scrollbar.clone().with_styles(
        ScrollbarStyles::default()
            .track(|t| t.width(px(8.)))
            .thumb(|s| s.width(px(4.)).inset(px(2.)).radius(px(2.)).bg(thumb))
            .thumb_hover(|s| s.width(px(6.)).inset(px(1.)).radius(px(3.)).bg(thumb_hover))
            .thumb_active(|s| s.width(px(6.)).inset(px(1.)).radius(px(3.)).bg(thumb_hover)),
    );
}

/// A vertically scrollable region: the one way the app's panels make a list
/// or a column scroll.
///
/// Takes the styled container and hands back gpui-kit's `Scrollable` wrapper —
/// itself `Styled` and `ParentElement`, so padding and children continue the
/// chain as if nothing had intervened.
pub fn vertical<E: ScrollableElement>(element: E) -> Scrollable<E> {
    element.overflow_y_scrollbar()
}

pub use gpui_kit::component::scroll::ScrollableElement;
