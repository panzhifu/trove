//! Fallback for kinds with no dedicated preview: the kind's icon on a soft
//! card. Audio, documents, archives and anything unclassified land here,
//! and so does any asset whose thumbnail is missing from the library.

use gpui_kit::base::v_flex;
use gpui_kit::component::{ActiveTheme, Icon};
use gpui_kit::*;
use trove_core::model::AssetKind;

use crate::panels::common::kind_icon;

/// Large centered icon for the main area.
pub(super) fn icon_large(kind: AssetKind) -> AnyElement {
    v_flex()
        .h_64()
        .items_center()
        .justify_center()
        .child(Icon::new(kind_icon(kind)).size_8())
        .into_any_element()
}

/// Icon on the inspector card's background, sized for the small card.
pub(super) fn icon_card(kind: AssetKind, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .h(px(120.))
        .items_center()
        .justify_center()
        .bg(cx.theme().secondary)
        .rounded(cx.theme().radius)
        .child(Icon::new(kind_icon(kind)).size_10())
        .into_any_element()
}
