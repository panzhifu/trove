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
//! video player (if any). Its toolbar (asset name, zoom, close) is rendered
//! by the host panel's title bar, so the workspace panel can hand its whole
//! content area over — the same contract [`model::ModelViewport`] offers
//! for 3D models.

mod audio;
mod fallback;
mod font;
mod fullscreen;
mod gpu3d;
mod image;
pub(crate) mod model;
mod video;

// The model viewport is the model kind's preview surface, so it lives in
// this folder too; re-exported here so hosts reach it without knowing the
// internal layout.
pub(crate) use model::{ModelViewport, ModelViewportEvent};

use std::path::{Path, PathBuf};

use gpui_kit::base::{ElementExt as _, v_flex};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

/// Zoom factor per wheel notch.
const ZOOM_FACTOR: f32 = 1.15;

use crate::library::LibraryController;
use video::{FullscreenSeed, PlayerResume, VideoPlayer, VideoPlayerEvent};

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

/// The main-area placement for a non-model asset: the full-size still or
/// live player. Its toolbar (asset name + close) is rendered by the host
/// panel's title bar while a preview is open — see
/// [`crate::panels::WorkspacePanel::title_suffix`]. The workspace panel
/// hands its whole content area to this entity, exactly as it does to
/// [`model::ModelViewport`] for 3D models.
pub(crate) struct AssetPreviewPanel {
    data: AssetPreviewData,
    /// Live player for videos; `None` renders the still variants instead.
    video: Option<Entity<VideoPlayer>>,
    /// Watches the player for the fullscreen request: the panel pauses it
    /// and opens the fullscreen window from its state. `None` when there
    /// is no player.
    _video_events: Option<Subscription>,
    /// Applied zoom for the still (1.0 = fit the viewport).
    zoom: f32,
    /// Measured content-viewport size; the fit base for the zoom math.
    viewport: Entity<Size<Pixels>>,
    /// Pan/drag state: where the cursor was when the drag started.
    drag_from: Point<Pixels>,
    /// Whether the user is currently dragging to pan.
    dragging: bool,
    /// Current scroll offset for panning (tracked so wheel-zoom can
    /// recenter on the cursor).
    scroll_offset: Point<Pixels>,
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
        let viewport = cx.new(|_| size(px(0.), px(0.)));
        cx.new(|cx| {
            // Fullscreen hand-off: the player pauses here and a fresh
            // player in a fullscreen OS window continues from its state.
            // Leaving fullscreen comes back through the action, which the
            // fullscreen host answers with `resume_from` on this player.
            let _video_events = video.as_ref().map(|video| {
                cx.subscribe(
                    video,
                    |this: &mut AssetPreviewPanel,
                     player: Entity<VideoPlayer>,
                     event: &VideoPlayerEvent,
                     cx| {
                        // Single-variant event: irrefutable destructure.
                        let VideoPlayerEvent::EnterFullscreen {
                            position_ms,
                            speed,
                            volume,
                            muted,
                        } = *event;
                        // Only the picture stops here: the soundtrack is
                        // handed to the fullscreen window through the seed,
                        // so pausing it would cut the sound for as long as a
                        // fresh audio pipe takes to start.
                        player.update(cx, |player, cx| player.pause_video(cx));
                        let Some(original) = this.data.original.clone() else {
                            return;
                        };
                        // Hand over what the panel already knows: the
                        // probed facts (no second ffprobe on the click
                        // path) and the frame on screen (the fullscreen
                        // window opens on a picture, not on black).
                        let seed = {
                            let player = player.read(cx);
                            FullscreenSeed {
                                facts: player.facts(),
                                frame: player.current_frame(),
                                audio: player.audio(),
                            }
                        };
                        let host = cx.weak_entity();
                        fullscreen::open(
                            host,
                            original,
                            PlayerResume {
                                position_ms,
                                speed,
                                volume,
                                muted,
                                playing: true,
                            },
                            seed,
                            cx,
                        );
                    },
                )
            });
            Self {
                data,
                video,
                _video_events,
                zoom: 1.0,
                viewport,
                drag_from: Point::default(),
                dragging: false,
                scroll_offset: Point::default(),
            }
        })
    }

    /// Stills can be zoomed while the live video plays in its own player.
    pub(crate) fn zoomable(&self) -> bool {
        self.video.is_none() && self.data.dimensions.is_some()
    }

    /// Expose the asset name so the title bar can render it.
    pub(crate) fn asset_name(&self) -> &str {
        &self.data.name
    }

    /// Hand the video's last decoded frame back to the window before the
    /// panel is dropped — gpui's sprite atlas never evicts on its own.
    pub(crate) fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(video) = &self.video {
            video.update(cx, |video, _| video.release(window));
        }
    }

    /// Handle a scroll-wheel event: zoom toward the cursor, like the 3D
    /// model viewport does.
    fn handle_scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if !self.zoomable() {
            return;
        }
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => delta.y.as_f32() / 40.0,
        };
        if lines == 0.0 {
            return;
        }
        let cfg = trove_core::config::AppConfig::load();
        let zmin = cfg.min_preview_zoom();
        let zmax = cfg.max_preview_zoom();
        let old_zoom = self.zoom;
        let factor = ZOOM_FACTOR.powf(lines);
        let new_zoom = (self.zoom * factor).clamp(zmin, zmax);
        if new_zoom == old_zoom {
            return;
        }
        // Zoom toward the cursor: keep the point under the cursor stationary.
        let cursor = event.position;
        let (vp_w, vp_h) = {
            let vp = self.viewport.read(cx);
            (f32::from(vp.width), f32::from(vp.height))
        };
        // Cursor position relative to the viewport center, in content space.
        let cx_rel = f32::from(cursor.x) - vp_w / 2.0 - f32::from(self.scroll_offset.x);
        let cy_rel = f32::from(cursor.y) - vp_h / 2.0 - f32::from(self.scroll_offset.y);
        // Scale the offset so the content under the cursor stays put.
        let ratio = new_zoom / old_zoom;
        let new_cx = cx_rel * ratio;
        let new_cy = cy_rel * ratio;
        self.scroll_offset = point(
            px(new_cx - f32::from(cursor.x) + vp_w / 2.0),
            px(new_cy - f32::from(cursor.y) + vp_h / 2.0),
        );
        self.zoom = new_zoom;
        // Clamp panning to image bounds.
        self.clamp_scroll_offset(cx);
        // Sync the slider next render (needs Window).
        cx.notify();
    }

    /// Clamp the scroll offset so the image cannot be panned out of view.
    fn clamp_scroll_offset(&mut self, cx: &mut Context<Self>) {
        let (vp_w, vp_h) = {
            let vp = self.viewport.read(cx);
            (f32::from(vp.width), f32::from(vp.height))
        };
        let (iw, ih) = match self.data.dimensions {
            Some((w, h)) => (w as f32, h as f32),
            None => return,
        };
        let pad = 32.0;
        let fit = ((vp_w - pad).max(60.0) / iw).min((vp_h - pad).max(60.0) / ih);
        let w = iw * fit * self.zoom;
        let h = ih * fit * self.zoom;
        // Allow panning only when the image is larger than the viewport.
        let max_x = ((w - vp_w) / 2.0).max(0.0);
        let max_y = ((h - vp_h) / 2.0).max(0.0);
        let x = f32::from(self.scroll_offset.x).clamp(-max_x, max_x);
        let y = f32::from(self.scroll_offset.y).clamp(-max_y, max_y);
        self.scroll_offset = point(px(x), px(y));
    }

    /// Begin a pan drag.
    fn begin_pan(&mut self, position: Point<Pixels>) {
        self.dragging = true;
        self.drag_from = position;
    }

    /// Update a pan drag: scroll the viewport.
    fn update_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let dx = self.drag_from.x - position.x;
        let dy = self.drag_from.y - position.y;
        self.scroll_offset = point(
            px(f32::from(self.scroll_offset.x) + f32::from(dx)),
            px(f32::from(self.scroll_offset.y) + f32::from(dy)),
        );
        self.drag_from = position;
        // Clamp panning to image bounds.
        self.clamp_scroll_offset(cx);
        cx.notify();
    }

    /// End a pan drag.
    fn end_pan(&mut self) {
        self.dragging = false;
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
        // Centred by hand and offset by the pan, rather than centred by the
        // layout and shifted with margins: a sized child of an
        // overflow-clipped flex container does not move reliably on the cross
        // axis, which left the picture pannable up and down but not sideways.
        let left = (viewport_w - w) / 2.0 - f32::from(self.scroll_offset.x);
        let top = (viewport_h - h) / 2.0 - f32::from(self.scroll_offset.y);
        div()
            .id("preview-zoom-area")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(w))
                    .h(px(h))
                    .child(image),
            )
            .into_any_element()
    }
}

impl Render for AssetPreviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let zoomable = self.zoomable();
        v_flex().size_full().overflow_hidden().child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .p_4()
                .cursor(if self.dragging {
                    CursorStyle::ClosedHand
                } else if zoomable {
                    CursorStyle::OpenHand
                } else {
                    CursorStyle::Arrow
                })
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
                // Wheel zoom toward the cursor.
                .when(zoomable, |this| {
                    this.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        this.handle_scroll_wheel(event, cx);
                    }))
                })
                // Drag to pan when zoomed in.
                .when(zoomable, |this| {
                    this.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_pan(event.position);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        this.update_pan(event.position, cx);
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.end_pan();
                            cx.notify();
                        }),
                    )
                })
                .child(content),
        )
    }
}
