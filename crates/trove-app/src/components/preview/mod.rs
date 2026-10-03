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

mod anim;
mod audio;
mod chrome;
mod fallback;
mod font;
mod gpu3d;
mod image;
pub(crate) mod model;
mod quick_look;
mod sequence;
mod soundtrack;
mod text;
mod transport;
mod video;

// The model viewport is the model kind's preview surface, so it lives in
// this folder too; re-exported here so hosts reach it without knowing the
// internal layout.
pub(crate) use model::{ModelViewport, ModelViewportEvent};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::base::{ElementExt as _, v_flex};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

/// Zoom factor per wheel notch.
const ZOOM_FACTOR: f32 = 1.15;

/// The content container's `p_4`, taken off each axis before a picture is
/// fitted: the still and the live video stage are both measured against it.
const STAGE_PAD: f32 = 32.0;

pub(crate) use quick_look::LiveCard;
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

/// A picture's size at zoom 1.0: `geometry` fitted into `area`, preserving
/// its aspect ratio. The still, the video stage and the pan/zoom base all
/// size themselves through this one rule, so a picture never changes size
/// when the surface behind it does. `None` when either side is degenerate.
pub(super) fn fit_box(geometry: (f32, f32), area: (f32, f32)) -> Option<(f32, f32)> {
    let (gw, gh) = geometry;
    let (aw, ah) = area;
    if gw <= 0.0 || gh <= 0.0 || aw <= 0.0 || ah <= 0.0 {
        return None;
    }
    let scale = (aw / gw).min(ah / gh);
    Some((gw * scale, gh * scale))
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
    /// Whether the previewed asset is an image — the only kind whose
    /// toolbar carries the pixel-edit tools.
    pub(crate) is_image: bool,
    /// Whether an edit on this asset writes its result back over the
    /// original file (a *linked* asset: the library holds no copy). The
    /// toolbar asks for confirmation before the first byte moves; a stored
    /// asset's edit only touches the library's own blob.
    pub(crate) write_back: bool,
    /// Why the backend would refuse a pixel edit on this asset, `None` when
    /// it may take one. A trashed asset is not editable until restored —
    /// the toolbar renders its buttons disabled with this as the reason,
    /// rather than hiding them; a silently missing toolbar reads as a bug.
    pub(crate) edit_blocker: Option<&'static str>,
    pub(crate) thumb: Option<PathBuf>,
    /// A video's own first frame, extracted at import and cached beside the
    /// thumbnail. The player stands it in until ffmpeg produces the real
    /// first frame; because it *is* that frame, the swap is invisible. `None`
    /// for every other kind, and for videos imported before posters existed —
    /// there the player falls back to `thumb`, which hides the wait too at
    /// the cost of one content jump.
    pub(crate) poster: Option<PathBuf>,
    /// Full-size original: the library blob, or the linked source.
    pub(crate) original: Option<PathBuf>,
    /// The exposure-mapped render of [`Self::original`], when the preview's
    /// exposure control has produced one. Session state attached by the panel
    /// rather than something the record resolves — a scene-linear file shows
    /// its thumbnail until the user moves the slider, and shows it again the
    /// moment the slider returns to zero.
    pub(crate) exposed: Option<gpui_kit::ImageSource>,
    /// Animated image source (GIF / animated WebP / APNG) when the original
    /// file can play frames.
    pub(crate) animated: Option<gpui_kit::ImageSource>,
    /// Family name probed at import, set only for fonts gpui can register.
    pub(crate) font_family: Option<String>,
    /// The language the font file declares (`font_language` fact), parsed to
    /// a preview language — the initial value of the viewer's language
    /// picker. `None` when the file's evidence is inconclusive.
    pub(crate) font_declared_language: Option<font::FontPreviewLanguage>,
    /// The font file's `wght` axis as bounds, when it is variable — what the
    /// weight picker offers and what the rendered weight clamps to.
    pub(crate) font_variable_weight: Option<(u16, u16)>,
    /// The face's own (default-instance) weight, the value the text system
    /// renders without any patched registration.
    pub(crate) font_base_weight: Option<u16>,
    /// The font viewer's session state — the picked preview language and the
    /// text it resolves to — attached by the preview panel exactly like
    /// [`Self::exposed`]: resolved once per mutation, cloned on the paint
    /// path. `None` until a live font preview attaches it.
    pub(crate) font_preview: Option<font::FontPreviewState>,
    /// Media dimensions, for the inspector card's aspect-fit height.
    pub(crate) dimensions: Option<(u32, u32)>,
    /// Cache root plus content hash, which is everything the waveform cache
    /// needs to key itself. Only audio uses it today; it is not folded into
    /// `thumb` because a thumbnail's path is a finished artifact while this is
    /// an address to write one.
    pub(crate) wave_cache: Option<(PathBuf, String)>,
    /// Recorded media length, mined at import. The audio transport needs it to
    /// bound its timeline; the video player gets its own duration from the
    /// decode probe, so only audio reads this today.
    pub(crate) duration_ms: Option<u64>,
    /// The frame run this asset belongs to, resolved to file paths — what the
    /// sequence player walks. `None` for a standalone asset and for every
    /// context that cannot resolve paths (the inspector renders stills).
    pub(crate) sequence: Option<SequenceInfo>,
    /// Complete decoder facts mined at import, when the row carries them.
    /// `Some` means the player can spawn the instant the panel opens — no
    /// probe between the click and the picture; `None` (rows imported before
    /// the facts existed) falls back to probing while the still stands in.
    pub(crate) video_facts: Option<trove_core::media::video::VideoStreamFacts>,
}

