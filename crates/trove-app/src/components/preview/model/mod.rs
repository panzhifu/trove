//! The main panel's 3D viewport.
//!
//! Previewing a model hands the whole main content area over to this entity:
//! the grid and every other asset disappear and the model is drawn on its own,
//! large, where it can actually be examined. The viewport closes back to the
//! grid on Escape or the toolbar's back button.
//!
//! The work is split by responsibility, because one file carrying all of it
//! stopped being readable:
//!
//! * this module — the entity itself: its state, its public seam (`spawn`,
//!   `release`, `close`, [`Render`]) and the enums its host matches on;
//! * [`load`] — getting geometry onto the screen: the parse, the streaming and
//!   indexed point-cloud readers, LOD selection and the GPU upload;
//! * [`frame`] — the frame loop and camera input: when a frame is drawn, at
//!   what quality, and which gesture the pointer or keyboard is making;
//! * [`ui`] — the element tree: canvas, toolbar, placeholder and the shortcut
//!   hint.
//!
//! Rendering itself is off-screen wgpu (see [`super::gpu3d`]), with the CPU
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

use gpui_kit::base::v_flex;
use gpui_kit::*;

use trove_core::media::formats::streaming_point_cloud::StreamingPointCloud;
use trove_core::media::formats::types::{Bounds as MeshBounds, Mesh, Winding};
use trove_core::media::index::IndexedCloud;
use trove_core::media::render3d::{self, Camera};

use super::gpu3d::{GpuMesh, GpuRenderer, GpuUnavailable};

mod frame;
mod load;
mod ui;

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
    /// Streaming a large point cloud — loading incrementally.
    Streaming,
    /// Reading a large point cloud from its index — loading incrementally.
    Indexed,
}

/// What the viewport tells its host, the workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelViewportEvent {
    /// The user left the viewport (Escape, or the back button).
    Closed,
}

/// What a held mouse button does to the camera.
///
/// Decided when the button goes down and kept for the whole gesture: a user
/// who presses shift halfway through a turn is adjusting their grip, not
/// asking for the gesture to change under their hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    /// Turn the model around its pivot.
    Orbit,
    /// Slide the model across the viewport.
    Pan,
}

/// How many times a streaming load rebuilds the renderable mesh.
///
/// The mesh is a fresh spatial query over every resident point, and it is the
/// most expensive part of a stream step: rebuilding it once per fifty-thousand
/// point chunk makes a forty-million-point file spend most of a minute drawing
/// a model it could show in seconds. Refining this many times looks continuous
/// and costs a fraction of it.
const STREAM_REFINEMENTS: usize = 32;

/// How long after the last wheel notch or key press the camera still counts as
/// in motion.
///
/// A drag says when it ends by releasing the button; a wheel or a key has no
/// such event, so it needs a window instead. Long enough to cover the gap
/// between notches of a slow spin, short enough that the sharp frame lands as
/// soon as the user stops.
const GESTURE_LINGER: std::time::Duration = std::time::Duration::from_millis(120);

/// How often the point-enhancement setting is re-read from the config file.
///
/// It lives behind a separate settings window, so an open viewport has to poll
/// for a change; polling every frame is a file read on the frame path. A
/// person clicking a switch will not notice half a second.
const ENHANCE_REFRESH: std::time::Duration = std::time::Duration::from_millis(500);

/// The 3D model viewport: a camera, a mesh, and whichever renderer came up.
pub struct ModelViewport {
    /// Display name, as the grid shows it.
    name: String,
    /// Backend task manager (shared from the library): runs the mesh parse
    /// as a registered, cancellable job.
    tasks: trove_core::tasks::TaskManager,
    /// The in-flight mesh-parse task, cancelled if the viewport closes first.
    load_task: Option<trove_core::tasks::TaskId>,
    mesh: Arc<Mesh>,
    camera: Camera,

    /// Progressive point-cloud stream. When `Some`, the viewport is loading
    /// a large point cloud incrementally and renders intermediate results.
    /// Taken for the duration of a background step, so it can be `None`
    /// briefly even while `is_streaming` is set.
    streaming: Option<StreamingPointCloud>,
    /// Whether the current file is a streaming point cloud.
    is_streaming: bool,
    /// A streaming step is running on a background thread.
    stream_step: bool,
    /// Points read from the file, the total it holds, and how many are
    /// resident, for the status line. Kept here rather than read from the
    /// streamer, which is off-thread while a step runs.
    stream_read: usize,
    stream_total: usize,
    stream_kept: usize,
    /// Points read when the displayed mesh was last rebuilt, so a step that
    /// has not moved the picture on can skip the rebuild.
    stream_rendered_read: usize,

    /// Point cloud read from an index sidecar beside the file, when one is
    /// there. Like `streaming`, it is taken for the duration of a background
    /// step, so it can be `None` briefly while `is_indexed` is set.
    indexed: Option<IndexedCloud>,
    /// Whether the current file is being read from its index.
    is_indexed: bool,
    /// An index step is running on a background thread.
    index_step: bool,
    /// Chunks read and total, for the status line.
    index_chunks_read: usize,
    index_chunks_total: usize,

