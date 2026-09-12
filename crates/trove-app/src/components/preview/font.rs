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
use crate::panels::common::{ensure_font_registered, font_live_preview, font_sample};

/// Width the main area typically leaves for text, inside the stage's
/// padding.
const TEXT_WIDTH: f32 = 900.0;
/// Roughly how wide a glyph is relative to its size. Latin faces run nearer
/// 0.5 and CJK nearer 1.0, so this splits the difference and errs towards
/// not clipping.
const GLYPH_RATIO: f32 = 0.62;

/// The large specimen line, or `None` when the font cannot be registered.
pub(super) fn specimen(data: &AssetPreviewData, cx: &mut App) -> Option<AnyElement> {
    let family = data.font_family.as_ref()?;
    if !ensure_font_registered(family, data.original.as_deref(), cx) {
        return None;
    }
    let glyphs = font_sample().chars().count().max(1) as f32;
    let size = (TEXT_WIDTH / (glyphs * GLYPH_RATIO)).clamp(32.0, 160.0);
    Some(
        font_live_preview(family, cx)
            .w_full()
            .h(px(size * 2.2))
            .text_size(px(size))
            .into_any_element(),
    )
}
