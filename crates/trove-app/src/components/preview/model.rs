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
use trove_core::media::formats::simplify::{select_lod, simplify_mesh};
use trove_core::media::formats::streaming_point_cloud::StreamingPointCloud;
use trove_core::media::formats::types::{Bounds as MeshBounds, Mesh, Winding};
use trove_core::media::index::{IndexedCloud, index_path_for};
use trove_core::media::render3d::{self, Camera, RenderOptions};

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

/// How many times a streaming load rebuilds the renderable mesh.
///
/// The mesh is a fresh spatial query over every resident point, and it is the
/// most expensive part of a stream step: rebuilding it once per fifty-thousand
/// point chunk makes a forty-million-point file spend most of a minute drawing
/// a model it could show in seconds. Refining this many times looks continuous
/// and costs a fraction of it.
const STREAM_REFINEMENTS: usize = 32;

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
            };
            this.start_load(path, cx);
            this
        })
    }

    /// Parse the mesh through the backend task manager (registered, visible
    /// in the task list, cancelled when the viewport closes), then hand it
    /// back to the viewport.
    fn start_load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Detect if this is a large point cloud that should use streaming.
        const STREAMING_THRESHOLD: u64 = 64 << 20; // 64 MiB
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();

        if ext == "ply" && size > STREAMING_THRESHOLD {
            // An index beside the file turns "read twenty gigabytes" into
            // "read the chunks on screen". One is used only when it already
            // exists: building is a separate, offline step (the `index_build`
            // example), so opening a model never pays for an index nobody
            // asked for. Large files are imported as links by default, so the
            // path here really is the one the user indexed next to.
            let index_path = index_path_for(&path);
            if index_path.exists()
                && let Ok(cloud) = IndexedCloud::open(&index_path)
            {
                self.is_indexed = true;
                self.index_chunks_total = cloud.chunk_count();
                // Frame the whole cloud from the header, not the part that has
                // arrived: the model keeps its size on screen while it fills.
                self.scene_bounds = cloud.bounds();
                self.indexed = Some(cloud);
                self.backend = Backend::Indexed;
                self.dirty = true;
                // Deliberately started here rather than left to `pump`: the
                // first step is what produces the first frame, and a viewport
                // that waits for a paint to start loading shows nothing.
                self.begin_index_step(cx);
                cx.notify();
                return;
            }
            // Try to open as a streaming point cloud. `open` refuses a mesh
            // (a PLY with faces) and any layout this reader cannot walk, and
            // the whole-file loader below covers those.
            match StreamingPointCloud::open(&path) {
                Ok(streamer) => {
                    self.is_streaming = true;
                    self.streaming = Some(streamer);
                    // Deliberately *not* `Backend::Loading`: `pump` returns
                    // early on that state, and the streaming branch inside it
                    // is what loads the first chunk. A viewport that stays
                    // Loading here never streams anything.
                    self.backend = Backend::Streaming;
                    self.dirty = true;
                    cx.notify();
                    return;
                }
                Err(_) => {
                    // Fall back to regular loading if streaming init fails.
                }
            }
        }

        let started = self.tasks.start(
            trove_core::tasks::TaskKind::ModelPreview,
            format!("parse {}", path.display()),
            move |ctx| {
                let _ = ctx; // parsing is one indivisible unit; no checkpoints
                Self::load_mesh(&path)
            },
        );
        let (id, rx) = match started {
            Ok(pair) => pair,
            Err(_) => return, // a parse is somehow already running; keep the placeholder
        };
        self.load_task = Some(id);
        cx.spawn(async move |weak, cx| {
            // The channel is blocking; park the recv on a pool thread.
            let result = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            weak.update(cx, |this, cx| match result {
                Some(mesh) => this.set_mesh(mesh, cx),
                // Failed or cancelled — the task event carries the details.
                None => {
                    this.error = Some("load failed".into());
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
        // Don't simplify synchronously — QEM is O(n log n) and would block
        // the UI thread for large meshes. Instead, show the mesh immediately
        // and compute LOD levels in the background.
        self.simplified = None;
        self.current_lod = 0;
        // The camera frames this, and it stays put for the life of the
        // viewport — LOD levels and streamed chunks change the geometry, not
        // the model's extent.
        self.scene_bounds = mesh.bounds;
        self.swap_mesh(mesh);

        // Spawn QEM simplification in the background for large meshes.
        if self.mesh.triangle_count() > 1000 {
            let mesh_for_lod = self.mesh.clone();
            let serial = self.mesh_serial;
            cx.spawn(async move |weak, cx| {
                let simplified = cx
                    .background_executor()
                    .spawn(async move { simplify_mesh(&mesh_for_lod) })
                    .await;
                weak.update(cx, |this, cx| {
                    // A newer mesh has landed since this was queued: its levels
                    // describe geometry that is no longer on screen.
                    if this.mesh_serial != serial {
                        return;
                    }
                    this.simplified = Some(simplified);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        self.backend = Backend::Starting;
        self.start_gpu(cx);
        cx.notify();
    }

    /// Replace the displayed geometry, recording that it is newer than
    /// anything an in-flight task might still be holding.
    fn swap_mesh(&mut self, mesh: Mesh) {
        // Derived geometry passes the winding it inherits: an LOD level comes
        // off the same surface as the mesh it was simplified from, and
        // re-classifying a million-triangle level (a hash map over its edges)
        // on the UI thread is exactly the stall this avoids. A cloud has no
        // triangles to classify, so two-sided is free and correct.
        let winding = if mesh.is_point_cloud() {
            Winding::TwoSided
        } else {
            mesh.winding()
        };
        self.swap_mesh_with(mesh, winding);
    }

    /// [`ModelViewport::swap_mesh`], with the winding already decided.
    fn swap_mesh_with(&mut self, mesh: Mesh, winding: Winding) {
        self.mesh_winding = winding;
        self.mesh_serial += 1;
        self.mesh = Arc::new(mesh);
        self.dirty = true;
    }

    /// Swap in the LOD level the camera calls for, if it is not the one
    /// already drawn.
    fn update_lod(&mut self, cx: &mut Context<Self>) {
        let Some(simplified) = &self.simplified else {
            return;
        };
        if simplified.levels.len() <= 1 {
            return;
        }
        // The selection is measured against the *original* bounds, not the
        // level's: a level whose bounds shift would otherwise change the
        // distance, which would change the level — a feedback loop that
        // flickers between two levels as the camera sits still.
        let bounds = simplified.bounds;
        let aspect = self.logical.0.max(1.0) / self.logical.1.max(1.0);
        let framing = self.camera.framing(bounds, aspect);
        let radius = 1.0 / framing.inv_radius;
        let cam_dist = (framing.distance * radius) as f64;

        let Some(lod) = select_lod(
            simplified,
            cam_dist,
            self.logical.1.max(1.0),
            render3d::FOV_DEG,
        ) else {
            return;
        };
        if lod == self.current_lod || lod >= simplified.levels.len() {
            return;
        }
        self.current_lod = lod;
        let level = &simplified.levels[lod];
        let has_normals = simplified.has_normals;
        // The level carries its own triangles: a level swapped in without
        // them would be drawn as a point cloud by both renderers.
        let normals = if has_normals {
            level.vertices.iter().map(|v| v.normal).collect()
        } else {
            Vec::new()
        };
        let mesh = Mesh::from_parts(
            level.vertices.iter().map(|v| v.position).collect(),
            normals,
            Vec::new(),
            level.triangles.clone(),
        );
        if let Some(mesh) = mesh {
            // The level inherits the original mesh's winding.
            self.swap_mesh_with(mesh, self.mesh_winding);
            self.backend = Backend::Starting;
            self.start_gpu(cx);
        }
    }

    /// Start one streaming step on a background thread, unless one is
    /// already running.
    ///
    /// Reading a chunk, rebalancing the octree and rebuilding the renderable
    /// mesh all cost time proportional to the cloud loaded so far. On the UI
    /// thread that is a visible freeze — on exactly the multi-gigabyte files
    /// this path exists for — so the whole step runs off-thread and only the
    /// finished mesh comes back.
    fn begin_stream_step(&mut self, cx: &mut Context<Self>) {
        if self.stream_step {
            return; // The running step will come back with the next mesh.
        }
        let Some(streamer) = self.streaming.take() else {
            return;
        };
        let cam_pos = self.camera_eye();
        let last_rendered = self.stream_rendered_read;
        self.stream_step = true;
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let mut streamer = streamer;
                    let step = streamer.step();
                    // Rebuilding the mesh walks every resident point. Only when
                    // the picture has really advanced — or the stream is done —
                    // is that worth its cost; the steps in between are still
                    // read into the octree, so no data is skipped, and the
                    // status line still moves every step.
                    let due = step.complete
                        || last_rendered == 0
                        || step.points_read.saturating_sub(last_rendered)
                            >= (step.total_points / STREAM_REFINEMENTS).max(1);
                    let mesh = due.then(|| streamer.render_mesh_all(cam_pos));
                    (streamer, step, mesh)
                })
                .await;
            weak.update(cx, |this, cx| {
                this.stream_step = false;
                let (streamer, step, mesh) = outcome;
                this.stream_read = step.points_read;
                this.stream_total = step.total_points;
                this.stream_kept = step.points_loaded;
                // The cloud's own extent, which only grows: a stable frame for
                // a camera that is being turned while the file streams.
                if let Some(bounds) = streamer.framing_bounds() {
                    this.scene_bounds = bounds;
                }
                this.streaming = Some(streamer);
                if let Some(mesh) = mesh {
                    this.stream_rendered_read = step.points_read;
                    if mesh.vertex_count() > 0 {
                        this.swap_mesh(mesh);
                        this.backend = Backend::Streaming;
                    }
                }
                if step.complete {
                    // Streaming done: the cloud is complete, so the mesh is
                    // final and can go to the GPU like any other model.
                    this.is_streaming = false;
                    this.streaming = None;
                    this.backend = Backend::Starting;
                    this.start_gpu(cx);
                }
                // `pump` starts the next step and draws what just arrived.
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Start one index-reading step on a background thread, unless one is
    /// already running.
    ///
    /// The same shape as [`ModelViewport::begin_stream_step`], and for the
    /// same reason: decoding chunks and rebuilding the renderable mesh cost
    /// time proportional to the cloud loaded so far, so the work runs off the
    /// UI thread and only the finished mesh comes back.
    fn begin_index_step(&mut self, cx: &mut Context<Self>) {
        if self.index_step {
            return; // The running step will come back with the next mesh.
        }
        let Some(cloud) = self.indexed.take() else {
            return;
        };
        let cam_pos = self.camera_eye();
        // Chunks behind the camera should not spend the resident budget, so
        // the selection culls against the real frustum (which is also the
        // frame the user is looking at).
        let bounds = self.framing_bounds();
        let aspect = self.logical.0.max(1.0) / self.logical.1.max(1.0);
        let frustum = self.camera.framing(bounds, aspect).frustum();
        self.index_step = true;
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let mut cloud = cloud;
                    let step = cloud.step(&frustum, cam_pos);
                    let mesh = cloud.render_mesh(cam_pos);
                    (cloud, step, mesh)
                })
                .await;
            weak.update(cx, |this, cx| {
                this.index_step = false;
                let (cloud, step, mesh) = outcome;
                this.index_chunks_read = step.chunks_read;
                this.index_chunks_total = step.chunks_total;
                this.indexed = Some(cloud);
                if mesh.vertex_count() > 0 {
                    this.swap_mesh(mesh);
                    this.backend = Backend::Indexed;
                }
                if step.complete {
                    // Nothing more will be read: the resident set is what the
                    // viewport draws from here on, so it goes to the GPU like
                    // any other model and the index — with its millions of
                    // resident points — is dropped.
                    this.is_indexed = false;
                    this.indexed = None;
                    this.backend = Backend::Starting;
                    this.start_gpu(cx);
                }
                // `pump` starts the next step and draws what just arrived.
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The camera's eye in model space, as the renderers will place it.
    /// Sharing the camera's own framing keeps LOD selection and the picture
    /// from disagreeing about how far away the model is.
    fn camera_eye(&self) -> [f32; 3] {
        let bounds = self.framing_bounds();
        let aspect = self.logical.0.max(1.0) / self.logical.1.max(1.0);
        self.camera.framing(bounds, aspect).eye_in_model_space()
    }

    /// The bounds the camera frames: the model's full extent, falling back to
    /// the displayed mesh before the first mesh or sample has arrived.
    fn framing_bounds(&self) -> MeshBounds {
        if self.scene_bounds.is_empty() {
            self.mesh.bounds
        } else {
            self.scene_bounds
        }
    }

    /// Load a mesh, dispatching on the file size. Files over 512 MiB go
    /// through the chunked LOD loader with a 128 MiB parsed-geometry budget;
    /// smaller files take the stock loader, which keeps every vertex.
    fn load_mesh(path: &PathBuf) -> Result<Mesh, String> {
        const CHUNKED_THRESHOLD: u64 = 512 << 20;
        let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        // Only a PLY has a chunked reader. Routing every large file here used
        // to send a big OBJ or STL into a loader that only understands PLY,
        // which failed with a parse error about a format the file never was —
        // the whole-file loader has its own ceiling and its own message.
        let is_ply = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ply"));
        if is_ply && size > CHUNKED_THRESHOLD {
            let config = LodConfig {
                memory_budget: 128 << 20,
                max_lod_step: 32,
            };
            chunked::load_ply_chunked(path, config)
        } else {
            trove_core::media::formats::load(path)
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
    ///
    /// A device that is already up is reused: only the geometry is re-uploaded.
    /// Building a second device and a second set of pipelines for every LOD
    /// switch costs far more than the upload it accompanies. The mesh serial
    /// is captured too, so a slow upload that finishes after the geometry has
    /// been replaced again is dropped instead of installing stale buffers.
    fn start_gpu(&mut self, cx: &mut Context<Self>) {
        // A device that already failed to finish a frame is not retried: the
        // CPU path is what the viewport settled on.
        if self.gpu_demoted {
            return;
        }
        let mesh = self.mesh.clone();
        let serial = self.mesh_serial;
        let existing = self.gpu.clone();
        cx.spawn(async move |weak, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let renderer = match existing {
                        Some(renderer) => renderer,
                        None => Arc::new(GpuRenderer::new()?),
                    };
                    let uploaded = renderer.upload_capped(&mesh);
                    Ok::<_, GpuUnavailable>((renderer, uploaded))
                })
                .await;
            weak.update(cx, |this, cx| {
                // Newer geometry is on screen by now; these buffers are for a
                // mesh nobody is looking at.
                if this.mesh_serial != serial {
                    return;
                }
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
        // While streaming, keep one step in flight. Each finished step swaps
        // in a new mesh (which marks the viewport dirty) and comes back here
        // to start the next one, so the loop is driven by the work itself
        // rather than by the frame rate.
        if self.is_streaming {
            self.begin_stream_step(cx);
        } else if self.is_indexed {
            self.begin_index_step(cx);
        } else if !self.is_dragging() {
            // Update LOD based on camera distance for triangle meshes — but
            // not mid-gesture: a level change re-uploads the whole mesh, and a
            // drag is the worst possible moment to spend that. The drag end
            // pumps again and picks the level up then.
            self.update_lod(cx);
        }
        // Whether this request is drawn, deferred to the frame in flight, or
        // held back by the rate limit — the last of which has to be retried,
        // because nothing else comes back for it.
        match frame_action(
            self.dirty,
            self.in_flight,
            self.is_dragging(),
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
        let gpu = self.gpu.clone();
        let gpu_mesh = self.gpu_mesh.clone();
        let mesh = self.mesh.clone();
        let camera = self.camera;
        let bounds = self.framing_bounds();
        // A gesture gets a draft: a quarter of the geometry (so a heavy mesh
        // turns fluently on the CPU path), half the resolution and no MSAA.
        // The gesture's end sets `dirty` again and draws the settled version.
        let interactive = self.is_dragging();
        let quality = if interactive { 0.25 } else { 1.0 };
        let scratch = self.scratch.clone();
        // Re-read rather than cache: the settings window is a separate OS
        // window, so a toggle there can only reach an already-open viewport
        // this way. `AppConfig::load` is a small JSON read.
        let enhance = trove_core::config::AppConfig::load().point_enhance();
        let options = RenderOptions {
            // Skipping back faces is free for a closed mesh and wrong for
            // anything else, so it follows the winding exactly.
            cull_backfaces: self.mesh_winding != Winding::TwoSided,
            // Eye-dome lighting and gap filling are a full-screen pass each;
            // they are worth it on a settled frame and wasted on a draft the
            // user is dragging past.
            enhance_points: !interactive && enhance,
        };

        self.dirty = false;
        self.in_flight = true;
        cx.spawn(async move |weak, cx| {
            let started = Instant::now();
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
                        // `gpu_demoted` is what makes "for good" true — without
                        // it the next LOD switch would build a fresh device and
                        // hand the user a second failure.
                        this.gpu = None;
                        this.gpu_mesh = None;
                        this.gpu_demoted = true;
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

    /// Whether a gesture is in progress, i.e. whether this frame is a draft.
    fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Start a gesture: remember what the button means and where it started.
    fn begin_drag(&mut self, mode: Drag, from: Point<Pixels>, cx: &mut Context<Self>) {
        self.drag = Some(mode);
        self.drag_from = from;
        // The frame that arrives from here on is a draft, so the first move
        // does not have to wait for a settled frame to finish.
        cx.notify();
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

    /// The canvas: the model, and every interaction that moves the camera.
    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let image = self.shown.clone();
        let cursor = if self.is_dragging() {
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
            // The canvas takes focus itself: `on_key_down` only fires for an
            // element on the focus path, so without this the shortcuts below
            // are dead. `track_focus` puts it on that path; `render` claims
            // focus the first time the viewport is painted.
            .track_focus(&self.focus_handle)
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .cursor(cursor)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // Clicking the model focuses it, so the keys work without
                    // having to know that they need to be aimed at the canvas.
                    window.focus(&this.focus_handle, cx);
                    if let Some(mode) = drag_for(event.button, event.modifiers) {
                        this.begin_drag(mode, event.position, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if let Some(mode) = drag_for(event.button, event.modifiers) {
                        this.begin_drag(mode, event.position, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(mode) = this.drag else {
                    return;
                };
                let dx = (event.position.x - this.drag_from.x).as_f32();
                let dy = (event.position.y - this.drag_from.y).as_f32();
                this.drag_from = event.position;
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                match mode {
                    // Grab-and-turn: the surface under the cursor follows it.
                    // Dragging right therefore *decreases* yaw — increasing it
                    // moves the eye towards +x, which swings the model's front
                    // face the other way — and dragging down tips the top
                    // towards the viewer.
                    Drag::Orbit => {
                        let step = this.turn_per_pixel();
                        this.camera.orbit(-dx * step, dy * step);
                    }
                    // The model follows the hand: the pivot moves the opposite
                    // way to the drag, so the geometry under the cursor stays
                    // under it.
                    Drag::Pan => {
                        let step = this.camera.pan_per_pixel(this.logical.1.max(1.0));
                        this.camera.pan_by([-dx * step, dy * step]);
                    }
                }
                this.dirty = true;
                // Render now, off the UI thread: the frame follows the cursor
                // instead of waiting for the button to come up. `pump` coalesces
                // — while a frame is in flight, later moves only update the pose
                // it will pick up next, so a fast drag never queues work.
                this.pump(cx);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            // Released outside the canvas: the drag still has to end.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystep = 0.05f32;
                // A nudge of a twentieth of the model's radius, which reads the
                // same at any zoom — the same idea as `turn_per_pixel`.
                let panstep = 0.05f32;
                let zoom_factor = 1.1f32;
                // Shift turns the arrow keys from turning the model into
                // sliding it, so the keyboard can do everything the mouse can.
                let pan = event.keystroke.modifiers.shift;
                let key = event.keystroke.key.as_str();
                let nudge = |this: &mut Self, dx: f32, dy: f32| {
                    if pan {
                        // Panning pushes the *view*: the model slides the other
                        // way, which is what an arrow key does in every viewer.
                        this.camera.pan_by([dx * panstep, dy * panstep]);
                    } else {
                        // Turning follows the drag's convention instead, so an
                        // arrow turns the model the way it points.
                        this.camera.orbit(-dx * keystep, dy * keystep);
                    }
                    this.dirty = true;
                };
                match key {
                    "left" => nudge(this, -1.0, 0.0),
                    "right" => nudge(this, 1.0, 0.0),
                    "up" => nudge(this, 0.0, -1.0),
                    "down" => nudge(this, 0.0, 1.0),
                    "w" => nudge(this, 0.0, -1.0),
                    "s" => nudge(this, 0.0, 1.0),
                    "a" => nudge(this, -1.0, 0.0),
                    "d" => nudge(this, 1.0, 0.0),
                    "e" => {
                        this.camera.zoom_by(1.0 / zoom_factor);
                        this.dirty = true;
                    }
                    "q" => {
                        this.camera.zoom_by(zoom_factor);
                        this.dirty = true;
                    }
                    "=" | "+" => {
                        this.camera.zoom_by(1.0 / zoom_factor);
                        this.dirty = true;
                    }
                    "-" | "_" => {
                        this.camera.zoom_by(zoom_factor);
                        this.dirty = true;
                    }
                    "r" => this.reset_camera(cx),
                    _ => return,
                }
                // The key was aimed at the viewport: keep an arrow — or a
                // `q`/`e` bound elsewhere — from also running the app's own
                // action for it.
                cx.stop_propagation();
                this.pump(cx);
                cx.notify();
            }))
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.reset_camera(cx);
                }
            }))
            .child(match image {
                Some(frame) => frame_element(frame),
                None => self.placeholder(cx),
            })
            .child(self.shortcuts_hint(cx))
    }

    /// A small panel in the top-right corner listing the keyboard shortcuts.
    fn shortcuts_hint(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::base::v_flex;
        let line = |text: String| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        v_flex()
            .absolute()
            .top_2()
            .right_2()
            .gap_1()
            .p_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .opacity(0.8)
            .child(line(rust_i18n::t!("viewport.shortcuts").to_string()))
            .child(line(rust_i18n::t!("viewport.shortcut_rotate").to_string()))
            .child(line(
                rust_i18n::t!("viewport.shortcut_pan_drag").to_string(),
            ))
            .child(line(
                rust_i18n::t!("viewport.shortcut_pan_keys").to_string(),
            ))
            .child(line(rust_i18n::t!("viewport.shortcut_zoom").to_string()))
            .child(line(rust_i18n::t!("viewport.shortcut_reset").to_string()))
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
        if let Some(bytes) = renderer.render(
            uploaded,
            &framing,
            size,
            interactive,
            options.enhance_points,
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

/// The rendered frame, stretched over the whole canvas.
///
/// The explicit `size_full` is load-bearing. gpui's image element lays itself
/// out at the *image's own pixel size* whenever its style leaves the size
/// `Auto` — `absolute` and `inset_0` set the insets, not the size — so a frame
/// rendered at a different resolution would be drawn at that size in the
/// canvas's top-left corner instead of being stretched over it. That is
/// exactly what a half-resolution draft frame (see [`INTERACTIVE_SCALE`])
/// would do: the picture shrinks into the corner every time the camera moves
/// and snaps back when the settled frame arrives, since that one happens to be
/// canvas-sized.
fn frame_element(frame: Arc<RenderImage>) -> AnyElement {
    img(ImageSource::Render(frame))
        .absolute()
        .inset_0()
        .size_full()
        .object_fit(ObjectFit::Fill)
        .into_any_element()
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
    dragging: bool,
    since_last_frame: Option<std::time::Duration>,
) -> FrameAction {
    if in_flight {
        return FrameAction::Defer;
    }
    if !dirty {
        return FrameAction::Idle;
    }
    // A gesture draws every pose, however fast they arrive: the model has to
    // follow the cursor. Otherwise the frame rate is held, and the caller is
    // told to come back rather than losing the frame.
    if !dragging && since_last_frame.is_some_and(|since| since < FRAME_INTERVAL) {
        return FrameAction::Retry;
    }
    FrameAction::Render
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
    use super::{Drag, FRAME_INTERVAL, FrameAction, drag_for, frame_action, interactive_size};
    use gpui_kit::{Modifiers, MouseButton, NavigationDirection};

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
