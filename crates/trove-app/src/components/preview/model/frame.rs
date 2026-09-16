//! The frame loop, and the camera input that drives it.
//!
//! One frame at a time, off the UI thread, for whichever renderer can draw it
//! — and the rule for when a frame is asked for at all. A camera move is a
//! *gesture*: while one is running the viewport draws cheap draft frames and
//! leaves the mesh's LOD alone, then draws one settled frame when it stops.
//! A drag says when it ends by releasing the button; a wheel notch or a key
//! has no such event, so it lingers for [`GESTURE_LINGER`].

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::*;

use trove_core::media::formats::types::{Bounds as MeshBounds, Mesh, Winding};
use trove_core::media::render3d::{self, Camera, RenderOptions};

use super::super::gpu3d::{GpuMesh, GpuRenderer};
use super::{Backend, Drag, ENHANCE_REFRESH, GESTURE_LINGER, ModelViewport};

/// Above this many triangles the CPU fallback renders at a reduced size and
/// lets the UI scale the frame up. A software rasterizer is the exception, not
/// the rule; it should stay usable rather than correct-but-frozen.
const CPU_BUSY_TRIANGLES: usize = 40_000;
/// Longest edge the CPU fallback renders at.
const CPU_MAX_EDGE: u32 = 900;
/// Frame size limits, matching the ones `render3d` enforces internally (its
/// `MAX_EDGE` / `MIN_EDGE` are private, so they are restated here).
const MAX_FRAME_EDGE: f32 = 2048.0;
const MIN_FRAME_EDGE: f32 = 16.0;

/// Frame size for a frame drawn mid-gesture, as a fraction of the settled one.
///
/// Everything downstream of the rasteriser is proportional to pixels: the MSAA
/// resolve, the read-back, the BGRA unpack and gpui's texture upload. Half the
/// edge is a quarter of that work, and the UI stretches the small frame back
/// over the canvas — softer while the model is moving, sharp the moment it
/// stops.
const INTERACTIVE_SCALE: f32 = 0.5;
/// Never shrink past this on the longest edge, however small the window.
const INTERACTIVE_MIN_EDGE: u32 = 240;