    /// QEM-simplified LOD for triangle meshes. None for point clouds or small meshes.
    simplified: Option<trove_core::media::formats::simplify::SimplifiedMesh>,
    /// Current LOD level index (0 = finest).
    current_lod: usize,
    /// Bumped every time the geometry on screen is replaced. Background work
    /// captures it and discards its result when it no longer matches, so a
    /// slow parse or a slow GPU upload cannot install itself over a newer
    /// mesh.
    mesh_serial: u64,

    /// Where keyboard events for the viewport go. Without a focus handle the
    /// canvas never enters the focus path, and its `on_key_down` — the
    /// shortcuts the on-screen hint advertises — never fires.
    focus_handle: FocusHandle,
    /// Whether focus has been claimed; done at the first paint, because the
    /// viewport is built without a window to focus it in.
    focused: bool,

    /// The GPU device and the mesh's buffers in it. Both are `None` while the
    /// device is starting and when it could not be created at all — the CPU
    /// path covers both.
    gpu: Option<Arc<GpuRenderer>>,
    gpu_mesh: Option<Arc<GpuMesh>>,
    /// The device came up but could not finish a frame, so the viewport will
    /// not go back to it.
    gpu_demoted: bool,
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
    /// A frame is waiting for the rate limit to expire; exactly one retry is
    /// ever outstanding, however many requests arrive meanwhile.
    retry_pending: bool,

    /// Which way the displayed mesh's triangles wind, computed once when the
    /// geometry is swapped in. It is what says whether the back faces are
    /// hidden by the front ones — and so whether they may be skipped.
    mesh_winding: Winding,
    /// The bounds the camera frames the model against.
    ///
    /// Deliberately *not* the displayed mesh's own bounds: a streaming cloud's
    /// renderable mesh is rebuilt every frame from the points nearest the
    /// camera, so its bounding box moves as the user turns the model and as
    /// chunks arrive — framing on it rescales the model mid-rotation. This is
    /// the model's real extent instead, which never shrinks.
    scene_bounds: MeshBounds,
    /// Buffers the CPU rasteriser reuses across frames. Shared with the
    /// render task so a drag stops paying for 50 MB of allocation per frame.
    scratch: Arc<std::sync::Mutex<render3d::Scratch>>,
    /// What the held mouse button is doing, while it is held.
    drag: Option<Drag>,
    /// Last drag position, in window coordinates.
    drag_from: Point<Pixels>,
    /// When the last camera move arrived that has no pointer state of its own
    /// — a wheel notch or a key press. See [`ModelViewport::is_interacting`].
    last_camera_move: Option<Instant>,
    /// The linger timer is armed; it ends the gesture window and draws the
    /// settled frame.
    gesture_armed: bool,
    /// Cached `AppConfig::point_enhance`, so the frame path does not read the
    /// config file every time it draws.
    enhance_points: bool,
    /// Whether the model is painted by height, cached from the same config
    /// read as `enhance_points`.
    height_color: bool,
    /// Whether the scene's X/Y/Z axes are drawn, cached from the same config
    /// read. Independent of `height_color` — see [`ModelViewport::axis_toggle`].
    show_scene_axes: bool,
    /// Whether the corner trihedron is drawn, cached from the same config
    /// read.
    show_corner_axis: bool,
    /// When `enhance_points` was last re-read.
    enhance_checked: Option<Instant>,
}

impl EventEmitter<ModelViewportEvent> for ModelViewport {}

