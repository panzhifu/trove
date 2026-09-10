//! Asset preview dialog: the large Enter-key preview of the primary
//! selected asset. A standalone component so other surfaces (inspector,
//! search results, duplicates) can open the same preview later.

use gpui_kit::base::v_flex;
use gpui_kit::component::Icon;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::library::LibraryController;

use crate::library::video_player::VideoPlayer;
use crate::panels::common::{
    animated_preview_source, display_name, ensure_font_registered, font_live_preview, font_sample,
    kind_icon,
};

/// Resolve the preview inputs for `asset_id` from the library: display
/// name, static thumbnail path, kind, the full-size original, and an
/// animated image source (GIF / animated WebP / APNG) when the original
/// file can play frames.
struct PreviewData {
    name: String,
    kind: trove_core::model::AssetKind,
    thumb: Option<std::path::PathBuf>,
    /// Full-size original (library blob or linked source), kept for the
    /// video player.
    original: Option<std::path::PathBuf>,
    animated: Option<gpui_kit::ImageSource>,
    /// Family name probed at import, set only for fonts gpui can register.
    font_family: Option<String>,
}

impl PreviewData {
    fn load(controller: &LibraryController, id: Uuid) -> Option<Self> {
        let library_root = controller.library.root().to_path_buf();
        let conn = controller.library.store().conn();
        let asset = trove_core::store::assets::get(conn, id).ok().flatten()?;

        let thumb = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(&library_root, sha))
            .filter(|p| p.is_file());
        // Full-size original: the library blob, or the linked source.
        let original = if asset.origin == trove_core::model::Origin::Linked {
            asset
                .extra
                .get("source_path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
        } else {
            asset.rel_path.as_ref().map(|rel| library_root.join(rel))
        };
        let animated = animated_preview_source(Some(asset.mime.as_str()), original.as_deref());
        Some(Self {
            name: display_name(&asset),
            kind: asset.kind,
            thumb,
            original,
            animated,
            font_family: asset
                .extra
                .get("font_family")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }

    /// The preview element: a live video player when ffmpeg can decode the
    /// file, then a live font specimen, then the animated source, then the
    /// static thumbnail, then a kind icon.
    fn element(&self, cx: &mut App) -> gpui_kit::AnyElement {
        if self.kind == trove_core::model::AssetKind::Video
            && let Some(path) = &self.original
            && trove_core::media::video::ffmpeg_available()
            && let Some(player) = VideoPlayer::spawn(path.clone(), cx)
        {
            return player.into_any_element();
        }
        if self.kind == trove_core::model::AssetKind::Font
            && let Some(family) = self.font_family.as_ref()
            && ensure_font_registered(family, self.original.as_deref(), cx)
        {
            return self.specimen(family, cx);
        }
        if let Some(source) = &self.animated {
            return img(source.clone())
                .max_h(px(520.0))
                .max_w(px(720.0))
                .object_fit(gpui_kit::ObjectFit::Contain)
                .into_any_element();
        }
        match &self.thumb {
            Some(path) => img(path.clone())
                .max_h(px(520.0))
                .max_w(px(720.0))
                .object_fit(gpui_kit::ObjectFit::Contain)
                .into_any_element(),
            None => v_flex()
                .h_64()
                .items_center()
                .justify_center()
                .child(Icon::new(kind_icon(self.kind)).size_8())
                .into_any_element(),
        }
    }

    /// A font set large. The thumbnail is a 512×256 card, which is a fine
    /// way to tell two fonts apart in the grid but far too small to judge
    /// one by; in the preview the sample text is drawn in the font itself at
    /// whatever size makes the whole sample fit the dialog.
    fn specimen(&self, family: &str, cx: &mut App) -> gpui_kit::AnyElement {
        /// Width the dialog leaves for text, inside its own padding.
        const TEXT_WIDTH: f32 = 700.0;
        /// Roughly how wide a glyph is relative to its size. Latin faces run
        /// nearer 0.5 and CJK nearer 1.0, so this splits the difference and
        /// errs towards not clipping.
        const GLYPH_RATIO: f32 = 0.62;

        let glyphs = font_sample().chars().count().max(1) as f32;
        let size = (TEXT_WIDTH / (glyphs * GLYPH_RATIO)).clamp(24.0, 120.0);
        font_live_preview(family, cx)
            .w_full()
            .h(px(size * 2.2))
            .text_size(px(size))
            .into_any_element()
    }
}

/// Open the large preview dialog for `asset_id` (no-op when the asset no
/// longer exists).
pub(crate) fn open_asset_preview(
    controller: &Entity<LibraryController>,
    id: Uuid,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(data) = PreviewData::load(controller.read(cx), id) else {
        return;
    };
    window.open_dialog(cx, move |dialog, _window, cx| {
        dialog.title(data.name.clone()).width(px(780.)).child(
            v_flex()
                .w_full()
                .gap_2()
                .items_center()
                // Preview image.
                .child(
                    div()
                        .w_full()
                        .max_h(px(600.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .overflow_hidden()
                        .child(data.element(cx)),
                ),
        )
    });
}