impl ModelViewport {
    /// Come back for the frame the rate limit held back.
    ///
    /// Without this the viewport can sit on a draft frame for good: a gesture
    /// ends, the settled frame is asked for in the same breath as the last
    /// draft frame completing, the rate limit holds it back, and with no
    /// further mouse movement nothing ever asks again — the model stays blurry
    /// until the user happens to touch it. One retry is enough no matter how
    /// many requests pile up behind it: they all want the same latest pose.
    fn retry_later(&mut self, cx: &mut Context<Self>) {
        if self.retry_pending {
            return;
        }
        self.retry_pending = true;
        let wait = self
            .last_frame
            .map(|instant| FRAME_INTERVAL.saturating_sub(instant.elapsed()))
            .unwrap_or(FRAME_INTERVAL);
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(wait).await;
            weak.update(cx, |this, cx| {
                this.retry_pending = false;
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Record the measured viewport and re-render when it changed.
    pub(super) fn measure(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        let (width, height) = (size.width.as_f32(), size.height.as_f32());
        if width < 1.0 || height < 1.0 {
            return;
        }
        // Prepaint runs every frame; a sub-pixel wobble is not a resize.
        if (width - self.logical.0).abs() < 0.5 && (height - self.logical.1).abs() < 0.5 {
            return;
        }
        self.logical = (width, height);
        self.dirty = true;
        self.pump(cx);
    }

    /// Render the current camera on a background thread, unless a frame is
    /// already in flight or the viewport is not ready. While the mesh is
    /// still parsing the viewport shows a loading indicator and renders
    /// nothing.
    pub(super) fn pump(&mut self, cx: &mut Context<Self>) {
        // Nothing to draw, and nothing that drawing could fix. Without this
        // the placeholder mesh would be rendered into a blank frame and
        // painted over the very message explaining why there is no model.
        if matches!(self.backend, Backend::Loading | Backend::Unavailable(_)) {
            return;
        }
        // While streaming, keep one step in flight. Each finished step swaps
        // in a new mesh (which marks the viewport dirty) and comes back here
        // to start the next one, so the loop is driven by the work itself
        // rather than by the frame rate.
        if self.is_streaming {
            self.begin_stream_step(cx);
        } else if self.is_indexed {
            self.begin_index_step(cx);
        } else if !self.is_interacting() {
            // Update LOD based on camera distance for triangle meshes — but
            // not mid-gesture: a level change re-uploads the whole mesh, and a
            // camera that is moving is the worst possible moment to spend
            // that. The gesture's end pumps again and picks the level up then.
            self.update_lod(cx);
        }
        // Whether this request is drawn, deferred to the frame in flight, or
        // held back by the rate limit — the last of which has to be retried,
        // because nothing else comes back for it.
        match frame_action(
            self.dirty,
            self.in_flight,
            self.is_interacting(),
            self.last_frame.map(|instant| instant.elapsed()),
        ) {
            FrameAction::Defer | FrameAction::Idle => return,
            FrameAction::Retry => {
                self.retry_later(cx);
                return;
            }
            FrameAction::Render => {}
        }
        let device = self.device_size();
        if device.0 == 0 || device.1 == 0 {
            return;
        }
        // Re-read on a slow backoff rather than every frame: the settings
        // window is a separate OS window, so a change there can only reach an
        // already-open viewport by polling, but a config-file read on the frame
        // path is not free and the value only changes when a person clicks.
        //
        // Done before the camera is copied below, so a zoom-limit change
        // takes effect on this frame rather than the next one.
        if self
            .enhance_checked
            .is_none_or(|at| at.elapsed() >= ENHANCE_REFRESH)
        {
            let cfg = trove_core::config::AppConfig::load();
            self.enhance_points = cfg.point_enhance();
            self.height_color = cfg.height_color();
            self.show_scene_axes = cfg.scene_axes();
            self.show_corner_axis = cfg.corner_axis();
            // The zoom limits live in the same file: clamp so lowering the
            // range while a model is open pulls the camera back in, instead
            // of leaving it parked outside the configured limits.
            let (zmin, zmax) = super::ui::distance_bounds(&cfg);
            self.camera.zoom = self.camera.zoom.clamp(zmin, zmax);
            self.enhance_checked = Some(Instant::now());
        }
        let gpu = self.gpu.clone();
        let gpu_mesh = self.gpu_mesh.clone();
        let mesh = self.mesh.clone();
        let camera = self.camera;
        let bounds = self.framing_bounds();
        // A gesture gets a draft: a quarter of the geometry (so a heavy mesh
        // turns fluently on the CPU path), half the resolution and no MSAA.
        // The gesture's end sets `dirty` again and draws the settled version.
        let interactive = self.is_interacting();
        let quality = if interactive { 0.25 } else { 1.0 };
        let scratch = self.scratch.clone();
        let options = RenderOptions {
            // Skipping back faces is free for a closed mesh and wrong for
            // anything else, so it follows the winding exactly.
            cull_backfaces: self.mesh_winding != Winding::TwoSided,
            // Eye-dome lighting and gap filling are a full-screen pass each;
            // they are worth it on a settled frame and wasted on a draft the
            // user is dragging past.
            enhance_points: !interactive && self.enhance_points,
            // Height colouring and the scene axes are separate switches now:
            // the axes answer "where is X/Y/Z" whether or not the model is
            // painted by height, so each follows its own config value.
            height_color: self.height_color,
            show_axes: self.show_scene_axes,
        };

        self.dirty = false;
        self.in_flight = true;
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    draw(Shot {
                        gpu: gpu.as_deref().zip(gpu_mesh.as_deref()),
                        mesh: &mesh,
                        camera: &camera,
                        bounds,
                        size: device,
                        quality,
                        interactive,
                        options,
                        scratch: &scratch,
                    })
                })
                .await;

            weak.update(cx, |this, cx| {
                this.in_flight = false;
                match outcome {
                    Rendered::Frame(frame) => {
                        this.error = None;
                        this.pending = Some(frame);
                        this.last_frame = Some(std::time::Instant::now());
                    }
                    Rendered::Demoted(frame, reason) => {
                        // The device answered for the upload but not for this
                        // frame: drop it and let the CPU take over for good.
                        // `gpu_demoted` is what makes "for good" true — without
                        // it the next LOD switch would build a fresh device and
                        // hand the user a second failure.
                        this.gpu = None;
                        this.gpu_mesh = None;
                        this.gpu_demoted = true;
                        this.backend = Backend::Cpu(reason.clone());
                        this.error = Some(reason);
                        this.pending = Some(frame);
                        this.last_frame = Some(std::time::Instant::now());
                    }
                    Rendered::Failed(reason) => {
                        this.error = Some(reason);
                    }
                }
                // A camera move that arrived mid-render still needs drawing.
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Frame size in device pixels.
    ///
    /// A frame larger than the renderers accept is scaled down as a whole:
    /// clamping the two edges independently would change the aspect ratio,
    /// and the framing (and the stretch of the painted image) follows it.
    fn device_size(&self) -> (u32, u32) {
        let (width, height) = (self.logical.0 * self.scale, self.logical.1 * self.scale);
        if width < 1.0 || height < 1.0 {
            return (0, 0);
        }
        let longest = width.max(height);
        let factor = if longest > MAX_FRAME_EDGE {
            MAX_FRAME_EDGE / longest
        } else {
            1.0
        };
        let width = (width * factor).round().max(MIN_FRAME_EDGE) as u32;
        let height = (height * factor).round().max(MIN_FRAME_EDGE) as u32;
        (width, height)
    }

    /// Whether a gesture is in progress, i.e. whether this frame is a draft.
    pub(super) fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Whether the camera is moving right now: a held pointer, or a wheel or
    /// key input recent enough that more are probably coming.
    ///
    /// Everything that draws a frame asks this rather than
    /// [`Self::is_dragging`], so a wheel notch and a held arrow get what a drag
    /// gets: draft frames while the camera moves, one settled frame when it
    /// stops, and no mesh LOD swap in between. Without it a zoom paid for a
    /// full-resolution, multisampled, eye-dome-lit frame on every notch and was
    /// free to re-upload the whole mesh mid-move.
    fn is_interacting(&self) -> bool {
        self.drag.is_some() || within_gesture(Instant::now(), self.last_camera_move, GESTURE_LINGER)
    }

    /// Note a camera move that has no pointer-down state of its own — a wheel
    /// notch or a key press — and draw it as a gesture.
    pub(super) fn begin_gesture(&mut self, cx: &mut Context<Self>) {
        self.last_camera_move = Some(Instant::now());
        self.dirty = true;
        if !self.gesture_armed {
            self.gesture_armed = true;
            self.spawn_gesture_end(GESTURE_LINGER, cx);
        }
        self.pump(cx);
        cx.notify();
    }

    /// Wait out `after`, then end the gesture if the camera has stopped.
    fn spawn_gesture_end(&mut self, after: std::time::Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(after.max(std::time::Duration::from_millis(1)))
                .await;
            weak.update(cx, |this, cx| this.finish_gesture(cx)).ok();
        })
        .detach();
    }

    /// End the gesture window if the wheel or key has stopped, or wait out
    /// what is left of it if it has not — so the settled frame lands right
    /// after the input stops rather than a whole window after it.
    fn finish_gesture(&mut self, cx: &mut Context<Self>) {
        if self.is_interacting() {
            let remaining = self
                .last_camera_move
                .map(|at| GESTURE_LINGER.saturating_sub(at.elapsed()))
                .unwrap_or(GESTURE_LINGER);
            self.spawn_gesture_end(remaining, cx);
            return;
        }
        self.gesture_armed = false;
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }

    /// Start a gesture: remember what the button means and where it started.
    pub(super) fn begin_drag(&mut self, mode: Drag, from: Point<Pixels>, cx: &mut Context<Self>) {
        self.drag = Some(mode);
        self.drag_from = from;
        // The frame that arrives from here on is a draft, so the first move
        // does not have to wait for a settled frame to finish.
        cx.notify();
    }

    /// Radians of orbit per pixel dragged: about a half turn across the
    /// viewport's shorter edge, so the feel does not depend on window size.
    pub(super) fn turn_per_pixel(&self) -> f32 {
        const HALF_TURN: f32 = std::f32::consts::PI * 1.25;
        HALF_TURN / self.logical.0.min(self.logical.1).clamp(160.0, 4096.0)
    }

    /// Back to the default three-quarter view.
    pub(super) fn reset_camera(&mut self, cx: &mut Context<Self>) {
        if self.camera.is_default() {
            return;
        }
        self.camera.reset();
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }

    pub(super) fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag.is_none() {
            return;
        }
        self.drag = None;
        // The drag only moved the camera; redraw so the cursor and any pending
        // camera move settle together, at full resolution and with the LOD the
        // camera has earned by stopping.
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }
}

/// One frame's outcome, produced off the UI thread.
enum Rendered {
    /// The GPU drew this frame.
    Frame(Arc<RenderImage>),
    /// The GPU refused and the CPU drew it instead, so the viewport demotes
    /// itself rather than paying for a failing GPU attempt every frame.
    Demoted(Arc<RenderImage>, String),
    /// Neither renderer produced a frame.
    Failed(String),
}

/// One frame's request: what to draw, from where, how big, and how well.
struct Shot<'a> {
    /// The GPU renderer and the mesh's buffers in it, when a device came up.
    gpu: Option<(&'a GpuRenderer, &'a GpuMesh)>,
    mesh: &'a Mesh,
    camera: &'a Camera,
    bounds: MeshBounds,
    /// Device-pixel size of a settled frame; a draft is derived from it.
    size: (u32, u32),
    /// Fraction of the geometry the CPU path rasterises.
    quality: f32,
    /// A frame drawn mid-gesture.
    interactive: bool,
    /// What the rasteriser may do beyond drawing the raw geometry.
    options: RenderOptions,
    /// Buffers the CPU path reuses.
    scratch: &'a std::sync::Mutex<render3d::Scratch>,
}

