//! Image preview, and the thumbnail still every asset kind falls back to.
//!
//! Static pictures render the library thumbnail, not the original — the
//! thumbnail is what the import pipeline keeps on disk, and a 50-megapixel
//! original would decode into hundreds of megabytes of pixels for one
//! frame. Animated images (GIF / animated WebP / APNG) play from the
//! original file; gpui decodes GIF and animated WebP natively, APNG goes
//! through the cached multi-frame decoder in `panels::common`.

use gpui_kit::*;

use super::{fallback, AssetPreviewData};

/// Full-size still for the main area: animated source when the file can
/// play frames, thumbnail otherwise, kind icon when there is neither.
pub(super) fn still(data: &AssetPreviewData) -> AnyElement {
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
    match &data.thumb {
        Some(path) => img(path.clone())
            .w_full()
            .h(px(height))
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => fallback::icon_card(data.kind, cx),
    }
}
