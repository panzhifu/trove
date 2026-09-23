//! Progressive point-cloud renderer.
//!
//! Wraps a [`StreamingOctree`] and feeds new points to it as they arrive
//! from a file stream. The renderer can draw at any time — intermediate
//! results are valid and show a progressively refining point cloud.

use super::streaming::point_streamer::PointStreamer;
use crate::media::formats::point_cloud::{Frustum, StreamingOctree};
use crate::media::formats::types::{Bounds, Mesh};
use crate::media::formats::virtual_memory::{DEFAULT_MEMORY_BUDGET, VirtualMemory};
use std::path::Path;

/// Default budget for visible points per frame.
pub const DEFAULT_POINT_BUDGET: usize = 300_000;

/// Maximum points to load per frame (from disk).
const POINTS_PER_FRAME: usize = 50_000;

/// Points a streamed cloud keeps resident before it starts thinning.
///
/// Roughly 4M points is 130 MB of positions and colours plus the octree's own
/// index — small enough to sit in a browser-sized budget next to the rest of
/// the app, and dense enough that a scan still reads as a surface. The point of
/// the cap is that a twenty-gigabyte cloud cannot take the process down: past
/// it, detail thins out evenly instead of the run failing.
pub const DEFAULT_RESIDENT_POINTS: usize = 4_000_000;

/// A streaming point cloud that progressively loads and renders.
pub struct StreamingPointCloud {
    streamer: PointStreamer,
    octree: StreamingOctree,
    total_vertices: usize,
    loaded: bool,
    /// Virtual memory manager for large files. None for small files.
    vmem: Option<VirtualMemory>,
    /// The whole file's extent, sampled from disk on the first step. `None`
    /// until then, and for a file whose records cannot be seeked to.
    extent: Option<Bounds>,
    /// Whether the extent has been sampled (once).
    sampled: bool,
}

/// Records read when sampling the file's extent. Enough to find a cloud's
/// true bounding box for any real scan, few enough to cost a few hundred
/// random reads rather than a pass over twenty gigabytes.
const EXTENT_SAMPLES: usize = 4096;

/// Result of a single loading step.
#[derive(Debug)]
pub struct StreamStep {
    /// Points read from the file so far — the honest measure of progress, even
    /// when the cloud is thinning itself to stay inside its budget.
    pub points_read: usize,
    /// Points resident in the octree: what is actually on screen.
    pub points_loaded: usize,
    /// Total points in the file.
    pub total_points: usize,
    /// Whether loading is complete.
    pub complete: bool,
}

impl StreamingPointCloud {
    /// Open a PLY file for streaming.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let (streamer, vertex_count) = PointStreamer::open(&path)?;
        // A surface is not a cloud: hand it back to the mesh loader, which
        // reads its faces.
        if streamer.declares_faces() {
            return Err("the PLY declares a face element, so it is a mesh".into());
        }

        let file_size = std::fs::metadata(path.as_ref())
            .map(|m| m.len())
            .unwrap_or(0);

        // Start with a unit cube bounds; will expand as points arrive.
        let mut initial_bounds = Bounds::empty();
        initial_bounds.extend([-1.0, -1.0, -1.0]);
        initial_bounds.extend([1.0, 1.0, 1.0]);