impl Shot<'_> {
    /// Device-pixel size this frame is actually drawn at.
    fn size(&self) -> (u32, u32) {
        if self.interactive {
            interactive_size(self.size)
        } else {
            self.size
        }
    }
}

/// Draw one frame with whichever renderer can. Runs off the UI thread.
///
/// The GPU is tried first and the CPU is the safety net, so a device that
/// comes up but cannot finish a frame still leaves the user with a picture.
fn draw(shot: Shot<'_>) -> Rendered {
    let Shot {
        gpu,
        mesh,
        camera,
        bounds,
        quality,
        interactive,
        options,
        scratch,
        ..
    } = shot;
    // A frame drawn mid-gesture is a draft: smaller, so every stage after the
    // rasteriser costs a quarter as much. The aspect ratio is preserved (both
    // edges scale together), and the UI stretches the frame over the canvas,
    // so the only difference the user sees is softness while the model moves.
    let size = shot.size();
    let aspect = size.0 as f32 / size.1.max(1) as f32;

    if let Some((renderer, uploaded)) = gpu {
        // The framing is the same one the CPU would compute: one shared
        // function, so the two renderers cannot disagree about what "framed"
        // means.
        let framing = camera.framing(bounds, aspect);
        // The bands count from the model's own floor, exactly as the CPU
        // rasteriser counts them, so the two pictures agree.
        let bands = render3d::bands_uniform(options.height_color, options.show_axes, bounds.min[1]);
        if let Some(bytes) = renderer.render(
            uploaded,
            &framing,
            size,
            interactive,
            options.enhance_points,
            bands,
        ) && let Some(frame) = frame_image(size, bytes)
        {
            return Rendered::Frame(frame);
        }
        // Either the device answered for the upload but not for this frame, or
        // it handed back something that is not a frame at all. Both mean the
        // same thing to the caller: draw this one on the CPU and demote the
        // viewport, so a broken GPU costs a single frame instead of every one.
        let reason = rust_i18n::t!("viewport.reason_gpu_lost").to_string();
        return match cpu_frame(mesh, camera, size, quality, options, scratch) {
            Some(frame) => Rendered::Demoted(frame, reason),
            None => Rendered::Failed(reason),
        };
    }

    match cpu_frame(mesh, camera, size, quality, options, scratch) {
        Some(frame) => Rendered::Frame(frame),
        None => Rendered::Failed(rust_i18n::t!("viewport.reason_no_frame").to_string()),
    }
}

