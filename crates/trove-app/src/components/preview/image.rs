//! Image preview, and the thumbnail still every asset kind falls back to.
//!
//! Static pictures render the library thumbnail, not the original — the
//! thumbnail is what the import pipeline keeps on disk, and a 50-megapixel
//! original would decode into hundreds of megabytes of pixels for one
//! frame. Animated images (GIF / animated WebP / APNG) come from the original
//! file: gpui decodes GIF and animated WebP natively, APNG goes through the
//! cached multi-frame decoder in `panels::common`.
//!
//! Which is to say what these two functions are *not*: the main area's playing
//! animation is the `anim` module, which holds its own frames and its own clock.
//! The animated source here is what a preview shows while that decoder is still
//! running, what it shows if the file turns out not to animate, and what the
//! inspector card shows — the card is small and inside a list that repaints
//! constantly, so gpui advancing it during a repaint happens to work.

use gpui_kit::*;
use trove_core::model::AssetKind;

use super::{AssetPreviewData, fallback};

/// Full-size still for the main area: the exposure-mapped render when the
/// preview's exposure control has produced one, animated source when the file
/// can play frames, thumbnail otherwise, kind icon when there is neither.
pub(super) fn still(data: &AssetPreviewData) -> AnyElement {
    if let Some(source) = &data.exposed {
        return img(source.clone())
            .max_h_full()
            .max_w_full()
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    if let Some(source) = &data.animated {
        return img(source.clone())
            .max_h_full()
            .max_w_full()
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    match &data.thumb {
        Some(path) => img(path.clone())
            .max_h_full()
            .max_w_full()
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => fallback::icon_large(data.kind),
    }
}

/// The still filling the stage: the same picture as [`still`], sized by the
/// stage instead of its own pixels, so a player that replaces it has nothing
/// to resize.
pub(super) fn still_filling(data: &AssetPreviewData) -> AnyElement {
    let source: Option<gpui_kit::ImageSource> = data
        .animated
        .clone()
        .or_else(|| data.thumb.clone().map(Into::into));
    match source {
        Some(source) => img(source)
            .size_full()
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => fallback::icon_large(data.kind),
    }
}

/// Compact card for the inspector: height clamped by the asset's aspect.
pub(super) fn compact(data: &AssetPreviewData, cx: &App) -> AnyElement {
    let height = data.card_height();
    if let Some(source) = &data.animated {
        return img(source.clone())
            .w_full()
            .h(px(height))
            .object_fit(ObjectFit::Contain)
            .into_any_element();
    }
    // A card the library bakes at a fixed size has no dimensions of its own, so
    // its card takes the shape it is drawn at instead of the nominal height
    // every other kind is clamped to: the frame is as wide as the panel and the
    // card fills it exactly, with none of the side bands a mismatched height
    // leaves behind. Those bands are more than dead space for an audio or text
    // card — such a card carries the model card's gradient background, so a flat
    // frame beside it shows two colours meeting. The ratio rides on a wrapper
    // because `img` would otherwise derive its height from the file's pixels,
    // which are unknown until the thumbnail has loaded.
    let baked_card_aspect = data.baked_card_aspect.or_else(|| {
        (data.kind == AssetKind::Model).then_some(trove_core::media::thumb::MODEL_CARD_ASPECT)
    });
    if let Some(aspect) = baked_card_aspect
        && let Some(path) = &data.thumb
    {
        return div()
            .w_full()
            .aspect_ratio(aspect)
            .child(img(path.clone()).size_full().object_fit(ObjectFit::Contain))
            .into_any_element();
    }
    match &data.thumb {
        Some(path) => img(path.clone())
            .w_full()
            .h(px(height))
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => fallback::icon_card(data.kind, cx),
    }
}

/// Decode the original at `stops` and hand it over as a render image — one
/// full float decode of the original folded through the display transform.
/// This is the cost the exposure control's "commit on release" exists for:
/// a 4K render pass is tens of megabytes of pixels and hundreds of megabytes
/// of float samples on the way, so it runs on a background thread and the
/// stage keeps the previous picture until this lands.
///
/// `part` selects the EXR part when the original is a multi-part file; a
/// decode the part decoder refuses (a part carrying deep data, an index past
/// the file) falls back to `image`'s own default pick rather than leaving the
/// stage stuck on the previous picture.
///
/// The frame comes out BGRA, the layout gpui's renderer expects — the same
/// swap the animated decoder in `panels::common` does.
pub(super) fn decode_exposed(
    path: &std::path::Path,
    stops: f32,
    part: usize,
) -> Option<gpui_kit::RenderImage> {
    let image = trove_core::media::hdr::open_for_display_part(path, stops, part)
        .or_else(|_| trove_core::media::hdr::open_for_display_at(path, stops))
        .ok()?;
    let mut frame = image::Frame::new(image.to_rgba8());
    for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(gpui_kit::RenderImage::new([frame]))
}
