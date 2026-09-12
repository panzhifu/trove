//! The main panel's 3D viewport.
//!
//! Previewing a model hands the whole main content area over to this entity:
//! the grid and every other asset disappear and the model is drawn on its own,
//! large, where it can actually be examined. The viewport closes back to the
//! grid on Escape or the toolbar's back button.
//!
//! Rendering is off-screen wgpu (see [`super::gpu3d`]), with the CPU
//! rasterizer from `trove-core` as the fallback when there is no usable
//! graphics device — over a remote session, in a VM, or on a machine whose
//! driver the process cannot open. Both paths share
//! [`trove_core::media::render3d::Framing`], so the two produce the same
//! picture; only the speed differs.
//!
//! The off-screen render runs on a background thread. wgpu's handles are
//! `Send + Sync`, but reading a frame back blocks, and blocking the UI thread
//! for a million-triangle mesh would waste the very GPU this is here to use.
//! Camera changes bump a sequence number instead of queueing work, so a fast
//! drag renders the latest pose rather than every pose it passed through.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::base::{ElementExt as _, h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::media::chunked::{self, LodConfig};
use trove_core::media::mesh::{self, Bounds as MeshBounds, Mesh};
use trove_core::media::render3d::{self, Camera};

use super::gpu3d::{GpuMesh, GpuRenderer, GpuUnavailable};

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

/// Which renderer is painting the viewport, for the status line.
#[derive(Debug, Clone)]
pub enum Backend {
    /// The mesh is still being parsed on a background thread.
    Loading,
    /// The GPU device is still coming up on a background thread.
    Starting,
    /// Off-screen wgpu; the string is the adapter description.
    Gpu(String),
    /// The CPU rasterizer, carrying the reason the GPU was skipped.
    Cpu(String),
}

/// What the viewport tells its host, the workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelViewportEvent {
    /// The user left the viewport (Escape, or the back button).
    Closed,
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

/// The 3D model viewport: a camera, a mesh, and whichever renderer came up.
pub struct ModelViewport {
    /// Display name, as the grid shows it.
    name: String,
    mesh: Arc<Mesh>,
    camera: Camera,

    /// The GPU device and the mesh's buffers in it. Both are `None` while the
    /// device is starting and when it could not be created at all — the CPU
    /// path covers both.
    gpu: Option<Arc<GpuRenderer>>,
    gpu_mesh: Option<Arc<GpuMesh>>,
    backend: Backend,
    /// A renderer that reported a problem, shown in the status line.
    error: Option<String>,

    /// Viewport size in logical pixels, measured at prepaint.
    logical: (f32, f32),
    /// Display scale factor, so the frame is rendered at the device resolution
    /// and stays sharp on a HiDPI screen.
    scale: f32,
    /// Whether the camera or the size changed since the last frame.
    dirty: bool,
    /// A render is already running; further changes are picked up by the
    /// frame it produces.
    in_flight: bool,

    /// Frame decoded by the background task, waiting for its first paint.
    pending: Option<Arc<RenderImage>>,
    /// Frame currently on screen.
    shown: Option<Arc<RenderImage>>,
    /// Wall time of the last completed frame, in milliseconds.
    frame_ms: f32,
    /// Instant the last frame finished rendering. Gates `pump` to ~60 fps.
    last_frame: Option<std::time::Instant>,

    dragging: bool,
    /// Last drag position, in window coordinates.
    drag_from: Point<Pixels>,
}

impl EventEmitter<ModelViewportEvent> for ModelViewport {}

