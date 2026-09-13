//! Asset preview: one entry point, one renderer per asset kind.
//!
//! [`AssetPreviewData`] resolves what a preview shows from a store record —
//! thumbnail, full-size original, animated image source, font family — and
//! [`element`] dispatches to the per-kind renderer in this folder: `image`,
//! `video` and `font`, with `fallback` covering kinds without a dedicated
//! one. [`PreviewContext`] tunes the render per placement: the workspace
//! main area shows everything at full size, while the inspector card is
//! compact and deliberately shows videos as their cover thumbnail instead
//! of playing them.
//!
//! [`AssetPreviewPanel`] is the main-area host: an entity owning the live
//! video player (if any) under a toolbar with the asset name and a close
//! button, so the workspace panel can hand its whole content area over —
//! the same contract [`model::ModelViewport`] offers for 3D models.

mod fallback;
mod font;
mod gpu3d;
mod image;
pub(crate) mod model;
mod video;

// The model viewport is the model kind's preview surface, so it lives in
// this folder too; re-exported here so hosts reach it without knowing the
// internal layout.
pub(crate) use model::{ModelViewport, ModelViewportEvent};

use std::path::{Path, PathBuf};

use gpui_kit::base::{ElementExt as _, h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::*;
use uuid::Uuid;

use crate::library::LibraryController;
use video::VideoPlayer;

/// Which placement renders the preview; the kinds differ in what "as large
/// as useful" means for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewContext {
    /// The workspace main area: everything at full size.
    Main,
    /// The inspector's preview card: compact, no live video.
    Inspector,
}

/// Everything a preview shows, resolved from a store record.
#[derive(Clone)]
pub(crate) struct AssetPreviewData {
    pub(crate) name: String,
    pub(crate) kind: trove_core::model::AssetKind,
    pub(crate) thumb: Option<PathBuf>,
    /// Full-size original: the library blob, or the linked source.
    pub(crate) original: Option<PathBuf>,
    /// Animated image source (GIF / animated WebP / APNG) when the original
    /// file can play frames.
    pub(crate) animated: Option<gpui_kit::ImageSource>,
    /// Family name probed at import, set only for fonts gpui can register.
    pub(crate) font_family: Option<String>,
    /// Media dimensions, for the inspector card's aspect-fit height.
    pub(crate) dimensions: Option<(u32, u32)>,
}

impl AssetPreviewData {
    /// Load the preview inputs for `asset_id` from the library. `None` when
    /// the asset no longer exists.
    pub(crate) fn load(controller: &LibraryController, id: Uuid) -> Option<Self> {
        let library_root = controller.library.root().to_path_buf();
        let conn = controller.library.store().conn();
        let asset = trove_core::store::assets::get(conn, id).ok().flatten()?;
        Some(Self::from_asset(&asset, &library_root))
    }

    /// Build from a record the caller already holds (the inspector renders
    /// from its own fetch; a second query per frame would be waste).
    pub(crate) fn from_asset(asset: &trove_core::model::Asset, library_root: &Path) -> Self {
        let thumb = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(library_root, sha))
            .filter(|p| p.is_file());
        let original = if asset.origin == trove_core::model::Origin::Linked {
            asset
                .facts
                .source_path
                .as_ref()
                .map(std::path::PathBuf::from)
        } else {
            asset.rel_path.as_ref().map(|rel| library_root.join(rel))
        };
        let animated = crate::panels::common::animated_preview_source(
            Some(asset.mime.as_str()),
            original.as_deref(),
        );
        Self {
            name: crate::panels::common::display_name(asset),
            kind: asset.kind,
            thumb,
            original,
            animated,
            font_family: asset.facts.font.family.clone(),
            dimensions: asset.width.zip(asset.height),
        }
    }

    /// Build preview inputs for a not-imported system font: the file is
    /// previewed in place, no store record involved.
    pub(crate) fn for_system_font(font: &trove_core::services::font_manager::SystemFont) -> Self {
        let style = font.style.clone().unwrap_or_default();
        let name = if style.is_empty() {
            font.family.clone()
        } else {
            format!("{} · {}", font.family, style)
        };
        Self {
            name,
            kind: trove_core::model::AssetKind::Font,
            thumb: None,
            original: Some(font.path.clone()),
            animated: None,
            font_family: Some(font.family.clone()),
            dimensions: None,
        }
    }

    /// The inspector card's height for this asset, from its aspect ratio
    /// (min 120px, max 360px; 200px when the dimensions are unknown).
    fn card_height(&self) -> f32 {
        match self.dimensions {
            Some((w, h)) if w > 0 && h > 0 => (300.0 / (w as f32 / h as f32)).clamp(120.0, 360.0),
            _ => 200.0,
        }
    }

    /// The preview element for `context`, dispatched to the per-kind
    /// renderer.
    pub(crate) fn element(&self, context: PreviewContext, cx: &mut App) -> gpui_kit::AnyElement {
        match context {
            PreviewContext::Main => main_element(self, cx),
            PreviewContext::Inspector => inspector_element(self, cx),
        }
    }
}