/// Shortest gap between frames when nothing is being dragged.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// What `pump` should do about the frame it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameAction {
    /// Draw it now.
    Render,
    /// A frame is already in flight; the latest pose is kept and picked up when
    /// it lands.
    Defer,
    /// Nothing has changed.
    Idle,
    /// Held back by the frame rate. It has to be *retried*, not dropped:
    /// nothing else will ask again, and the frame being held here is the sharp
    /// one at the end of a gesture.
    Retry,
}

/// Decide whether `pump` draws, without touching the viewport.
///
/// The distinction that matters is between deferring a frame (something else
/// will come back for it — a frame in flight always does) and dropping one
/// (nothing will). Everything except the rate limit defers.
fn frame_action(
    dirty: bool,
    in_flight: bool,
    interacting: bool,
    since_last_frame: Option<std::time::Duration>,
) -> FrameAction {
    if in_flight {
        return FrameAction::Defer;
    }
    if !dirty {
        return FrameAction::Idle;
    }
    // A gesture draws every pose, however fast they arrive: the model has to
    // follow the input. Otherwise the frame rate is held, and the caller is
    // told to come back rather than losing the frame.
    if !interacting && since_last_frame.is_some_and(|since| since < FRAME_INTERVAL) {
        return FrameAction::Retry;
    }
    FrameAction::Render
}

