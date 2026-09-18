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
//! video player (if any). Its toolbar (asset name, edits, close) is rendered
//! by the host panel's title bar, so the workspace panel can hand its whole
//! content area over — the same contract [`model::ModelViewport`] offers
//! for 3D models. Every flat stage (still, specimen, video picture) moves
//! through [`PanZoom`], so drag-to-pan and wheel-zoom read the same in all
//! of them as they do in the 3D viewport.

mod audio;
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

use gpui_kit::base::{ElementExt as _, v_flex};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

/// Zoom factor per wheel notch.
const ZOOM_FACTOR: f32 = 1.15;

pub(crate) use video::VideoPlayer;

use crate::library::LibraryController;

// ============================================================================
// Shared pan/zoom
// ============================================================================

/// Pan/zoom state of a flat preview stage: a scale over the content's
/// viewport-fitted size plus the scroll offset that pans it. The 3D viewport
/// has its own camera; every flat preview — still, specimen, video picture —
/// moves through this, so drag-to-pan and wheel-zoom read the same in all of
/// them as they do in the model view.
pub(super) struct PanZoom {
    /// Applied zoom (1.0 = the viewport-fitted size).
    pub zoom: f32,
    /// Pan offset; positive moves the content right/down.
    pub offset: Point<Pixels>,
}

impl PanZoom {
    pub(super) fn new() -> Self {
        Self {
            zoom: 1.0,
            offset: Point::default(),
        }
    }