/// Render `data` for `context`, dispatching to the per-kind renderer.
pub(crate) fn element(
    data: &AssetPreviewData,
    context: PreviewContext,
    cx: &mut App,
) -> gpui_kit::AnyElement {
    match context {
        PreviewContext::Main => main_element(data, cx),
        PreviewContext::Inspector => inspector_element(data, cx),
    }
}

/// Full-size main-area render: the kind's dedicated renderer when it has
/// one, the thumbnail still otherwise. Video never reaches the still —
/// [`AssetPreviewPanel`] swaps in the live player instead.
fn main_element(data: &AssetPreviewData, cx: &mut App) -> gpui_kit::AnyElement {
    match data.kind {
        trove_core::model::AssetKind::Font => {
            font::specimen(data, cx).unwrap_or_else(|| image::still(data))
        }
        // Models normally preview through the interactive 3D viewport; if
        // one lands here anyway (viewport spawn failed), it shows its
        // rendered thumbnail.
        _ => image::still(data),
    }
}

/// Compact inspector render. Videos deliberately show their cover
/// thumbnail rather than playing — the card is small and a live player
/// there would fight the panel's edit controls. Animated images still play.
fn inspector_element(data: &AssetPreviewData, cx: &mut App) -> gpui_kit::AnyElement {
    match data.kind {
        trove_core::model::AssetKind::Video => video::cover(data, cx),
        _ => image::compact(data, cx),
    }
}

/// What the preview tells its host, the workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetPreviewEvent {
    /// The user left the preview (close button).
    Closed,
}

/// The main-area placement for a non-model asset: a toolbar with the asset
/// name, a zoom slider for stills and a close button over the full-size
/// preview. The workspace panel hands its whole content area to this
/// entity, exactly as it does to [`model::ModelViewport`] for 3D models.
pub(crate) struct AssetPreviewPanel {
    data: AssetPreviewData,
    /// Live player for videos; `None` renders the still variants instead.
    video: Option<Entity<VideoPlayer>>,
    /// Zoom slider state (0.25×–4×); the applied value lands in `zoom`.
    zoom_slider: Entity<SliderState>,
    /// Applied zoom for the still (1.0 = fit the viewport).
    zoom: f32,
    /// Measured content-viewport size; the fit base for the zoom math.
    viewport: Entity<Size<Pixels>>,
}

impl EventEmitter<AssetPreviewEvent> for AssetPreviewPanel {}

impl AssetPreviewPanel {
    /// Open the panel for `asset_id`, or `None` when the asset is gone.
    /// The controller borrow ends before the player entity is spawned, so
    /// the two library reads never alias `cx`.
    pub(crate) fn spawn(
        controller: &Entity<LibraryController>,
        id: Uuid,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        let data = AssetPreviewData::load(controller.read(cx), id)?;
        Some(Self::spawn_with_data(data, cx))
    }

    /// Open the panel for preview inputs the caller already resolved (a
    /// virtual system font, for instance).
    pub(crate) fn spawn_with_data(data: AssetPreviewData, cx: &mut App) -> Entity<Self> {
        // The live player is spawned once, here — never per render. An
        // undecodable file (or no ffmpeg) keeps the poster still.
        let video = video::spawn_player(&data, cx);
        let zoom_slider = cx.new(|_| {
            SliderState::new()
                .min(0.25)
                .max(4.0)
                .step(0.05)
                .default_value(1.0)
        });
        let viewport = cx.new(|_| size(px(0.), px(0.)));
        cx.new(|cx| {
            cx.subscribe(
                &zoom_slider,
                |this: &mut AssetPreviewPanel, _, event: &SliderEvent, cx| {
                    let value = match event {
                        SliderEvent::Change(value) | SliderEvent::Release(value) => value,
                    };
                    this.zoom = value.start().clamp(0.25, 4.0);
                    cx.notify();
                },
            )
            .detach();
            Self {
                data,
                video,
                zoom_slider,
                zoom: 1.0,
                viewport,
            }
        })
    }

