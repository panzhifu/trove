//! Font preview: the sample text set in the font itself.
//!
//! The thumbnail is a 512×256 card — a fine way to tell two fonts apart in
//! the grid, but far too small to judge one by. The main-area preview
//! draws the sample in the font itself, at whatever size makes the whole
//! sample fit the stage width. The inspector keeps the static card; a
//! giant specimen has no room there.
//!
//! Returns `None` from [`specimen`] when the font file cannot be
//! registered with the text system, so the caller falls back to the
//! thumbnail still.

use gpui_kit::*;

use super::AssetPreviewData;
use crate::panels::common::{ensure_font_registered, font_live_preview};

/// Width the main area typically leaves for text, inside the stage's
/// padding.
const TEXT_WIDTH: f32 = 900.0;
/// Roughly how wide a glyph is relative to its size. Latin faces run nearer
/// 0.5 and CJK nearer 1.0, so this splits the difference and errs towards
/// not clipping.
const GLYPH_RATIO: f32 = 0.62;

/// The specimen's base geometry at scale 1.0: block width, block height and
/// text size. The block is what the stage's fit math scales — the text size
/// always rides along proportionally, so the glyphs fill the block however
/// big the zoom makes it.
pub(super) fn specimen_metrics() -> (f32, f32, f32) {
    // Size against the built-in sample, the same text the grid cells and
    // the rasterized card show.
    let glyphs = trove_core::media::thumb::DEFAULT_FONT_SAMPLE
        .chars()
        .count()
        .max(1) as f32;
    let size = (TEXT_WIDTH / (glyphs * GLYPH_RATIO)).clamp(32.0, 160.0);
    (TEXT_WIDTH, size * 2.2, size)
}

/// Whether this font previews as live text (its file registered with the
/// text system) rather than as the thumbnail still.
pub(super) fn specimen_available(data: &AssetPreviewData, cx: &mut App) -> bool {
    data.font_family
        .as_ref()
        .is_some_and(|family| ensure_font_registered(family, data.original.as_deref(), cx))
}

/// The specimen block at an explicit geometry: the stage picks the size, the
/// font fills it.
pub(super) fn specimen_scaled(
    data: &AssetPreviewData,
    width: f32,
    height: f32,
    text_size: f32,
    cx: &mut App,
) -> Option<AnyElement> {
    let family = data.font_family.as_ref()?;
    if !ensure_font_registered(family, data.original.as_deref(), cx) {
        return None;
    }
    Some(
        font_live_preview(family, cx)
            .w(px(width))
            .h(px(height))
            .text_size(px(text_size))
            .into_any_element(),
    )
}

/// The large specimen line, or `None` when the font cannot be registered.
pub(super) fn specimen(data: &AssetPreviewData, cx: &mut App) -> Option<AnyElement> {
    let (width, height, size) = specimen_metrics();
    specimen_scaled(data, width, height, size, cx)
}
