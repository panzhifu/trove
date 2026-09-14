//! Getting a model's geometry onto the screen.
//!
//! Three sources, one outcome: a [`Mesh`] the renderers understand. A stock
//! parse for the small files that fit in memory, a streaming octree for a
//! point cloud too large to hold, a chunked PLY loader for a mesh that is, and
//! an index reader when one of those clouds has a sidecar beside it. On top of
//! that sits what the viewport does with the result: swap it in, pick an LOD
//! from the camera distance, and hand it to the GPU.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::*;

use trove_core::media::chunked::{self, LodConfig};
use trove_core::media::formats::simplify::{select_lod, simplify_mesh};
use trove_core::media::formats::streaming_point_cloud::StreamingPointCloud;
use trove_core::media::formats::types::{Bounds as MeshBounds, Mesh, Winding};
use trove_core::media::index::{IndexedCloud, index_path_for};
use trove_core::media::render3d;

use super::super::gpu3d::{GpuRenderer, GpuUnavailable};
use super::{Backend, ModelViewport, STREAM_REFINEMENTS, reason_text};

impl ModelViewport {
    /// Parse the mesh through the backend task manager (registered, visible
    /// in the task list, cancelled when the viewport closes), then hand it
    /// back to the viewport.
    pub(super) fn start_load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
    pub(super) fn update_lod(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn begin_stream_step(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn begin_index_step(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn framing_bounds(&self) -> MeshBounds {
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
            };
            chunked::load_ply_chunked(path, config)
        } else {
            trove_core::media::formats::load(path)
        }
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
}