impl ModelViewport {
    /// Open the viewport for `path`, showing a loading indicator until the
    /// mesh parses. The parse runs on a background thread so the UI never
    /// blocks — even a 20 GB export turns the spinner instead of freezing
    /// the window.
    pub fn spawn(
        name: String,
        path: PathBuf,
        tasks: trove_core::tasks::TaskManager,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            // Placeholder mesh: one invisible vertex so the renderer has
            // something valid to hold until the real parse lands. The true
            // mesh arrives via `set_mesh` from the background task below.
            let loading_mesh = Arc::new(Mesh::default());
            // One config read for every setting the viewport starts with.
            let cfg = trove_core::config::AppConfig::load();
            let mut this = Self {
                name,
                tasks,
                load_task: None,
                mesh: loading_mesh,
                camera: Camera::default(),
                streaming: None,
                is_streaming: false,
                stream_step: false,
                stream_read: 0,
                stream_total: 0,
                stream_kept: 0,
                stream_rendered_read: 0,
                indexed: None,
                is_indexed: false,
                index_step: false,
                index_chunks_read: 0,
                index_chunks_total: 0,
                simplified: None,
                current_lod: 0,
                mesh_serial: 0,
                focus_handle: cx.focus_handle(),
                focused: false,
                gpu: None,
                gpu_mesh: None,
                gpu_demoted: false,
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
                retry_pending: false,
                mesh_winding: Winding::default(),
                scene_bounds: MeshBounds::default(),
                scratch: Arc::new(std::sync::Mutex::new(render3d::Scratch::default())),
                drag: None,
                drag_from: Point::default(),
                last_camera_move: None,
                gesture_armed: false,
                enhance_points: true,
                height_color: cfg.height_color(),
                show_scene_axes: cfg.scene_axes(),
                show_corner_axis: cfg.corner_axis(),
                enhance_checked: None,
            };
            this.start_load(path, cx);
            this
        })
    }

    /// The model's display name, for the host panel's title bar.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Primitives and vertices of the loaded geometry, for the status line.
    /// Primitives are triangles for a mesh, points for a cloud.
    pub fn stats(&self) -> (usize, usize) {
        (self.mesh.primitive_count(), self.mesh.vertex_count())
    }

    /// What is drawing the model, for the status bar: the adapter for a GPU
    /// frame, the reason for a CPU one, or the progress of a load that is
    /// still running.
    ///
    /// It lives here rather than in the status bar so the wording stays beside
    /// the state it describes, and so the main panel's title bar can stop
    /// carrying it — which is what freed that row for the preview's own tools.
    pub(crate) fn backend_text(&self) -> String {
        match &self.backend {
            Backend::Loading => rust_i18n::t!("viewport.backend_loading").to_string(),
            Backend::Starting => rust_i18n::t!("viewport.backend_starting").to_string(),
            Backend::Gpu(adapter) => {
                rust_i18n::t!("viewport.backend_gpu", adapter = adapter).to_string()
            }
            Backend::Cpu(reason) => {
                rust_i18n::t!("viewport.backend_cpu", reason = reason).to_string()
            }
            Backend::Streaming => {
                // Progress comes from the fields the background step updates:
                // the streamer itself is off-thread while a step is running.
                let loaded = self.stream_read;
                let total = self.stream_total;
                if total > 0 {
                    let pct = (loaded as f32 / total as f32 * 100.0) as u32;
                    // Progress is counted in points *read*: once the resident
                    // budget starts thinning the cloud, the kept count stops
                    // tracking the file.
                    rust_i18n::t!(
                        "viewport.backend_streaming",
                        percent = pct,
                        loaded = loaded,
                        total = total,
                        kept = self.stream_kept
                    )
                    .to_string()
                } else {
                    rust_i18n::t!("viewport.backend_streaming_starting").to_string()
                }
            }
            Backend::Indexed => rust_i18n::t!(
                "viewport.backend_indexed",
                chunks = self.index_chunks_read,
                total = self.index_chunks_total
            )
            .to_string(),
        }
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

    /// Leave the viewport; the workspace panel puts the grid back. A mesh
    /// parse still in flight is cancelled (cooperatively — it will not
    /// start further work, though an indivisible parse finishes).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.load_task.take() {
            self.tasks.cancel(id);
        }
        cx.emit(ModelViewportEvent::Closed);
    }
}

impl Render for ModelViewport {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The viewport is built without a window, so this is the first place
        // focus can be claimed. Doing it once is enough: the canvas is the
        // only focusable thing here, and clicking it re-claims focus.
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus_handle, cx);
        }
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

        // The toolbar lives in the host panel's title bar (see
        // `WorkspacePanel::title_suffix`), so the canvas gets the whole area.
        v_flex()
            .size_full()
            .overflow_hidden()
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

/// The gesture a button press starts, if any.
///
/// Left turns the model, middle slides it, and left with shift slides it too —
/// the near-universal alternative on a laptop or a two-button mouse. The right
/// button is deliberately left alone: it belongs to the context menu.
fn drag_for(button: MouseButton, modifiers: Modifiers) -> Option<Drag> {
    match button {
        MouseButton::Left if modifiers.shift => Some(Drag::Pan),
        MouseButton::Left => Some(Drag::Orbit),
        MouseButton::Middle => Some(Drag::Pan),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags in a `test`
    // attribute macro from the gpui prelude, which makes expanding `#[test]`
    // below recurse.
    use super::{Drag, drag_for};
    use gpui_kit::{Modifiers, MouseButton, NavigationDirection};

    #[test]
    fn buttons_map_to_the_documented_gestures() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(drag_for(MouseButton::Left, plain), Some(Drag::Orbit));
        assert_eq!(drag_for(MouseButton::Left, shift), Some(Drag::Pan));
        assert_eq!(drag_for(MouseButton::Middle, plain), Some(Drag::Pan));
        assert_eq!(drag_for(MouseButton::Middle, shift), Some(Drag::Pan));
        // The right button is the context menu's, and a navigation button is
        // not ours either.
        assert_eq!(drag_for(MouseButton::Right, plain), None);
        assert_eq!(
            drag_for(MouseButton::Navigate(NavigationDirection::Back), plain),
            None
        );
    }
}
