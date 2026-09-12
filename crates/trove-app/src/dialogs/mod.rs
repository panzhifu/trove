//! Modal dialogs layered above the dock: settings, the smart-collection rule
//! editor and the duplicate finder.

pub mod convert;
pub mod duplicates;
pub mod preview;
pub mod rename;
pub mod rules;
pub mod settings;
pub mod system_fonts;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _, WindowExt as _};
use gpui_kit::*;

/// A panel-local close X. gpui-kit's built-in floating close button sits
/// above the backdrop's window-drag region and silently loses clicks, so our
/// dialogs render their own X inside the panel and close via the imperative
/// `window.close_dialog` (no action dispatch, no focus dependence).
pub(crate) fn close_x(id: &'static str) -> gpui_kit::component::button::Button {
    Button::new(id)
        .small()
        .ghost()
        .icon(IconName::Close)
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// Wrap dialog content with a relative container hosting the close X in the
/// panel's top-right corner. Pair with `dialog.close_button(false)`.
pub(crate) fn with_close_x(id: &'static str, content: impl IntoElement) -> Div {
    div()
        .relative()
        .w_full()
        .child(content)
        .child(close_x(id).absolute().top_2().right_2())
}