        let mut octree = StreamingOctree::new(initial_bounds);
        octree.set_budget(DEFAULT_RESIDENT_POINTS);
        Ok(Self {
            streamer,
            octree,
            total_vertices: vertex_count,
            loaded: false,
            vmem: if file_size > DEFAULT_MEMORY_BUDGET as u64 {
                Some(VirtualMemory::new(file_size))
            } else {
                None
            },
            extent: None,
            sampled: false,
        })
    }

    /// Total vertices in the file.
    pub fn total_vertices(&self) -> usize {
        self.total_vertices
    }

    /// Whether the file's header carries the two scanner attributes, which the
    /// header answers before a single point has been read — so the viewport can
    /// say "this model has no intensity" rather than waiting, and painting by a
    /// channel that is not there is the one answer that has to be right.
    pub fn has_intensity(&self) -> bool {
        self.streamer.has_intensity()
    }

    pub fn has_class(&self) -> bool {
        self.streamer.has_class()
    }

    /// Whether all vertices have been loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Points loaded so far.
    pub fn points_loaded(&self) -> usize {
        self.octree.total_points()
    }

    /// Progress ratio [0, 1].
    pub fn progress(&self) -> f32 {
        if self.total_vertices == 0 {
            return 1.0;
        }
        (self.octree.total_points() as f32 / self.total_vertices as f32).clamp(0.0, 1.0)
    }

    /// Resident memory usage in bytes (tracked by virtual memory).
    pub fn resident_memory(&self) -> u64 {
        self.vmem.as_ref().map(|v| v.resident_memory()).unwrap_or(0)
    }

    /// Virtual memory residency ratio [0, 1]. None if not using vmem.
    pub fn vmem_ratio(&self) -> Option<f32> {
        self.vmem.as_ref().map(|v| v.residency_ratio())
    }

    /// Cap how many points stay resident; `0` for no limit.
    ///
    /// Set before the first step. Past the cap the cloud thins itself evenly,
    /// so a file larger than memory loses detail instead of failing.
    pub fn set_resident_budget(&mut self, points: usize) {
        self.octree.set_budget(points);
    }

    /// Points resident right now — what is actually available to draw.
    pub fn points_kept(&self) -> usize {
        self.octree.kept_points()
    }

    /// The bounds the viewport should frame the camera on.
    ///
    /// The union of the file's sampled extent and everything loaded so far.
    /// Both halves are independent of the camera and only ever grow, which is
    /// what keeps the model a constant size on screen while it streams in and
    /// while the user turns it: the renderable mesh is a camera-dependent
    /// subset of the cloud, so its own bounding box is not a stable frame of
    /// reference.
    pub fn framing_bounds(&self) -> Option<Bounds> {
        let loaded = self.octree.bounds();
        match (self.extent, loaded.is_empty()) {
            (Some(extent), false) => Some(Bounds {
                min: [
                    extent.min[0].min(loaded.min[0]),
                    extent.min[1].min(loaded.min[1]),
                    extent.min[2].min(loaded.min[2]),
                ],
                max: [
                    extent.max[0].max(loaded.max[0]),
                    extent.max[1].max(loaded.max[1]),
                    extent.max[2].max(loaded.max[2]),
                ],
            }),
            (Some(extent), true) => Some(extent),
            (None, false) => Some(loaded),
            (None, true) => None,
        }
    }

    /// Load up to `POINTS_PER_FRAME` more points. Call this once per frame.
    pub fn step(&mut self) -> StreamStep {
        // Sample the file's extent once, before any points are read. This
        // runs on whatever thread drives the streaming, which is a background
        // one: a few thousand random reads are not UI work.
        if !self.sampled {
            self.sampled = true;
            self.extent = self.streamer.sample_bounds(EXTENT_SAMPLES);
        }
        if self.loaded {
            return StreamStep {
                points_read: self.streamer.vertices_read(),
                points_loaded: self.octree.total_points(),
                total_points: self.total_vertices,
                complete: true,
            };
        }

        // Track pages loaded this step via virtual memory.
        if let Some(ref mut vmem) = self.vmem {
            // Simulate page loading: each chunk touches ~1 page (256KB).
            // In a real implementation, the streamer would report which
            // file offset it read from, and we'd mark those pages resident.
            let approx_page = (self.octree.total_points() / 8000) as u32;
            if !vmem.is_resident(approx_page) {
                vmem.mark_loading(approx_page);
                vmem.mark_resident(approx_page);
            }
        }

        match self.streamer.next_chunk(POINTS_PER_FRAME) {
            Ok(Some(chunk)) => {
                if !chunk.positions.is_empty() {
                    // One batch, one rebuild. Routing a whole chunk through
                    // `insert_point` rebuilds the tree every ten thousand
                    // points — a pass over everything already resident, per
                    // ten thousand — which makes streaming a file cost its own
                    // size again for every position. The batch path keeps the
                    // budget and the thinning rules and skips only the churn.
                    self.octree.insert_points(
                        &chunk.positions,
                        &chunk.colors,
                        &chunk.intensities,
                        &chunk.classes,
                    );
                }
                if chunk.is_last {
                    self.loaded = true;
                    self.octree.rebuild();
                }
            }
            Ok(None) => {
                self.loaded = true;
                self.octree.rebuild();
            }
            Err(_e) => {
                // On error, mark as loaded to stop retrying.
                self.loaded = true;
            }
        }

        StreamStep {
            points_read: self.streamer.vertices_read(),
            points_loaded: self.octree.total_points(),
            total_points: self.total_vertices,
            complete: self.loaded,
        }
    }

    /// Get a renderable mesh for the current state, with frustum culling
    /// and LOD applied.
    pub fn render_mesh(&mut self, frustum: &Frustum, camera_pos: [f32; 3]) -> Mesh {
        self.octree
            .to_mesh_lod(frustum, camera_pos, DEFAULT_POINT_BUDGET)
    }

    /// Get a renderable mesh without frustum culling (full cloud at LOD).
    pub fn render_mesh_all(&mut self, camera_pos: [f32; 3]) -> Mesh {
        self.octree
            .to_mesh_lod(&Frustum::everything(), camera_pos, DEFAULT_POINT_BUDGET)
    }

    /// Bounds of loaded points so far (for camera framing).
    pub fn loaded_bounds(&self) -> Bounds {
        self.octree.bounds()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("trove-stream-cloud-{}-{name}", std::process::id()))
    }

    /// Write a binary PLY cloud: a dense blob near the origin plus two far
    /// points at the end of the file, the shape a scan has and the shape a
    /// camera-dependent framing gets wrong.
    fn write_cloud(path: &std::path::Path, blob: usize) {
        let mut body = Vec::new();
        let mut header = String::new();
        let total = blob + 2;
        header.push_str(&format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex {total}\n\
             property float x\nproperty float y\nproperty float z\nend_header\n"
        ));
        for i in 0..blob {
            let t = i as f32 * 0.001;
            for value in [t, -t, 0.0f32] {
                body.extend_from_slice(&value.to_le_bytes());
            }
        }
        for point in [[-900.0f32, -900.0, -900.0], [900.0, 900.0, 900.0]] {
            for value in point {
                body.extend_from_slice(&value.to_le_bytes());
            }
        }
        let mut file = header.into_bytes();
        file.extend_from_slice(&body);
        std::fs::write(path, &file).unwrap();
    }

    /// Write a binary PLY cloud whose points are exactly those given, so a
    /// test can plant a non-finite coordinate on purpose.
    fn write_points(path: &std::path::Path, points: &[[f32; 3]]) {
        let mut file = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex {}\n\
             property float x\nproperty float y\nproperty float z\nend_header\n",
            points.len()
        )
        .into_bytes();
        for point in points {
            for value in point {
                file.extend_from_slice(&value.to_le_bytes());
            }
        }
        std::fs::write(path, &file).unwrap();
    }

    /// The same two attributes on the streaming path, which is what most clouds
    /// between a few hundred megabytes and a few gigabytes arrive by: no index,
    /// just the file read in chunks. They have to come out of the file, through
    /// the octree and into the mesh the renderers draw, or a scan can be painted
    /// in its thumbnail and nowhere else.
    #[test]
    fn a_streamed_scan_renders_with_its_own_channels() {
        let dir = temp("scan");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scan.ply");
        let count = 30usize;
        let mut file = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex {count}\n\
             property float x\nproperty float y\nproperty float z\n\
             property float intensity\nproperty uchar classification\nend_header\n"
        )
        .into_bytes();
        for index in 0..count {
            for value in [index as f32 * 0.5, 0.0, 0.0] {
                file.extend_from_slice(&value.to_le_bytes());
            }
            // The intensity is the point's own number, and the class follows
            // from it, so a pair that came back together says whose point it is.
            file.extend_from_slice(&(index as f32 * 7.0).to_le_bytes());
            file.push((index % 23) as u8);
        }
        std::fs::write(&path, &file).unwrap();

        let mut cloud = StreamingPointCloud::open(&path).expect("a scan streams");
        assert!(cloud.has_intensity());
        assert!(cloud.has_class());
        while !cloud.step().complete {}
        let mesh = cloud.render_mesh_all([0.0, 0.0, 0.0]);
        assert_eq!(mesh.vertex_count(), count);
        let fields = mesh
            .fields
            .as_deref()
            .expect("the mesh carries the channels");
        for (point, (intensity, class)) in mesh
            .positions
            .iter()
            .zip(fields.intensities.iter().zip(&fields.classes))
        {
            let index = (intensity / 7.0).round() as usize;
            assert!(
                (point[0] - index as f32 * 0.5).abs() < 1e-3,
                "{point:?} carries the intensity {intensity}"
            );
            assert_eq!(*class as usize, index % 23, "{point:?} lost its class");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A scan written with NaN for its unobserved points must load, not hang.
    /// The octree used to loop forever growing its bounds around the NaN —
    /// every comparison with NaN is false, so the point could never be inside
    /// them. This is the regression that took the whole viewport down at 100%
    /// CPU.
    #[test]
    fn a_cloud_with_nan_points_still_loads() {
        let path = temp("nan.ply");
        write_points(
            &path,
            &[
                [0.0, 0.0, 0.0],
                [f32::NAN, 0.0, 0.0],
                [1.0, 1.0, 1.0],
                [f32::INFINITY, 0.0, 0.0],
                [0.5, 0.5, 0.5],
            ],
        );
        let mut cloud = StreamingPointCloud::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();

        let mut steps = 0;
        loop {
            let step = cloud.step();
            steps += 1;
            if step.complete {
                assert_eq!(step.points_read, 5, "every record is read");
                break;
            }
            assert!(steps < 100, "the cloud must finish");
        }
        // The three finite points are the ones on screen.
        assert_eq!(cloud.points_kept(), 3);
        let mesh = cloud.render_mesh_all([0.0, 0.0, 0.0]);
        assert!(mesh.vertex_count() > 0);
        assert!(
            mesh.positions
                .iter()
                .all(|p| p.iter().all(|v| v.is_finite()))
        );
        assert!(mesh.bounds.min.iter().all(|v| v.is_finite()));
        assert!(mesh.bounds.max.iter().all(|v| v.is_finite()));
    }

    /// Progress is measured in points *read*, not points kept: once the cloud
    /// is thinning to stay inside its budget, the kept count stops tracking the
    /// file and a progress bar driven by it would stall.
    #[test]
    fn progress_counts_points_read_not_points_kept() {
        let path = temp("budget.ply");
        write_cloud(&path, 120_000);
        let mut cloud = StreamingPointCloud::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        // A budget far below the file, so thinning definitely happens.
        cloud.set_resident_budget(5_000);

        let mut last = None;
        loop {
            let step = cloud.step();
            assert!(
                step.points_read >= step.points_loaded,
                "read {} vs kept {}",
                step.points_read,
                step.points_loaded
            );
            if let Some(previous) = last {
                assert!(step.points_read > previous, "progress must advance");
            }
            last = Some(step.points_read);
            if step.complete {
                assert_eq!(step.points_read, 120_002, "every point was read");
                assert!(
                    step.points_loaded <= 10_000,
                    "the budget must bound what is kept: {}",
                    step.points_loaded
                );
                break;
            }
        }
    }

    /// The frame the camera uses must never shrink while the cloud streams:
    /// it is the union of everything the file says it holds and everything
    /// loaded so far, so more data can only widen it. Anything that moved it
    /// the other way would rescale the model under the user mid-rotation.
    #[test]
    fn framing_bounds_never_shrink_while_the_cloud_streams() {
        let path = temp("extent.ply");
        // A dense blob near the origin plus two far outliers at the *end* of
        // the file: the case a strided sample can miss and the loaded points
        // have to catch.
        write_cloud(&path, 120_000);
        let mut cloud = StreamingPointCloud::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(cloud.total_vertices(), 120_002);

        let mut previous: Option<Bounds> = None;
        let mut steps = 0;
        loop {
            let step = cloud.step();
            steps += 1;
            if let Some(bounds) = cloud.framing_bounds() {
                if let Some(previous) = previous {
                    for axis in 0..3 {
                        assert!(
                            bounds.min[axis] <= previous.min[axis] + 1e-6,
                            "step {steps} pulled the frame in: {bounds:?} vs {previous:?}"
                        );
                        assert!(bounds.max[axis] >= previous.max[axis] - 1e-6);
                    }
                }
                previous = Some(bounds);
            }
            if step.complete {
                break;
            }
            assert!(steps < 100, "streaming should finish");
        }
        assert!(steps > 1, "the cloud should take more than one chunk");
        // Once everything is loaded the frame holds the whole cloud.
        let bounds = cloud.framing_bounds().expect("bounds after loading");
        assert_eq!(bounds.min, [-900.0, -900.0, -900.0]);
        assert_eq!(bounds.max, [900.0, 900.0, 900.0]);
    }
}