impl ModelViewport {
    /// Open the viewport for `path`, showing a loading indicator until the
    /// mesh parses. The parse runs on a background thread so the UI never
    /// blocks — even a 20 GB export turns the spinner instead of freezing
    /// the window.
    pub fn spawn(name: String, path: PathBuf, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            // Placeholder mesh: one invisible vertex so the renderer has
            // something valid to hold until the real parse lands. The true
            // mesh arrives via `set_mesh` from the background task below.
            let loading_mesh = Arc::new(Mesh::default());
            let this = Self {
                name,
                mesh: loading_mesh,
                camera: Camera::default(),
                gpu: None,
                gpu_mesh: None,
                backend: Backend::Loading,
                error: None,
                logical: (0.0, 0.0),
                scale: 1.0,
                dirty: true,
                in_flight: false,
                pending: None,
                shown: None,
                frame_ms: 0.0,
                last_frame: None,
                dragging: false,
                drag_from: Point::default(),
            };
            this.start_load(path, cx);
            this
        })
    }

    /// Parse the mesh off the UI thread, then hand it back to the viewport.
    fn start_load(&self, path: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { Self::load_mesh(&path) })
                .await;
            weak.update(cx, |this, cx| match result {
                Ok(mesh) => this.set_mesh(mesh, cx),
                Err(err) => {
                    this.error = Some(err);
                    this.backend = Backend::Cpu("load failed".into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Swap in a freshly parsed mesh and bring the GPU up.
    fn set_mesh(&mut self, mesh: Mesh, cx: &mut Context<Self>) {
        self.mesh = Arc::new(mesh);
        self.backend = Backend::Starting;
        self.start_gpu(cx);
        cx.notify();
    }

    /// Load a mesh, dispatching on the file size. Files over 512 MiB go
    /// through the chunked LOD loader with a 128 MiB parsed-geometry budget;
    /// smaller files take the stock loader, which keeps every vertex.
    fn load_mesh(path: &PathBuf) -> Result<Mesh, String> {
        const CHUNKED_THRESHOLD: u64 = 512 << 20;
        let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        if size > CHUNKED_THRESHOLD {
            let config = LodConfig {
                memory_budget: 128 << 20,
                max_lod_step: 32,
            };
            chunked::load_ply_chunked(path, config)
        } else {
            mesh::load(path)
        }
    }

    /// Primitives and vertices of the loaded geometry, for the status line.
    /// Primitives are triangles for a mesh, points for a cloud.
    pub fn stats(&self) -> (usize, usize) {
        (self.mesh.primitive_count(), self.mesh.vertex_count())
    }

    /// Bring the GPU up on a background thread and upload the mesh. Until it
    /// finishes — or forever, if it fails — the CPU renders the viewport.
    ///
    /// When the mesh is larger than [`GpuRenderer::GPU_UPLOAD_BUDGET`] the
    /// upload is skipped and the viewport falls back to the CPU rasterizer:
    /// a few-hundred-pixel software preview costs a bounded amount of work
    /// regardless of how many triangles the model has.
    fn start_gpu(&mut self, cx: &mut Context<Self>) {
        let mesh = self.mesh.clone();
        cx.spawn(async move |weak, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let renderer = GpuRenderer::new()?;
                    let uploaded = renderer.upload_capped(&mesh);
                    Ok::<_, GpuUnavailable>((Arc::new(renderer), uploaded))
                })
                .await;
            weak.update(cx, |this, cx| {
                match built {
                    Ok((renderer, Some(uploaded))) => {
                        this.backend = Backend::Gpu(renderer.adapter.clone());
                        this.gpu = Some(renderer);
                        this.gpu_mesh = Some(Arc::new(uploaded));
                    }
                    Ok((renderer, None)) => {
                        // The mesh exceeded the GPU budget; keep the renderer
                        // for its adapter info but fall back to CPU.
                        this.backend = Backend::Cpu(format!(
                            "mesh exceeds the {} MiB GPU preview limit",
                            GpuRenderer::GPU_UPLOAD_BUDGET >> 20
                        ));
                        this.gpu = Some(renderer);
                    }
                    Err(unavailable) => this.backend = Backend::Cpu(reason_text(&unavailable)),
                }
                this.dirty = true;
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Record the measured viewport and re-render when it changed.
    fn measure(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
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
    fn pump(&mut self, cx: &mut Context<Self>) {
        if matches!(self.backend, Backend::Loading) {
            return;
        }
        if self.in_flight || !self.dirty {
            return;
        }
        // Throttle to ~60 fps.  Rendering faster than the display refreshes
        // only burns CPU on frames that are never seen; on a laptop that
        // means heat, fan noise and a sluggish UI.  A moved camera sets
        // `dirty` again, so the next pose is picked up as soon as this frame
        // lands.
        if self
            .last_frame
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(16))
        {
            return;
        }
        let device = self.device_size();
        if device.0 == 0 || device.1 == 0 {
            return;
        }
        let gpu = self.gpu.clone();
        let gpu_mesh = self.gpu_mesh.clone();
        let mesh = self.mesh.clone();
        let camera = self.camera;
        let bounds = self.mesh.bounds;
        // Dragging: subsample to a quarter of the geometry so a heavy mesh
        // turns fluently.  After the drag the viewport re-renders at full
        // quality (the `dirty` flag is set again on mouse up).
        let quality = if self.dragging { 0.25 } else { 1.0 };

        self.dirty = false;
        self.in_flight = true;
        cx.spawn(async move |weak, cx| {
            let started = Instant::now();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    draw(
                        gpu.as_deref().zip(gpu_mesh.as_deref()),
                        &mesh,
                        &camera,
                        bounds,
                        device,
                        quality,
                    )
                })
                .await;
            let elapsed = started.elapsed().as_secs_f32() * 1000.0;

            weak.update(cx, |this, cx| {
                this.in_flight = false;
                match outcome {
                    Rendered::Frame(frame) => {
                        this.error = None;
                        this.frame_ms = elapsed;
                        this.pending = Some(frame);
                        this.last_frame = Some(std::time::Instant::now());
                    }
                    Rendered::Demoted(frame, reason) => {
                        // The device answered for the upload but not for this
                        // frame: drop it and let the CPU take over for good.
                        this.gpu = None;
                        this.gpu_mesh = None;
                        this.backend = Backend::Cpu(reason.clone());
                        this.error = Some(reason);
                        this.frame_ms = elapsed;
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

    /// MSAA samples a frame gets. Informational: the GPU's pipelines are
    /// built with it once, the CPU picks its own supersampling.
    pub fn samples(&self) -> u32 {
        self.gpu.as_ref().map_or(1, |gpu| gpu.samples())
    }

    /// Radians of orbit per pixel dragged: about a half turn across the
    /// viewport's shorter edge, so the feel does not depend on window size.
    fn turn_per_pixel(&self) -> f32 {
        const HALF_TURN: f32 = std::f32::consts::PI * 1.25;
        HALF_TURN / self.logical.0.min(self.logical.1).clamp(160.0, 4096.0)
    }

    /// Back to the default three-quarter view.
    fn reset_camera(&mut self, cx: &mut Context<Self>) {
        if self.camera.is_default() {
            return;
        }
        self.camera.reset();
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        // The drag only moved the camera; redraw so the cursor and any pending
        // camera move settle together.
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }

    /// The canvas: the model, and every interaction that moves the camera.
    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let image = self.shown.clone();
        let cursor = if self.dragging {
            CursorStyle::ClosedHand
        } else {
            CursorStyle::OpenHand
        };

        div()
            // `on_prepaint` lives on the plain `Div`, before the element
            // becomes `Stateful`; the id has to come after it.
            .on_prepaint(move |bounds: Bounds<Pixels>, _, cx| {
                let size = bounds.size;
                entity.update(cx, |this, cx| this.measure(size, cx));
            })
            .id("model-canvas")
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .cursor(cursor)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.dragging = true;
                    this.drag_from = event.position;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if !this.dragging {
                    return;
                }
                let step = this.turn_per_pixel();
                let dx = (event.position.x - this.drag_from.x).as_f32();
                let dy = (event.position.y - this.drag_from.y).as_f32();
                this.drag_from = event.position;
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                // Dragging right turns the model right, and dragging down tips
                // its top towards the viewer — grab-and-turn, not a slider.
                this.camera.orbit(dx * step, dy * step);
                this.dirty = true;
                // Don't render during the drag: a large mesh takes hundreds of
                // milliseconds per frame, so chasing every mouse move would
                // leave the picture lapsing well behind the cursor. The
                // render happens on mouse up, when the final pose is known.
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            // Released outside the canvas: the drag still has to end.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let lines = match event.delta {
                    ScrollDelta::Lines(delta) => delta.y,
                    ScrollDelta::Pixels(delta) => delta.y.as_f32() / 40.0,
                };
                if lines == 0.0 {
                    return;
                }
                // Exponential, so every notch scales the distance equally.
                this.camera.zoom_by((-lines * 0.12).exp());
                this.dirty = true;
                this.pump(cx);
                cx.notify();
            }))
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.reset_camera(cx);
                }
            }))
            .child(match image {
                // Rendered at the device resolution and painted over the whole
                // canvas: `Fill` and `Contain` agree here, because the aspect
                // ratio the frame was rendered with is the canvas's own.
                Some(frame) => img(ImageSource::Render(frame))
                    .absolute()
                    .inset_0()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element(),
                None => self.placeholder(cx),
            })
    }

    /// What the canvas shows before the first frame arrives.
    fn placeholder(&self, cx: &App) -> AnyElement {
        let message = match &self.backend {
            Backend::Loading => rust_i18n::t!("viewport.loading").to_string(),
            _ => match &self.error {
                Some(reason) => {
                    rust_i18n::t!("viewport.render_failed", reason = reason).to_string()
                }
                None => rust_i18n::t!("viewport.rendering").to_string(),
            },
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(message)
            .into_any_element()
    }

    /// The toolbar: what the model is, how it is being drawn, and the way out.
    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (primitives, vertices) = self.stats();
        let count_label = if self.mesh.is_point_cloud() {
            rust_i18n::t!("viewport.points", count = primitives)
        } else {
            rust_i18n::t!("viewport.triangles", count = primitives)
        };
        let backend = match &self.backend {
            Backend::Loading => rust_i18n::t!("viewport.backend_loading").to_string(),
            Backend::Starting => rust_i18n::t!("viewport.backend_starting").to_string(),
            Backend::Gpu(adapter) => {
                rust_i18n::t!("viewport.backend_gpu", adapter = adapter).to_string()
            }
            Backend::Cpu(reason) => {
                rust_i18n::t!("viewport.backend_cpu", reason = reason).to_string()
            }
        };
        let frame_ms = self.frame_ms;

        h_flex()
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
                    .child(self.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} · {}",
                        count_label,
                        rust_i18n::t!("viewport.vertices", count = vertices)
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .when(frame_ms > 0.0, |element| {
                        // Two decimals: at 60 fps the number is small and the
                        // difference between 1.2 and 1.9 ms is what the user
                        // is watching.
                        element.child(format!("{frame_ms:.2} ms"))
                    }),
            )
            .when(self.samples() > 1, |element| {
                element.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{}×", self.samples())),
                )
            })
            .child(
                div()
                    .flex_none()
                    .max_w(px(220.0))
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(backend),
            )
            .child(
                Button::new("model-reset")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RotateCw)
                    .tooltip(rust_i18n::t!("viewport.reset").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.reset_camera(cx))),
            )
            .child(
                Button::new("model-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(rust_i18n::t!("viewport.close").to_string())
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Free the frame before the panel drops us.
                        this.release(window);
                        this.close(cx);
                    })),
            )
    }

    /// Hand this viewport's frames back to the window.
    ///
    /// gpui's sprite atlas never evicts on its own, so simply dropping the
    /// entity would leave the last frame — a full-viewport image, several
    /// megabytes of video memory — resident for the life of the window. Every
    /// caller that can reach a `Window` must release before letting go.
    pub fn release(&mut self, window: &mut Window) {
        if let Some(frame) = self.pending.take() {
            let _ = window.drop_image(frame);
        }
        if let Some(frame) = self.shown.take() {
            let _ = window.drop_image(frame);
        }
    }

    /// Leave the viewport; the workspace panel puts the grid back.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ModelViewportEvent::Closed);
    }
}