    /// Wheel event over the stage: zoom toward the cursor, keeping the point
    /// under it stationary — the 3D viewport's convention. `base` is the
    /// content's size at zoom 1.0; zooming is clamped to the configured
    /// preview limits. Returns whether anything changed.
    fn handle_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        viewport: (f32, f32),
        base: (f32, f32),
        zmin: f32,
        zmax: f32,
    ) -> bool {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => delta.y.as_f32() / 40.0,
        };
        if lines == 0.0 || base.0 <= 0.0 || base.1 <= 0.0 {
            return false;
        }
        let old_zoom = self.zoom;
        let new_zoom = (old_zoom * ZOOM_FACTOR.powf(lines)).clamp(zmin, zmax);
        if new_zoom == old_zoom {
            return false;
        }
        // Cursor position relative to the viewport center, in content space.
        let (vw, vh) = viewport;
        let cursor = event.position;
        let cx_rel = f32::from(cursor.x) - vw / 2.0 - f32::from(self.offset.x);
        let cy_rel = f32::from(cursor.y) - vh / 2.0 - f32::from(self.offset.y);
        // Scale the offset so the content under the cursor stays put.
        let ratio = new_zoom / old_zoom;
        self.offset = point(
            px(cx_rel * ratio - f32::from(cursor.x) + vw / 2.0),
            px(cy_rel * ratio - f32::from(cursor.y) + vh / 2.0),
        );
        self.zoom = new_zoom;
        self.clamp(viewport, base);
        true
    }

    /// Drag delta from the hand: the content follows the pointer. `base` is
    /// the content's size at zoom 1.0.
    fn pan_by(&mut self, dx: f32, dy: f32, viewport: (f32, f32), base: (f32, f32)) {
        self.offset = point(
            px(f32::from(self.offset.x) + dx),
            px(f32::from(self.offset.y) + dy),
        );
        self.clamp(viewport, base);
    }

    /// Keep the offset so the content cannot be panned out of view: the pan
    /// range is only the overflow past the viewport, half on each side.
    fn clamp(&mut self, (vw, vh): (f32, f32), (bw, bh): (f32, f32)) {
        let w = (bw * self.zoom).max(1.0);
        let h = (bh * self.zoom).max(1.0);
        let max_x = ((w - vw) / 2.0).max(0.0);
        let max_y = ((h - vh) / 2.0).max(0.0);
        self.offset = point(
            px(f32::from(self.offset.x).clamp(-max_x, max_x)),
            px(f32::from(self.offset.y).clamp(-max_y, max_y)),
        );
    }

    /// Back to the fitted view.
    fn reset(&mut self) {
        self.zoom = 1.0;
        self.offset = Point::default();
    }
}

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
    /// The store record this preview shows, when there is one (a virtual
    /// system font previews without an asset row).
    pub(crate) asset_id: Option<Uuid>,
    pub(crate) name: String,
    pub(crate) kind: trove_core::model::AssetKind,
    /// Whether the backend may re-encode this asset's pixels: an image the
    /// library owns (a linked file belongs to its owner) and not in trash.
    pub(crate) editable: bool,
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
        let cache_root = controller.library.cache().to_path_buf();
        let conn = controller.library.store().conn();
        let asset = trove_core::store::assets::get(conn, id).ok().flatten()?;
        Some(Self::from_asset(&asset, &library_root, &cache_root))
    }

    /// Build from a record the caller already holds (the inspector renders
    /// from its own fetch; a second query per frame would be waste).
    /// `library_root` locates stored blobs, `cache_root` the thumbnail.
    pub(crate) fn from_asset(
        asset: &trove_core::model::Asset,
        library_root: &Path,
        cache_root: &Path,
    ) -> Self {
        let thumb = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(cache_root, sha))
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
            asset_id: Some(asset.id),
            name: crate::panels::common::display_name(asset),
            kind: asset.kind,
            // The same terms the edit dialog admits on: pixel edits re-encode
            // the file, so only an image the library owns (never a linked
            // file, never a trashed one) may take them.
            editable: asset.kind == trove_core::model::AssetKind::Image
                && asset.origin != trove_core::model::Origin::Linked
                && asset.trashed_at.is_none(),
            thumb,
            original,
            animated,
            font_family: asset.facts.font.family.clone(),
            dimensions: asset.width.zip(asset.height),
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
    /// Whether the font specimen actually registered (a font the text system
    /// refuses falls back to its thumbnail still, which only zooms when the
    /// asset carries dimensions).
    font_live: bool,
    /// Pan/zoom of the flat stage — the still or the specimen.
    pan: PanZoom,
    /// Measured content-viewport size; the fit base for the zoom math.
    viewport: Entity<Size<Pixels>>,
    /// Pan/drag state: where the cursor was when the drag started.
    drag_from: Point<Pixels>,
    /// Whether the user is currently dragging to pan.
    dragging: bool,
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
        // A font whose specimen registers zooms the text itself; one that
        // falls back to its thumbnail still zooms only if that still has
        // recorded dimensions.
        let font_live = video.is_none()
            && data.kind == trove_core::model::AssetKind::Font
            && font::specimen_available(&data, cx);
        let viewport = cx.new(|_| size(px(0.), px(0.)));
        cx.new(|_| Self {
            data,
            video,
            font_live,
            pan: PanZoom::new(),
            viewport,
            drag_from: Point::default(),
            dragging: false,
        })
    }

    /// Stills, font specimens and videos all move through [`PanZoom`]; the
    /// flag decides whether the stage carries the gestures at all.
    pub(crate) fn zoomable(&self) -> bool {
        if self.video.is_some() {
            // The video player stages its own picture; this panel's gestures
            // would fight the transport controls.
            return false;
        }
        self.data.dimensions.is_some() || self.font_live
    }

    /// The store record behind the preview, for the title-bar tools.
    pub(crate) fn asset_id(&self) -> Option<Uuid> {
        self.data.asset_id
    }

    /// Whether the backend may re-encode this asset's pixels (an owned, live
    /// image) — the preview toolbar shows its edit buttons on this.
    pub(crate) fn editable(&self) -> bool {
        self.data.editable
    }

    /// The live player, for the app view to render as a fullscreen stage in
    /// its own window (nothing is handed over: the same player keeps going).
    pub(crate) fn video_player(&self) -> Option<Entity<VideoPlayer>> {
        self.video.clone()
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
        let (vw, vh) = self.viewport_size(cx);
        let Some(base) = self.fitted_base(vw, vh) else {
            return;
        };
        let cfg = trove_core::config::AppConfig::load();
        if self.pan.handle_wheel(
            event,
            (vw, vh),
            base,
            cfg.min_preview_zoom(),
            cfg.max_preview_zoom(),
        ) {
            cx.notify();
        }
    }

    fn viewport_size(&self, cx: &Context<Self>) -> (f32, f32) {
        let vp = self.viewport.read(cx);
        (f32::from(vp.width), f32::from(vp.height))
    }

    /// The content's on-screen size at zoom 1.0: the image aspect-fitted
    /// into the viewport, or the font specimen scaled to fit the same way
    /// (text renders sharp at any size, so it fills the stage like a
    /// picture does). `None` when there is nothing measurable behind the
    /// content.
    fn fitted_base(&self, vw: f32, vh: f32) -> Option<(f32, f32)> {
        let pad = 32.0; // the content container's p_4
        let fit = |w: f32, h: f32| {
            ((vw - pad).max(60.0) / w).min((vh - pad).max(60.0) / h)
        };
        if self.font_live {
            let (tw, th, _) = font::specimen_metrics();
            let scale = fit(tw, th);
            return Some((tw * scale, th * scale));
        }
        let (iw, ih) = self.data.dimensions?;
        if iw == 0 || ih == 0 {
            return None;
        }
        let scale = fit(iw as f32, ih as f32);
        Some((iw as f32 * scale, ih as f32 * scale))
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
        let dx = f32::from(self.drag_from.x - position.x);
        let dy = f32::from(self.drag_from.y - position.y);
        self.drag_from = position;
        let (vw, vh) = self.viewport_size(cx);
        if let Some(base) = self.fitted_base(vw, vh) {
            self.pan.pan_by(dx, dy, (vw, vh), base);
        }
        cx.notify();
    }

    /// End a pan drag.
    fn end_pan(&mut self) {
        self.dragging = false;
    }

    /// Double click: back to the fitted view, the 3D viewport's reset.
    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.pan.reset();
        cx.notify();
    }

    /// The still at the applied zoom: the image keeps its aspect ratio and
    /// scales from its viewport fit; past 1:1 the original (not the
    /// thumbnail) carries the detail the zoom is asking for, and the
    /// viewport scrolls instead of letterboxing. A font specimen scales the
    /// text itself the same way.
    fn zoomed_still(&self, cx: &mut Context<Self>) -> AnyElement {
        let (viewport_w, viewport_h) = self.viewport_size(cx);
        if viewport_w <= 0.0 || viewport_h <= 0.0 {
            return element(&self.data, PreviewContext::Main, cx);
        }
        let Some(base) = self.fitted_base(viewport_w, viewport_h) else {
            return element(&self.data, PreviewContext::Main, cx);
        };
        let w = (base.0 * self.pan.zoom).max(1.0);
        let h = (base.1 * self.pan.zoom).max(1.0);
        let content: Option<AnyElement> = if self.font_live {
            // The specimen scales as a block: the base geometry times the
            // zoom, and the text size with it.
            let (tw, _, ts) = font::specimen_metrics();
            let scale = w / tw;
            font::specimen_scaled(&self.data, w, h, ts * scale, cx)
        } else {
            let source: Option<gpui_kit::ImageSource> = if self.pan.zoom > 1.05 {
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
            source.map(|source| {
                img(source)
                    .w(px(w))
                    .h(px(h))
                    .object_fit(ObjectFit::Contain)
                    .rounded(cx.theme().radius)
                    .into_any_element()
            })
        };
        let Some(content) = content else {
            return element(&self.data, PreviewContext::Main, cx);
        };
        // Centred by hand and offset by the pan, rather than centred by the
        // layout and shifted with margins: a sized child of an
        // overflow-clipped flex container does not move reliably on the cross
        // axis, which left the picture pannable up and down but not sideways.
        let left = (viewport_w - w) / 2.0 - f32::from(self.pan.offset.x);
        let top = (viewport_h - h) / 2.0 - f32::from(self.pan.offset.y);
        div()
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
                    .child(content),
            )
            .into_any_element()
    }
}

impl Render for AssetPreviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match &self.video {
            Some(player) => player.clone().into_any_element(),
            None => {
                if self.zoomable() && self.pan.zoom != 1.0 {
                    self.zoomed_still(cx)
                } else {
                    element(&self.data, PreviewContext::Main, cx)
                }
            }
        };
        let zoomable = self.zoomable();
        v_flex().size_full().overflow_hidden().child(
            // `on_prepaint` lives on the plain `Div`, before the element
            // becomes `Stateful`; the id has to come after it (same contract
            // as the model canvas).
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
                .id("preview-stage")
                // Wheel zoom toward the cursor, drag to pan, double click
                // back to the fitted view — the model viewport's gestures.
                .when(zoomable, |this| {
                    this.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        this.handle_scroll_wheel(event, cx);
                    }))
                })
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
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.end_pan();
                            cx.notify();
                        }),
                    )
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        if event.click_count() == 2 {
                            this.reset_zoom(cx);
                        }
                    }))
                })
                .child(content),
        )
    }
}