    /// Stills can be zoomed while the live video plays in its own player.
    fn zoomable(&self) -> bool {
        self.video.is_none() && self.data.dimensions.is_some()
    }

    /// Hand the video's last decoded frame back to the window before the
    /// panel is dropped — gpui's sprite atlas never evicts on its own.
    pub(crate) fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(video) = &self.video {
            video.update(cx, |video, _| video.release(window));
        }
    }

    /// Leave the preview; the workspace panel puts the grid back.
    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(AssetPreviewEvent::Closed);
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let zoom_value = self.zoom_slider.read(cx).value().start();
        let zoom_label = format!("{:.0}%", (zoom_value * 100.0).round());
        let mut row = h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(self.data.name.clone()),
            );
        // Simple-edit tools land here later; for now the still's zoom.
        if self.zoomable() {
            row = row
                .child(
                    div()
                        .id("preview-zoom")
                        .flex_none()
                        .w(px(140.0))
                        .px_1()
                        .child(Slider::new(&self.zoom_slider)),
                )
                .child(
                    div()
                        .text_xs()
                        .w(px(38.0))
                        .text_color(cx.theme().muted_foreground)
                        .child(zoom_label),
                );
        }
        row.child(
            Button::new("preview-close")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(rust_i18n::t!("viewport.close").to_string())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.release(window, cx);
                    this.close(cx);
                })),
        )
    }

    /// The still at the applied zoom: the image keeps its aspect ratio and
    /// scales from its viewport fit; past 1:1 the original (not the
    /// thumbnail) carries the detail the zoom is asking for, and the
    /// viewport scrolls instead of letterboxing.
    fn zoomed_still(&self, cx: &mut Context<Self>) -> AnyElement {
        let (viewport_w, viewport_h) = {
            let bounds = self.viewport.read(cx);
            (f32::from(bounds.width), f32::from(bounds.height))
        };
        if viewport_w <= 0.0 || viewport_h <= 0.0 {
            return element(&self.data, PreviewContext::Main, cx);
        }
        let Some((iw, ih)) = self.data.dimensions else {
            return element(&self.data, PreviewContext::Main, cx);
        };
        let pad = 32.0; // the content container's p_4
        let fit = ((viewport_w - pad).max(60.0) / iw as f32)
            .min((viewport_h - pad).max(60.0) / ih as f32);
        let w = (iw as f32 * fit * self.zoom).max(1.0);
        let h = (ih as f32 * fit * self.zoom).max(1.0);
        let source: Option<gpui_kit::ImageSource> = if self.zoom > 1.05 {
            self.data
                .animated
                .clone()
                .or(self.data.original.clone().map(Into::into))
        } else {
            self.data
                .animated
                .clone()
                .or(self.data.thumb.clone().map(Into::into))
        };
        let image = match source {
            Some(source) => img(source)
                .w(px(w))
                .h(px(h))
                .object_fit(ObjectFit::Contain)
                .rounded(cx.theme().radius)
                .into_any_element(),
            None => element(&self.data, PreviewContext::Main, cx),
        };
        div()
            .id("preview-zoom-area")
            .size_full()
            .overflow_scroll()
            .child(
                div()
                    .w(px(w.max(viewport_w)))
                    .h(px(h.max(viewport_h)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(image),
            )
            .into_any_element()
    }
}

impl Render for AssetPreviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match &self.video {
            Some(player) => player.clone().into_any_element(),
            None => {
                if self.zoomable() && self.zoom != 1.0 {
                    self.zoomed_still(cx)
                } else {
                    element(&self.data, PreviewContext::Main, cx)
                }
            }
        };
        v_flex()
            .size_full()
            .overflow_hidden()
            .child(self.toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .p_4()
                    // Track the content viewport so the zoom has a fit base.
                    .on_prepaint({
                        let viewport = self.viewport.clone();
                        move |bounds: Bounds<Pixels>, _, cx| {
                            viewport.update(cx, |size, cx| {
                                if *size != bounds.size {
                                    *size = bounds.size;
                                    cx.notify();
                                }
                            });
                        }
                    })
                    .child(content),
            )
    }
}
