//! Shared helpers for the dock panels.

use gpui_kit::base::h_flex;
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use trove_core::model::{Asset, AssetKind, AssetQuery};
use trove_core::store::assets;

use crate::library::LibraryController;

/// Per-asset color-label palette (name ↔ swatch hex); the names match
/// `trove_core::model::COLOR_LABELS`.
pub(crate) const COLOR_LABEL_SWATCHES: &[(&str, &str)] = &[
    ("red", "#ef4444"),
    ("orange", "#f97316"),
    ("yellow", "#eab308"),
    ("green", "#22c55e"),
    ("blue", "#3b82f6"),
    ("purple", "#a855f7"),
];

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
    h_flex().px_1().pt_1().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(text.into()),
    )
}

pub(crate) fn live_count(controller: &LibraryController) -> u64 {
    let conn = controller.library.store().conn();
    assets::query(conn, &AssetQuery::default())
        .map(|(total, _)| total)
        .unwrap_or(0)
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
    // Larger swatch for smoother edges (less visible aliasing) and a more
    // touch-friendly target.
    div()
        .id(id)
        .cursor_pointer()
        .size_6()
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

/// Payload for internal drag & drop of one or many selected assets.
#[derive(Debug, Clone)]
pub struct AssetsDrag(pub Vec<Uuid>);

/// Payload for dragging a collection row (reparent / reorder in the tree).
#[derive(Debug, Clone)]
pub struct CollectionDrag(pub Uuid);

/// Reveal a file or directory in the platform file manager. Where the
/// platform supports it the file is selected (Windows/macOS); on Linux the
/// containing directory opens instead.
pub(crate) fn reveal_path(path: &std::path::Path) {
    let is_file = path.is_file();
    let _ = if cfg!(target_os = "windows") {
        if is_file {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(path)
                .spawn()
        } else {
            std::process::Command::new("explorer").arg(path).spawn()
        }
    } else if cfg!(target_os = "macos") {
        if is_file {
            std::process::Command::new("open")
                .arg("-R")
                .arg(path)
                .spawn()
        } else {
            std::process::Command::new("open").arg(path).spawn()
        }
    } else {
        let dir = if is_file {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
}