impl Render for ModelViewport {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Swap in the new frame and hand the old one back so its atlas entry
        // is freed — gpui's sprite atlas never evicts on its own.
        if let Some(frame) = self.pending.take() {
            if let Some(old) = self.shown.take() {
                let _ = window.drop_image(old);
            }
            self.shown = Some(frame);
        }
        // Read here rather than in the background task: the scale factor is a
        // property of the window, and prepaint (which measures the canvas)
        // runs after this.
        self.scale = window.scale_factor();

        v_flex()
            .size_full()
            .overflow_hidden()
            .child(self.toolbar(cx))
            .child(self.canvas(cx))
    }
}

/// The user-facing reason the CPU is drawing this viewport, in their language.
///
/// `gpu3d` reports the reason as a variant rather than a sentence precisely so
/// this translation does not have to live inside the renderer.
fn reason_text(unavailable: &GpuUnavailable) -> String {
    match unavailable {
        GpuUnavailable::NoAdapter => rust_i18n::t!("viewport.gpu_no_adapter").to_string(),
        GpuUnavailable::Software(name) => {
            rust_i18n::t!("viewport.gpu_software", name = name).to_string()
        }
        GpuUnavailable::NoDevice(detail) => {
            rust_i18n::t!("viewport.gpu_no_device", detail = detail).to_string()
        }
    }
}

