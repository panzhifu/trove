//! Shared helpers for the dock panels.

use uuid::Uuid;
use gpui_kit::base::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::*;

use trove_core::model::{Asset, AssetKind, AssetQuery};
use trove_core::store::assets;

use crate::state::LibraryController;

/// Distinct icon per asset kind (image cells only fall back to this when no
/// thumbnail was generated). Icon names resolve to the gpui-kit asset set.
pub(crate) fn kind_icon(kind: AssetKind) -> IconName {
    match kind {
        AssetKind::Image => IconName::Frame,
        AssetKind::Video => IconName::Play,
        AssetKind::Audio => IconName::Pause,
        AssetKind::Document => IconName::FileText,
        AssetKind::Archive => IconName::File,
        AssetKind::Font => IconName::CaseSensitive,
        AssetKind::Other => IconName::File,
    }
}

pub(crate) fn display_name(asset: &Asset) -> String {
    asset
        .title
        .clone()
        .unwrap_or_else(|| asset.file_name.clone())
}

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub(crate) fn observe_controller<V: Render + 'static>(
    cx: &mut Context<V>,
    controller: &Entity<LibraryController>,
) {
    cx.observe(controller, |_, _, cx| cx.notify()).detach();
}

pub(crate) fn separator_label(cx: &Context<impl Render>, text: impl Into<String>) -> Div {
    h_flex()
        .px_1()
        .pt_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().muted_foreground)
                .child(text.into()),
        )
}

pub(crate) fn live_count(controller: &LibraryController) -> u64 {
    let conn = controller.library.store().conn();
    assets::query(conn, &AssetQuery::default()).map(|(total, _)| total).unwrap_or(0)
}

pub(crate) fn trash_count(controller: &LibraryController) -> u64 {
    let conn = controller.library.store().conn();
    assets::query(
        conn,
        &AssetQuery {
            is_trashed: true,
            ..Default::default()
        },
    )
    .map(|(total, _)| total)
    .unwrap_or(0)
}



/// Parse a `#rrggbb` hex (leading `#` optional, case-insensitive) into an
/// opaque `u32` value usable with `gpui::rgb(0xRRGGBB)`. `None` if malformed.
pub(crate) fn hex_to_rgb(s: &str) -> Option<u32> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// A round color chip in the shared palette style: hairline border, a
/// stronger ring when `selected`, hover feedback. Used by the smart-collection
/// palette and the Inspector's mined-color swatches.
pub(crate) fn color_swatch(
    cx: &App,
    id: String,
    hex: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let rgb = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    div()
        .id(id)
        .cursor_pointer()
        .size_5()
        .flex_shrink_0()
        .rounded_full()
        .bg(gpui_kit::rgb(rgb))
        .border_1()
        .border_color(cx.theme().border)
        .hover(|this| this.border_color(cx.theme().muted_foreground))
        .when(selected, |this| {
            this.border_2().border_color(cx.theme().foreground)
        })
        .on_click(on_click)
}

/// A clickable sidebar row with a selected highlight and an optional
/// right-click menu. Shared by the collection, tag and smart-collection lists.
/// `on_click` and `context_menu` each receive the full window/cx so they can
/// update entities; capture owned clones for `'static` closures.
type RowClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
type RowMenu =
    Box<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static>;

pub(crate) fn selectable_row(
    cx: &Context<impl Render>,
    id: &str,
    name: String,
    selected: bool,
    indent: Pixels,
    accent: Option<u32>,
    on_click: RowClick,
    context_menu: Option<RowMenu>,
) -> AnyElement {
    let mut row = div()
        .id(id.to_string())
        .cursor_pointer()
        .w_full()
        .pl(px(8.) + indent)
        .pr_2()
        .py_1()
        .rounded(cx.theme().radius)
        .on_click(move |ev: &ClickEvent, window, cx| on_click(ev, window, cx))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_1p5()
                .when_some(accent, |this, rgb| {
                    this.child(
                        div()
                            .size_2()
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(gpui_kit::rgb(rgb)),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(name),
                ),
        );
    if selected {
        row = row.bg(cx.theme().secondary);
    }
    match context_menu {
        Some(menu) => row.context_menu(menu).into_any_element(),
        None => row.into_any_element(),
    }
}

/// Payload for internal drag & drop of one or many selected assets.
#[derive(Debug, Clone)]
pub struct AssetsDrag(pub Vec<Uuid>);