/// Whether `last_move` is recent enough that the camera still counts as in
/// motion. Pure, so the window can be tested without waiting it out.
fn within_gesture(now: Instant, last_move: Option<Instant>, linger: std::time::Duration) -> bool {
    last_move.is_some_and(|at| now.saturating_duration_since(at) < linger)
}

/// The device-pixel size a draft frame is rendered at: half the settled size,
/// never smaller than [`INTERACTIVE_MIN_EDGE`] on the longest edge.
///
/// Both edges scale by the same factor, so the picture the UI stretches back
/// over the canvas keeps the aspect ratio the framing was computed for —
/// clamping the edges independently would distort the model.
fn interactive_size((width, height): (u32, u32)) -> (u32, u32) {
    let (width, height) = (width.max(1), height.max(1));
    let scaled = (
        ((width as f32 * INTERACTIVE_SCALE).round() as u32).max(1),
        ((height as f32 * INTERACTIVE_SCALE).round() as u32).max(1),
    );
    let longest = scaled.0.max(scaled.1);
    if longest >= INTERACTIVE_MIN_EDGE {
        return scaled;
    }
    // A small window keeps its draft legible instead of turning into a smudge.
    let upscale = INTERACTIVE_MIN_EDGE as f32 / longest.max(1) as f32;
    (
        ((scaled.0 as f32 * upscale).round() as u32).max(1),
        ((scaled.1 as f32 * upscale).round() as u32).max(1),
    )
}

/// The CPU fallback: the software rasterizer, at a reduced size for anything
/// heavy so the viewport stays responsive.
///
/// `None` only when the rasterizer's own output cannot be wrapped as an image,
/// which its contract rules out; the caller treats it as a failed frame rather
/// than panicking on a background thread.
fn cpu_frame(
    mesh: &Mesh,
    camera: &Camera,
    size: (u32, u32),
    quality: f32,
    options: RenderOptions,
    scratch: &std::sync::Mutex<render3d::Scratch>,
) -> Option<Arc<RenderImage>> {
    let longest = size.0.max(size.1);
    // A software rasterizer is the exception, not the rule: it should stay
    // usable on a big mesh rather than correct-but-frozen. A cloud counts its
    // points here, which is the same kind of per-frame work.
    let cap = if mesh.primitive_count() > CPU_BUSY_TRIANGLES {
        CPU_MAX_EDGE
    } else {
        render3d::clamp_edge(longest)
    };
    let (width, height) = if longest > cap {
        // Scale both edges together, or the model comes out stretched.
        let factor = cap as f32 / longest as f32;
        (
            ((size.0 as f32 * factor).round() as u32).max(16),
            ((size.1 as f32 * factor).round() as u32).max(16),
        )
    } else {
        size
    };
    // The interactive frame supersamples once; the grid thumbnail — rendered
    // once per asset at import time, off the interactive path — uses 2. The
    // buffers come from the viewport, so a drag reuses them instead of
    // allocating a fresh colour and depth buffer per frame.
    let rendered = {
        let mut scratch = scratch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render3d::render_with_scratch(
            mesh,
            camera,
            width,
            height,
            1,
            quality,
            options,
            &mut scratch,
        )
    };
    frame_image((rendered.width, rendered.height), rendered.bgra)
}