/// Draw one frame with whichever renderer can. Runs off the UI thread.
///
/// The GPU is tried first and the CPU is the safety net, so a device that
/// comes up but cannot finish a frame still leaves the user with a picture.
fn draw(
    gpu: Option<(&GpuRenderer, &GpuMesh)>,
    mesh: &Mesh,
    camera: &Camera,
    bounds: MeshBounds,
    size: (u32, u32),
    quality: f32,
) -> Rendered {
    let aspect = size.0 as f32 / size.1.max(1) as f32;

    if let Some((renderer, uploaded)) = gpu {
        // The framing is the same one the CPU would compute: one shared
        // function, so the two renderers cannot disagree about what "framed"
        // means.
        let framing = camera.framing(bounds, aspect);
        if let Some(bytes) = renderer.render(uploaded, &framing, size)
            && let Some(frame) = frame_image(size, bytes)
        {
            return Rendered::Frame(frame);
        }
        // Either the device answered for the upload but not for this frame, or
        // it handed back something that is not a frame at all. Both mean the
        // same thing to the caller: draw this one on the CPU and demote the
        // viewport, so a broken GPU costs a single frame instead of every one.
        let reason = rust_i18n::t!("viewport.reason_gpu_lost").to_string();
        return match cpu_frame(mesh, camera, size, quality) {
            Some(frame) => Rendered::Demoted(frame, reason),
            None => Rendered::Failed(reason),
        };
    }

    match cpu_frame(mesh, camera, size, quality) {
        Some(frame) => Rendered::Frame(frame),
        None => Rendered::Failed(rust_i18n::t!("viewport.reason_no_frame").to_string()),
    }
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
    // once per asset at import time, off the interactive path — uses 2.
    let rendered = render3d::render(mesh, camera, width, height, 1, quality);
    frame_image((rendered.width, rendered.height), rendered.bgra)
}

/// Wrap a rendered BGRA frame into the image type gpui draws.
fn frame_image(size: (u32, u32), bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(size.0, size.1, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}