/// A frame run the preview can play: the run's frame rate and every frame's
/// own file, in display order.
#[derive(Clone)]
pub(crate) struct SequenceInfo {
    pub(crate) fps: f64,
    pub(crate) frames: Vec<PathBuf>,
}

impl AssetPreviewData {
    /// Load the preview inputs for `asset_id` from the library. `None` when
    /// the asset no longer exists.
    pub(crate) fn load(controller: &LibraryController, id: Uuid) -> Option<Self> {
        let library_root = controller.library.root().to_path_buf();
        let cache_root = controller.library.cache().to_path_buf();
        let asset = controller.library.asset(id).ok().flatten()?;
        let mut data = Self::from_asset(&asset, &library_root, &cache_root);
        // A frame run is playable only when the library can name every
        // frame's file; a run with missing files plays the ones it can name,
        // and fewer than two is no run at all.
        if data.is_image
            && let Ok(Some(membership)) = controller.library.sequence_of(id)
        {
            let frames: Vec<PathBuf> = membership
                .frames
                .iter()
                .filter_map(|frame_id| controller.library.asset_file(*frame_id))
                .collect();
            if frames.len() >= 2 {
                data.sequence = Some(SequenceInfo {
                    fps: membership.fps(),
                    frames,
                });
            }
        }
        // Video: the import already probed the container, so a complete fact
        // set may be sitting on the row — the player starts without asking
        // ffprobe anything. Any missing piece (rows from before the facts
        // existed) drops back to the probe path.
        if data.kind == trove_core::model::AssetKind::Video
            && let Some((width, height)) = asset.width.zip(asset.height)
            && let Some(fps) = asset.facts.video.fps
            && let Some(has_audio) = asset.facts.video.has_audio
        {
            data.video_facts = Some(trove_core::media::video::VideoStreamFacts {
                width,
                height,
                fps,
                duration_ms: asset.duration_ms.unwrap_or(0),
                has_audio,
            });
        }
        Some(data)
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
            .content_hash
            .as_deref()
            .map(|hash| trove_core::media::thumb::abs_path(cache_root, hash))
            .filter(|p| p.is_file());
        // The video's first-frame poster, when the import cache holds one.
        // Read on the same query as the thumbnail — both are keyed by the
        // content hash, so nothing here costs a second lookup.
        let poster = asset
            .content_hash
            .as_deref()
            .and_then(|hash| trove_core::media::thumb::cached_poster(cache_root, hash));
        // Where the record's own bytes are: the library blob, or the linked
        // original. `blob_path` is that rule in one place rather than a fifth
        // copy of it here -- five callers had each re-derived it, and a rule
        // restated five times is a rule that drifts.
        let original = trove_core::media::thumb::blob_path(library_root, asset);
        let animated = crate::panels::common::animated_preview_source(
            Some(asset.mime.as_str()),
            original.as_deref(),
        );
        Self {
            asset_id: Some(asset.id),
            name: crate::panels::common::display_name(asset),
            kind: asset.kind,
            is_image: asset.kind == trove_core::model::AssetKind::Image,
            write_back: asset.location().is_linked(),
            edit_blocker: if asset.placement().is_trashed() {
                Some("edit.blocked_trashed")
            } else if !trove_core::media::edit::is_editable_ext(&asset.ext) {
                // Read-only here means *read-only in place*: a format the
                // editor has no encoder for (EXR, HDR, TGA, RAW, HEIF, PSD,
                // SVG, JXL) would come back as a refused edit, and for a
                // linked asset a refusal is the good outcome — an 8-bit
                // re-encode of a scene-linear file would destroy it.
                Some("edit.blocked_format")
            } else {
                None
            },
            thumb,
            poster,
            original,
            exposed: None,
            animated,
            font_family: asset.facts.font.family.clone(),
            font_declared_language: asset
                .facts
                .font
                .language
                .as_deref()
                .and_then(font::FontPreviewLanguage::parse_declared),
            font_variable_weight: asset
                .facts
                .font
                .variable_weight
                .as_deref()
                .and_then(font::parse_variable_weight),
            font_base_weight: asset.facts.font.weight,
            font_preview: None,
            dimensions: asset.width.zip(asset.height),
            duration_ms: asset.duration_ms,
            wave_cache: asset
                .content_hash
                .as_deref()
                .map(|sha| (cache_root.to_path_buf(), sha.to_string())),
            sequence: None,
            video_facts: None,
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
    /// The animated-picture player, when the asset is one. See [`anim`].
    anim: Option<Entity<anim::AnimatedPlayer>>,
    /// The sequence player, when the asset is a frame of a run. See [`sequence`].
    sequence: Option<Entity<sequence::SequencePlayer>>,
    /// An animated picture whose player has not arrived yet. Decoding every
    /// frame is real work, so it happens off this thread and the panel opens on
    /// the still until it lands; this is what tells that still apart from a
    /// picture that will never play.
    anim_loading: bool,
    /// A video whose live player has not arrived yet. The probe that decides
    /// whether there is one runs on a background thread, so the panel opens on
    /// the still and swaps the player in when ffmpeg answers; until it does,
    /// this is what tells the still apart from a video that will never play.
    video_loading: bool,
    /// Live transport for audio assets. Its own entity because the soundtrack
    /// engine it drives outlives any window, exactly as the video one does.
    audio: Option<Entity<audio::AudioPlayer>>,
    /// The text viewer, for an asset whose file has characters to read. Its own
    /// entity because the editor state holds a line layout that outlives a
    /// render, exactly as the two players do.
    text: Option<Entity<text::TextViewer>>,
    /// Whether the font specimen actually registered (a font the text system
    /// refuses falls back to its thumbnail still, which only zooms when the
    /// asset carries dimensions).
    font_live: bool,
    /// The committed exposure, in stops, behind [`AssetPreviewData::exposed`].
    /// `0.0` means "no exposure applied" and no render exists; any other value
    /// means a decode was asked for (or finished) at this exact value.
    stops: f32,
    /// The exposure rail, owned here so the toolbar's popover can render it
    /// and this panel receives its releases.
    exposure: Entity<SliderState>,
    /// Every render the exposure control has produced this session, kept
    /// alive so `release` can hand them back to the window — gpui's sprite
    /// atlas never evicts on its own, and each committed slider value decodes
    /// a fresh full-size image.
    exposure_images: Vec<Arc<gpui_kit::RenderImage>>,
    /// Bumps on every exposure request; a decode that lands after a newer
    /// request (the user moved the slider again mid-decode) is dropped instead
    /// of overwriting the newer answer.
    exposure_generation: u64,
    /// The EXR part list, probed off-thread when a scene-linear preview
    /// opens. `None` until that probe lands, and empty for anything but an
    /// `.exr` — Radiance HDR and TGA have no parts to choose between.
    exr_parts: Option<Vec<trove_core::media::hdr::ExrPartInfo>>,
    /// The committed part selection, indexing [`Self::exr_parts`]. Zero is
    /// the file's first flat part; anything else re-decodes the original
    /// through the part decoder exactly as an exposure change does.
    part: usize,
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
        // The library handle clones out before the borrow ends: the text
        // viewer's save target needs it, and `spawn_with_data` runs without
        // the controller (the two library reads never alias `cx`).
        let library = controller.read(cx).library.clone();
        let data = AssetPreviewData::load(controller.read(cx), id)?;
        Some(Self::spawn_with_data(data, Some(library), cx))
    }

    /// Open the panel for preview inputs the caller already resolved (a
    /// virtual system font, for instance) — and no library behind it, so
    /// nothing there writes.
    pub(crate) fn spawn_with_data(
        mut data: AssetPreviewData,
        library: Option<trove_core::library::Library>,
        cx: &mut App,
    ) -> Entity<Self> {
        // The live player is spawned once, here — never per render. Probing the
        // stream is an `ffprobe` round trip that waits on a subprocess slot an
        // import burst can be holding, so it runs off this thread and the panel
        // opens on the still until it lands; an undecodable file (or no ffmpeg)
        // just keeps the still.
        let mut video_loading = data.kind == trove_core::model::AssetKind::Video;
        let video = if video_loading {
            match data.video_facts.take() {
                // Full facts from the import row: the player spawns now, on
                // its first-frame poster, and skips the probe entirely.
                Some(facts) => match data.original.clone() {
                    Some(original) => {
                        let audio = facts
                            .has_audio
                            .then(|| soundtrack::AudioEngine::spawn(original.clone(), cx));
                        // The first-frame poster hides the decoder's start-up;
                        // a library that predates posters falls back to the
                        // one-second thumbnail.
                        let poster = data.poster.clone().or_else(|| data.thumb.clone());
                        let player = VideoPlayer::spawn(original, facts, poster, audio, None, cx);
                        // The player is live, so the probe path below must not
                        // run: re-probing would replace this player and spawn a
                        // second audio engine for nothing — the facts already
                        // came from the import row.
                        video_loading = false;
                        Some(player)
                    }
                    // No file behind the record: fall back to the probe path,
                    // whose refusal notice names the cause. `video_loading`
                    // stays true here, which is what runs that path below.
                    None => None,
                },
                // Rows from before the facts existed: the still stands in
                // while one ffprobe answers, exactly as before — the existing
                // post-construction load_player call owns that path.
                None => None,
            }
        } else {
            None
        };
        // A frame run spawns its player here, once, exactly like the other
        // live players; a standalone asset never sees one.
        let sequence = if data.kind == trove_core::model::AssetKind::Image {
            data.sequence
                .take()
                .map(|info| sequence::SequencePlayer::spawn(info.frames, info.fps, cx))
        } else {
            None
        };
        // An animated picture is decoded once, off this thread, and advanced on
        // a clock this panel owns. gpui can animate a GIF itself, but only while
        // the window is active and only when something else happens to repaint
        // it, which is what made a GIF look frozen in a preview that was
        // otherwise just sitting there. A still never gets here at all, so the
        // ordinary thumbnail path is untouched.
        let anim_loading = !video_loading && anim::wants_player(&data);
        // An audio file is a soundtrack with no picture beside it; the engine
        // the video player uses is already file-agnostic, so this spawns the
        // transport over it. `None` (no ffmpeg, no decodable stream) leaves the
        // cover-art still, which is the whole picture either way.
        let audio = if data.kind == trove_core::model::AssetKind::Audio {
            audio::spawn_player(&data, cx)
        } else {
            None
        };
        // A text file is read off a background task and shown by the editor
        // element. The gate is the extension rather than the kind, because the
        // text family straddles `Document` and `Other` today. `None` (no file
        // behind the asset) leaves the still, as every other live surface does.
        let text = if text::is_text(&data) {
            text::spawn_viewer(&data, library, cx)
        } else {
            None
        };
        // A font whose specimen registers zooms the text itself; one that
        // falls back to its thumbnail still zooms only if that still has
        // recorded dimensions. The live one also gets its viewer state here —
        // language from the file's own declaration, text resolved once — so
        // the paint path never reads the config.
        let font_live =
            data.kind == trove_core::model::AssetKind::Font && font::specimen_available(&data, cx);
        if font_live {
            data.font_preview = Some(font::FontPreviewState::initial(
                data.font_declared_language,
                data.font_variable_weight,
                data.font_base_weight,
            ));
        }
        // The part probe runs only for an `.exr` original: everything else has
        // no parts, and the selector stays hidden without reading a header.
        let parts_probe = data.original.clone().filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exr"))
        });
        let viewport = cx.new(|_| size(px(0.), px(0.)));
        // The exposure rail: built per preview (it resets with the asset) and
        // subscribed here, so a release lands straight on this panel. Change
        // events are deliberately ignored — the picture only follows on
        // release, because every committed value is a full float re-decode.
        let exposure = cx.new(|_| {
            SliderState::new()
                .min(trove_core::media::hdr::MIN_STOPS)
                .max(trove_core::media::hdr::MAX_STOPS)
                .step(0.5)
                .default_value(0.)
        });
        let panel = cx.new(|cx| {
            cx.subscribe(&exposure, |this: &mut Self, _, event: &SliderEvent, cx| {
                if let SliderEvent::Release(value) = event {
                    this.set_exposure(value.start(), cx);
                }
            })
            .detach();
            Self {
                data,
                video,
                video_loading,
                anim: None,
                anim_loading,
                sequence,
                audio,
                text,
                font_live,
                stops: 0.0,
                exposure,
                exposure_images: Vec::new(),
                exposure_generation: 0,
                exr_parts: None,
                part: 0,
                pan: PanZoom::new(),
                viewport,
                drag_from: Point::default(),
                dragging: false,
            }
        });
        if video_loading {
            video::load_player(panel.clone(), cx);
        }
        if anim_loading {
            anim::load_player(panel.clone(), cx);
        }
        // The part probe: a header read on a scene-linear original, run off
        // this thread like every other file IO. Held weakly, exactly like the
        // player probes — a preview dismissed before the headers land must not
        // be kept alive by this task.
        if let Some(original) = parts_probe {
            let panel = panel.downgrade();
            cx.spawn(async move |cx| {
                let parts = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::media::hdr::exr_parts(&original).unwrap_or_default()
                    })
                    .await;
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |panel, cx| panel.set_parts(parts, cx));
                }
            })
            .detach();
        }
        panel
    }

    /// Stills, font specimens and videos all move through [`PanZoom`]; the
    /// flag decides whether the stage carries the gestures at all.
    pub(crate) fn zoomable(&self) -> bool {
        if self.video.is_some()
            || self.video_loading
            || self.anim.is_some()
            || self.anim_loading
            || self.audio.is_some()
            || self.text.is_some()
        {
            // The video player stages its own picture and the text viewer owns
            // its own scrolling; this panel's gestures would fight the controls
            // inside them. `video_loading` counts as the player because the
            // player is on its way: gestures on the poster would vanish the
            // moment it took the stage over.
            return false;
        }
        self.data.dimensions.is_some() || self.font_live
    }

    /// Switch the font specimen's preview language; the text re-resolves to
    /// that language's own sample (or the user's saved words for it).
    pub(crate) fn set_font_language(
        &mut self,
        language: font::FontPreviewLanguage,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &mut self.data.font_preview {
            if state.language == language {
                return;
            }
            *state = state.clone().switched(language);
            cx.notify();
        }
    }

    /// Store the user's sample words for the current preview language and
    /// show them. Empty input means "back to the built-in sample".
    pub(crate) fn set_font_custom_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(state) = &mut self.data.font_preview else {
            return;
        };
        let mut config = trove_core::config::AppConfig::load();
        config
            .font_preview
            .set_custom_text(state.language.as_str(), &text);
        crate::app::settings_write::note(config.save(), "font preview text");
        state.text = font::initial_text(state.language);
        cx.notify();
    }

    /// Render the specimen at `weight`. For a variable font that means
    /// registering the patched face first, so a refused registration keeps
    /// the old weight instead of dropping the preview to its thumbnail;
    /// for a static face the picker is hidden and this is never called.
    pub(crate) fn set_font_weight(&mut self, weight: u16, cx: &mut Context<Self>) {
        let Some(state) = &mut self.data.font_preview else {
            return;
        };
        if state.weight == weight {
            return;
        }
        let registered = self.data.font_family.as_deref().is_some_and(|family| {
            crate::panels::common::ensure_font_at_weight(
                family,
                self.data.original.as_deref(),
                weight,
                cx,
            )
        });
        if !registered {
            return;
        }
        state.weight = weight;
        cx.notify();
    }

    /// Drop the user's sample words for the current language: the built-in
    /// sample takes the stage back.
    pub(crate) fn reset_font_custom_text(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.data.font_preview else {
            return;
        };
        let mut config = trove_core::config::AppConfig::load();
        config
            .font_preview
            .set_custom_text(state.language.as_str(), "");
        crate::app::settings_write::note(config.save(), "font preview text reset");
        state.text = font::initial_text(state.language);
        cx.notify();
    }

    /// The store record behind the preview, for the title-bar tools.
    pub(crate) fn asset_id(&self) -> Option<Uuid> {
        self.data.asset_id
    }

    /// Whether the previewed asset is an image — the toolbar only carries
    /// pixel-edit tools for images.
    pub(crate) fn is_image(&self) -> bool {
        self.data.is_image
    }

    /// Why the backend would refuse a pixel edit on this asset; `None` when
    /// it may take one. The toolbar disables its buttons on `Some` and says
    /// why.
    pub(crate) fn edit_blocker(&self) -> Option<&'static str> {
        self.data.edit_blocker
    }

    /// Whether an edit writes its result back over the original file (the
    /// asset links to it) instead of re-encoding the library's own copy —
    /// the toolbar confirms before the first byte moves.
    pub(crate) fn write_back(&self) -> bool {
        self.data.write_back
    }

    /// The file an edit reads from and, for a linked asset, writes back to.
    pub(crate) fn original_path(&self) -> Option<&Path> {
        self.data.original.as_deref()
    }

    /// The live player, for the app view to render as a fullscreen stage in
    /// its own window (nothing is handed over: the same player keeps going).
    pub(crate) fn video_player(&self) -> Option<Entity<VideoPlayer>> {
        self.video.clone()
    }

    /// Whether a live player backs this preview — the precondition for
    /// grabbing a frame, and the reason the toolbar button appears only then.
    pub(crate) fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Whether a player is on screen whose picture can be held and resumed:
    /// a video or an animated image. The space bar answers for both.
    pub(crate) fn has_playback(&self) -> bool {
        self.video.is_some() || self.anim.is_some()
    }

    /// Whether this preview carries the exposure control at all: a
    /// scene-linear float source (EXR / Radiance HDR) whose original is
    /// reachable. Everything else comes out of the tonemap untouched, so a
    /// slider would be decoration.
    pub(crate) fn exposure_supported(&self) -> bool {
        self.data
            .original
            .as_deref()
            .and_then(|path| path.extension())
            .is_some_and(|ext| trove_core::media::hdr::is_scene_linear_ext(&ext.to_string_lossy()))
    }

    /// The committed exposure, in stops. `0.0` is "as authored" — the
    /// thumbnail's own transform, and the state the rail returns to.
    pub(crate) fn stops(&self) -> f32 {
        self.stops
    }

    /// The exposure rail, for the toolbar's popover to render.
    pub(crate) fn exposure_slider(&self) -> &Entity<SliderState> {
        &self.exposure
    }

    /// Commit an exposure: re-decode the original at `stops` off this thread
    /// and swap the result onto the stage when it lands. Zero clears instead
    /// of decoding — the thumbnail already *is* the 0-stops render. The
    /// generation counter drops a late decode from a superseded value rather
    /// than letting it overwrite the newer answer.
    ///
    /// This is the whole cost of the control, on purpose: `tonemap` needs the
    /// float samples, so each committed value re-decodes the original — the
    /// alternative (keeping the float buffer resident) is hundreds of
    /// megabytes per preview, and the picture following the thumb while
    /// dragging would multiply that by every step. Commit on release, one
    /// decode per release.
    fn set_exposure(&mut self, stops: f32, cx: &mut Context<Self>) {
        let stops = stops.clamp(
            trove_core::media::hdr::MIN_STOPS,
            trove_core::media::hdr::MAX_STOPS,
        );
        if stops == self.stops {
            return;
        }
        self.stops = stops;
        self.refresh_stage(cx);
    }

    /// Commit a part selection: the same re-decode an exposure change costs,
    /// and the same generation guard. Zero is the file's first flat part —
    /// unlike the exposure's zero, it still decodes, because the thumbnail on
    /// the stage was rendered from a part `image` picked and the selector's
    /// zero has no reason to agree with it.
    pub(crate) fn set_part(&mut self, part: usize, cx: &mut Context<Self>) {
        if part == self.part {
            return;
        }
        self.part = part;
        self.refresh_stage(cx);
    }

    /// The one decode path both controls share: stage a fresh full-size render
    /// of the original at the committed stops and part, or clear the stage
    /// back to the thumbnail when the answer is "the file as authored" —
    /// which is exposure zero *and* the decoder's own part pick.
    fn refresh_stage(&mut self, cx: &mut Context<Self>) {
        self.exposure_generation += 1;
        if self.stops == 0.0 && self.part == 0 {
            self.data.exposed = None;
            cx.notify();
            return;
        }
        let Some(path) = self.data.original.clone() else {
            return;
        };
        let generation = self.exposure_generation;
        let part = self.part;
        let stops = self.stops;
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move { image::decode_exposed(&path, stops, part) })
                .await;
            this.update(cx, |this, cx| {
                if this.exposure_generation == generation
                    && let Some(render) = decoded
                {
                    let render = Arc::new(render);
                    this.exposure_images.push(render.clone());
                    this.data.exposed = Some(render.into());
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Land the part list the probe gathered. Empty means "nothing to
    /// choose" — the toolbar's selector stays hidden.
    fn set_parts(
        &mut self,
        parts: Vec<trove_core::media::hdr::ExrPartInfo>,
        cx: &mut Context<Self>,
    ) {
        self.exr_parts = Some(parts);
        cx.notify();
    }

    /// The part list, once the probe has landed; `None` while it has not and
    /// for every non-EXR source. The toolbar renders the selector only for a
    /// landed list with more than one entry.
    pub(crate) fn exr_parts(&self) -> Option<&[trove_core::media::hdr::ExrPartInfo]> {
        self.exr_parts.as_deref()
    }

    /// The committed part selection, for the selector's highlight.
    pub(crate) fn part(&self) -> usize {
        self.part
    }

    /// Space bar: hold whatever is playing, or pick it back up.
    pub(crate) fn toggle_playback(&mut self, cx: &mut App) {
        if let Some(video) = &self.video {
            video.update(cx, |video, cx| video.toggle_play(cx));
        } else if let Some(anim) = &self.anim {
            anim.update(cx, |anim, cx| anim.toggle_playing(cx));
        }
    }

    /// Step the previewed clip one frame back or forward and hold it there.
    /// Video or animated image, whichever has the screen — the same two players
    /// space bar already answers, and the same no-op when neither is live.
    pub(crate) fn step_frame(&mut self, forward: bool, cx: &mut App) {
        if let Some(video) = &self.video {
            video.update(cx, |video, cx| video.step_frame(forward, cx));
        } else if let Some(anim) = &self.anim {
            anim.update(cx, |anim, cx| anim.step_frame(forward, cx));
        }
    }

    /// Save the frame under the video's playhead into the library. A no-op
    /// without a live player — see [`Self::has_video`].
    pub(crate) fn grab_frame(
        &mut self,
        controller: &Entity<LibraryController>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(video) = &self.video {
            video::grab_frame(video, controller, window, cx);
        }
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
        // The animated player holds one atlas entry per frame of the picture,
        // so it has more to hand back than the video one does.
        if let Some(anim) = &self.anim {
            anim.update(cx, |anim, _| anim.release(window));
        }
        // The exposure control holds one atlas entry per committed slider
        // value — each was a fresh full-size decode.
        for render in self.exposure_images.drain(..) {
            let _ = window.drop_image(render);
        }
    }

    /// Handle a scroll-wheel event. Over a live font specimen the wheel is
    /// the *font size* — for text, sizing is zooming, and the block pans
    /// when it outgrows the stage — so the zoom gesture never touches it.
    /// Everything else zooms toward the cursor, like the 3D model viewport.
    fn handle_scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if !self.zoomable() {
            return;
        }
        if self.font_live {
            if font::step_size(&mut self.data, event) {
                cx.notify();
            }
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
        let area = ((vw - STAGE_PAD).max(60.0), (vh - STAGE_PAD).max(60.0));
        if self.font_live {
            // Exact, not fitted: the size is the user's dial now, so an
            // oversized specimen pans instead of shrinking back to fit.
            // (`area` is unused on this branch; the tail needs it.)
            let (tw, th, _) = font::metrics_for(&self.data);
            return Some((tw, th));
        }
        let (iw, ih) = self.data.dimensions?;
        fit_box((iw as f32, ih as f32), area)
    }

    /// The area a freshly spawned video player gives its picture stage: the
    /// content box the player root fills. It is read off the measured viewport
    /// so the player can be seeded with it — the still that stood in for the
    /// video was cut to the same box, which is what makes the handover
    /// size-for-size. `None` before the viewport has been measured.
    fn video_stage_area(&self, cx: &App) -> Option<(f32, f32)> {
        let viewport = self.viewport.read(cx);
        let (w, h) = (
            f32::from(viewport.width) - STAGE_PAD,
            f32::from(viewport.height) - STAGE_PAD,
        );
        (w > 0.0 && h > 0.0).then_some((w, h))
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

    /// Double click: back to the fitted view, the 3D viewport's reset. A
    /// live specimen also gives its font size back — the wheel's dial is
    /// part of what "as authored" means for text.
    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.pan.reset();
        if self.font_live
            && let Some(state) = &mut self.data.font_preview
        {
            state.size = font::default_specimen_size();
        }
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
            let (tw, _, ts) = font::metrics_for(&self.data);
            let scale = w / tw;
            font::specimen_scaled(&self.data, w, h, ts * scale, cx)
        } else {
            let source: Option<gpui_kit::ImageSource> = if let Some(exposed) = &self.data.exposed {
                // The exposure render IS the original, already through the
                // display transform: beyond 1:1 it carries the detail too, so
                // it wins at every zoom and the raw original (which would
                // silently drop the exposure) never takes over.
                Some(exposed.clone())
            } else if self.pan.zoom > 1.05 {
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match (&self.video, &self.audio, &self.text) {
            (Some(player), _, _) => player.clone().into_any_element(),
            // The audio transport renders itself, so gpui passes the window to
            // it rather than this panel forwarding one it does not own.
            (None, Some(player), _) => player.clone().into_any_element(),
            // So does the text viewer, for the same reason: its editor state
            // needs a window the moment it first builds a line layout.
            (None, None, Some(viewer)) => viewer.clone().into_any_element(),
            (None, None, None) => {
                // The sequence player stages its own picture, ahead of even
                // the animated one: a frame of a run previews as the run.
                if let Some(player) = &self.sequence {
                    player.clone().into_any_element()
                }
                // The animated player stages its own picture, so it takes the
                // stage before the zoom does — see `zoomable`.
                else if let Some(player) = &self.anim {
                    player.clone().into_any_element()
                } else if self.video_loading || self.anim_loading {
                    image::still_filling(&self.data)
                } else if self.zoomable() && (self.pan.zoom != 1.0 || self.font_live) {
                    // A live specimen renders through the zoom math even at
                    // zoom 1.0: its block may outgrow the stage at a large
                    // font size, and the pan offsets live here.
                    self.zoomed_still(cx)
                } else {
                    element(&self.data, PreviewContext::Main, cx)
                }
            }
        };
        let zoomable = self.zoomable();
        // The stage div, hoisted: the font viewer appends its control strip
        // under it — outside the pan/drag gestures, which own the stage and
        // would otherwise fight the text input for clicks.
        let stage = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .p_4()
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
            .child(content);

        let mut root = v_flex().size_full().overflow_hidden().child(stage);
        if self.font_live {
            root = root.child(font::controls_bar(cx.entity(), &self.data, window, cx));
        }
        root
    }
}