/// Wrap a rendered BGRA frame into the image type gpui draws.
fn frame_image(size: (u32, u32), bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(size.0, size.1, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags in a `test`
    // attribute macro from the gpui prelude, which makes expanding `#[test]`
    // below recurse.
    use super::super::GESTURE_LINGER;
    use super::{FRAME_INTERVAL, FrameAction, frame_action, interactive_size, within_gesture};
    use std::time::Instant;

    /// A wheel notch and a key press have no pointer-down state, so they stay
    /// a gesture for a short window after the last one arrives — that window is
    /// what gives them the draft frames a drag gets.
    #[test]
    fn a_wheel_or_key_move_stays_a_gesture_for_a_short_while() {
        let now = Instant::now();
        let half = GESTURE_LINGER / 2;
        assert!(!within_gesture(now, None, GESTURE_LINGER));
        assert!(within_gesture(now, Some(now - half), GESTURE_LINGER));
        // Exactly at the edge is not inside the window.
        assert!(!within_gesture(
            now,
            Some(now - GESTURE_LINGER),
            GESTURE_LINGER
        ));
        assert!(!within_gesture(
            now,
            Some(now - GESTURE_LINGER * 2),
            GESTURE_LINGER
        ));
    }

    /// The rule that decides whether a requested frame is drawn, deferred or
    /// held back. The case worth pinning is the last one: a settled frame asked
    /// for right after a draft frame lands is *held*, not dropped — the pump
    /// that runs on frame completion is exactly the one the rate limit catches,
    /// and the end of a gesture asks in the same breath.
    #[test]
    fn the_frame_rule_holds_a_frame_back_instead_of_dropping_it() {
        let quick = Some(std::time::Duration::from_millis(1));
        let expired = Some(FRAME_INTERVAL + std::time::Duration::from_millis(1));

        // A frame in flight takes the request with it.
        assert_eq!(frame_action(true, true, false, None), FrameAction::Defer);
        assert_eq!(frame_action(true, true, true, quick), FrameAction::Defer);
        // Nothing to draw.
        assert_eq!(
            frame_action(false, false, false, expired),
            FrameAction::Idle
        );
        // Too soon after the last one, and not dragging: held, to be retried.
        assert_eq!(frame_action(true, false, false, quick), FrameAction::Retry);
        // A gesture is never held back, however fast the poses arrive.
        assert_eq!(
            frame_action(true, false, true, Some(std::time::Duration::ZERO)),
            FrameAction::Render
        );
        // Once the wait is over, the held frame goes out.
        assert_eq!(
            frame_action(true, false, false, expired),
            FrameAction::Render
        );
        // As does the first frame there has ever been.
        assert_eq!(frame_action(true, false, false, None), FrameAction::Render);
    }

    #[test]
    fn a_draft_frame_is_half_the_settled_one() {
        assert_eq!(interactive_size((1600, 900)), (800, 450));
        assert_eq!(interactive_size((1024, 768)), (512, 384));
    }

    /// The draft keeps the aspect ratio: the framing is computed from the
    /// draft's own size, and the UI stretches it over the canvas, so an
    /// anisotropic scale would show up as a stretched model.
    #[test]
    fn a_draft_frame_keeps_the_aspect_ratio() {
        for (width, height) in [(1920u32, 1080u32), (1000, 1000), (2560, 1440)] {
            let (draft_w, draft_h) = interactive_size((width, height));
            let settled = width as f32 / height as f32;
            let draft = draft_w as f32 / draft_h as f32;
            assert!(
                (settled - draft).abs() < 0.01,
                "{width}x{height} became {draft_w}x{draft_h}"
            );
            assert!(draft_w <= width && draft_h <= height);
        }
    }

    /// A tiny window keeps a legible draft instead of a smudge, and no size
    /// ever comes out zero — the renderers refuse a zero-sized frame.
    #[test]
    fn a_draft_frame_has_a_floor() {
        let (width, height) = interactive_size((320, 200));
        assert_eq!(width.max(height), 240, "the floor is on the longest edge");
        for size in [(1u32, 1u32), (16, 16), (200, 100)] {
            let (width, height) = interactive_size(size);
            assert!(
                width >= 1 && height >= 1,
                "{size:?} became {width}x{height}"
            );
        }
    }
}
