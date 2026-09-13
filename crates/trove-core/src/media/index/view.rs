//! Reading a cloud out of its index, a region at a time.
//!
//! The index exists so a renderer can draw part of a cloud without reading all
//! of it (see [`super::file`]). This module is the reader side of that bargain:
//! it keeps the chunks the camera can see, nearest first, up to a point
//! budget, and hands the viewport a renderable mesh at any point along the way.
//!
//! Nothing here builds an index — a reader that finds none simply is not used,
//! and the viewport falls back to streaming the source file. That keeps opening
//! a model from ever paying for an index nobody asked for; the offline
//! `index_build` example is what writes one.
//!
//! The chunks are kept in a [`StreamingOctree`], the same structure the
//! streaming reader fills: it thins to a budget and answers "the nearest
//! `n` points" cheaply, so a four-million-point resident set still draws as
//! the few hundred thousand points the renderers accept.

use std::path::{Path, PathBuf};

use super::file::CloudIndex;
use crate::media::formats::point_cloud::{Frustum, StreamingOctree};
use crate::media::formats::streaming_point_cloud::{DEFAULT_POINT_BUDGET, DEFAULT_RESIDENT_POINTS};
use crate::media::formats::types::{Bounds, Mesh};

/// Extension of an index file that sits beside the cloud it describes.
pub const INDEX_EXTENSION: &str = "trovecloud";

/// Points read per [`IndexedCloud::step`].
///
/// Small enough that a step is not a visible pause on the background thread,
/// large enough that filling a four-million-point budget — around sixty
/// chunks — takes a handful of steps rather than dozens.
const POINTS_PER_STEP: usize = 400_000;

/// The index file that belongs to `source`, or is meant to.
///
/// The convention is the one the `index_build` example writes by default:
/// the source path with its extension replaced. A sidecar rather than a
/// library cache because the reader has to find it from the path alone, with
/// no content hash and no import record — and because the files that need an
/// index are the ones imported as links, whose path is the user's own.
pub fn index_path_for(source: &Path) -> PathBuf {
    source.with_extension(INDEX_EXTENSION)
}

/// What one [`IndexedCloud::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStep {
    /// Chunks resident so far — how much of the index has been touched.
    pub chunks_read: usize,
    /// Chunks the index holds.
    pub chunks_total: usize,
    /// Points resident, which the resident budget bounds.
    pub points_loaded: usize,
    /// Points the index holds.
    pub points_total: u64,
    /// Nothing more will be read: every chunk is resident, or the point
    /// budget is full. The caller can stop stepping.
    pub complete: bool,
}

/// A point cloud read from its index, progressively.
///
/// Owns the resident points and the record of which chunks they came from.
/// Each [`IndexedCloud::step`] reads one more batch of chunks — the visible
/// ones nearest the camera, so the first frame is the surface the user is
/// looking at — and every state in between is renderable.
pub struct IndexedCloud {
    index: CloudIndex,
    /// The resident points, thinned to the budget and queried for a visible
    /// subset by the renderers.
    octree: StreamingOctree,
    /// One flag per chunk, in index order.
    read: Vec<bool>,
    /// How many of `read` are set.
    read_chunks: usize,
    /// Points that may stay resident; `0` for no limit.
    budget: usize,
    /// The budget is full, so reading more would only be thrown away.
    budget_full: bool,
}

impl IndexedCloud {
    /// Open an index file for progressive reading.
    ///
    /// Validates the header and the chunk table (not every point: that would
    /// be the full read this exists to avoid). A damaged run is skipped when
    /// it is reached rather than rejected here.
    pub fn open(path: &Path) -> Result<Self, String> {
        let index = CloudIndex::open(path)?;
        let bounds = index.bounds();
        // The header already holds the extent, so the tree never has to grow
        // its bounds — which is what would otherwise re-index every resident
        // point when a far chunk arrived.
        let mut octree = if bounds.is_empty() {
            StreamingOctree::empty()
        } else {
            StreamingOctree::new(bounds)
        };
        octree.set_budget(DEFAULT_RESIDENT_POINTS);
        let chunks = index.chunk_count();
        Ok(Self {
            index,
            octree,
            read: vec![false; chunks],
            read_chunks: 0,
            budget: DEFAULT_RESIDENT_POINTS,
            budget_full: false,
        })
    }

    /// Points the whole cloud holds.
    pub fn point_count(&self) -> u64 {
        self.index.point_count()
    }

    /// Chunks the index was cut into.
    pub fn chunk_count(&self) -> usize {
        self.index.chunk_count()
    }

    /// Chunks read so far.
    pub fn chunks_read(&self) -> usize {
        self.read_chunks
    }

    /// Points resident — what a render can draw, before the renderers' own
    /// visible-point budget.
    pub fn points_loaded(&self) -> usize {
        self.octree.total_points()
    }

    /// The cloud's extent, from the index header. Camera framing uses it: it
    /// is the whole model, not the loaded part, so the model does not rescale
    /// while it fills in.
    pub fn bounds(&self) -> Bounds {
        self.index.bounds()
    }

    /// Whether the cloud carries colours.
    pub fn has_colors(&self) -> bool {
        self.index.has_colors()
    }

    /// Whether a chunk's points are resident. A renderer asks this to tell
    /// "not loaded yet" from "loaded and empty".
    pub fn is_chunk_resident(&self, chunk: usize) -> bool {
        self.read.get(chunk).copied().unwrap_or(false)
    }

    /// Whether there is nothing left to read.
    pub fn is_complete(&self) -> bool {
        self.budget_full || self.read_chunks == self.index.chunk_count()
    }

    /// Cap how many points stay resident; `0` for no limit. Set before the
    /// first step.
    pub fn set_resident_budget(&mut self, points: usize) {
        self.budget = points;
        self.octree.set_budget(points);
    }

    /// Read the next batch of chunks, and report how far the cloud has come.
    ///
    /// Chunks the camera can see come before the ones behind it, and the
    /// order interleaves nearest-first with spread-across-the-cloud (see
    /// [`IndexedCloud::pending_order`]). The batch is bounded by
    /// [`POINTS_PER_STEP`] and never crosses the resident budget: a chunk is
    /// read whole (half a run is a seek and a decode thrown away) and a run
    /// that does not fit is left for a later, closer view.
    pub fn step(&mut self, frustum: &Frustum, camera_pos: [f32; 3]) -> IndexStep {
        if self.is_complete() {
            return self.summary(true);
        }
        let order = self.pending_order(camera_pos, Some(frustum));

        let mut added = 0usize;
        for chunk in order {
            if self.read[chunk] {
                continue;
            }
            let Some(entry) = self.index.chunk(chunk) else {
                continue;
            };
            if self.budget > 0 && self.octree.total_points() + entry.count as usize > self.budget {
                self.budget_full = true;
                break;
            }
            // A run that cannot be read is skipped, not retried: one damaged
            // block must not stop the walk reaching the rest.
            if let Ok(batch) = self.index.read_chunk(chunk) {
                self.octree.insert_points(&batch.points, &batch.colors);
                added += batch.points.len();
            }
            self.read[chunk] = true;
            self.read_chunks += 1;
            if added >= POINTS_PER_STEP || self.read_chunks == self.index.chunk_count() {
                break;
            }
        }
        self.summary(self.is_complete())
    }

    /// The renderable cloud: the resident points nearest the camera, capped at
    /// the renderers' own point budget.
    ///
    /// No frustum: the resident set is already the visible chunks, and a mesh
    /// that changed with the camera would re-upload every frame. The distance
    /// ordering inside the octree is what keeps the nearest surface complete.
    pub fn render_mesh(&mut self, camera_pos: [f32; 3]) -> Mesh {
        self.octree
            .to_mesh_lod(&Frustum::everything(), camera_pos, DEFAULT_POINT_BUDGET)
    }

    /// Chunks not yet resident, in the order they should be read.
    ///
    /// Nearest-first alone spends the whole budget on the side of the model
    /// facing the camera: a forty-million-point scan then previews as one
    /// dense patch, with nothing where the rest of it should be. Spread alone
    /// would ignore the camera and stay coarse wherever the user looks. The
    /// order interleaves the two, so half the budget goes to the nearest
    /// chunks and half is spread across the cloud — detail at the camera,
    /// coverage everywhere else.
    ///
    /// A `frustum` of `None`, or one that sees no chunk at all, falls back to
    /// ranking by distance alone: a camera inside a hole must not load nothing
    /// forever. Ties break on chunk index throughout, so a given camera always
    /// produces the same order — which is what makes the order testable.
    fn pending_order(&self, camera_pos: [f32; 3], frustum: Option<&Frustum>) -> Vec<usize> {
        let unread_visible = |frustum: Option<&Frustum>| -> Vec<usize> {
            (0..self.index.chunk_count())
                .filter(|&chunk| !self.read[chunk])
                .filter(|&chunk| match (frustum, self.index.chunk(chunk)) {
                    (Some(frustum), Some(entry)) => frustum.intersects_bounds(&entry.bounds),
                    _ => true,
                })
                .collect()
        };
        let mut candidates = unread_visible(frustum);
        if candidates.is_empty() {
            candidates = unread_visible(None);
        }

        let mut nearest = candidates.clone();
        nearest.sort_by(|&a, &b| {
            self.chunk_distance(a, camera_pos)
                .total_cmp(&self.chunk_distance(b, camera_pos))
                .then(a.cmp(&b))
        });
        let mut spread = candidates;
        // Bit-reversal is the classic progressive-refinement order: the first
        // few entries land in as many different regions of the Morton-ordered
        // file as there are bits, so an early stop still covers the cloud.
        spread.sort_by_key(|&chunk| (chunk.reverse_bits(), chunk));

        let mut taken = vec![false; self.index.chunk_count()];
        let mut order = Vec::with_capacity(nearest.len());
        let (mut n, mut s) = (0usize, 0usize);
        while order.len() < nearest.len() {
            if n < nearest.len() {
                let chunk = nearest[n];
                n += 1;
                if !taken[chunk] {
                    taken[chunk] = true;
                    order.push(chunk);
                }
            }
            if s < spread.len() && order.len() < nearest.len() {
                let chunk = spread[s];
                s += 1;
                if !taken[chunk] {
                    taken[chunk] = true;
                    order.push(chunk);
                }
            }
            if n >= nearest.len() && s >= spread.len() {
                break;
            }
        }
        order
    }

    /// Distance from a point to a chunk's box: zero inside it.
    fn chunk_distance(&self, chunk: usize, camera_pos: [f32; 3]) -> f32 {
        self.index
            .chunk(chunk)
            .map_or(f32::INFINITY, |entry| distance2(camera_pos, entry.bounds))
    }

    /// The state every reader reports, for a step that changed little or
    /// nothing.
    fn summary(&self, complete: bool) -> IndexStep {
        IndexStep {
            chunks_read: self.read_chunks,
            chunks_total: self.index.chunk_count(),
            points_loaded: self.octree.total_points(),
            points_total: self.index.point_count(),
            complete,
        }
    }
}

/// Squared distance from a point to a box: zero inside it.
///
/// Squared, and on the box rather than its centre, because a long thin chunk
/// is near the camera along its whole length even when its centre is not.
fn distance2(point: [f32; 3], bounds: Bounds) -> f32 {
    let mut total = 0.0;
    for (axis, value) in point.iter().enumerate() {
        let gap = if *value < bounds.min[axis] {
            bounds.min[axis] - *value
        } else if *value > bounds.max[axis] {
            *value - bounds.max[axis]
        } else {
            0.0
        };
        total += gap * gap;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::formats::point_cloud::Plane;
    use crate::media::index::file::{BatchSource, IndexConfig, PointBatch, build_index};
    use crate::media::index::order::Grid;

    /// A batch source over an in-memory list, so a test needs no PLY.
    struct Batches {
        points: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        at: usize,
    }

    impl BatchSource for Batches {
        fn next_batch(&mut self) -> Result<Option<PointBatch>, String> {
            if self.at >= self.points.len() {
                return Ok(None);
            }
            let to = (self.at + 64).min(self.points.len());
            let points = self.points[self.at..to].to_vec();
            let colors = if self.colors.is_empty() {
                Vec::new()
            } else {
                self.colors[self.at..to].to_vec()
            };
            self.at = to;
            Ok(Some(PointBatch { points, colors }))
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("trove-index-view-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Clusters of `per_cluster` points along +X, jittered across y and z, so
    /// distance decides which is nearest but the chunks do not line up with
    /// the clusters.
    fn clusters(count: usize, per_cluster: usize) -> Vec<[f32; 3]> {
        let mut points = Vec::with_capacity(count * per_cluster);
        for cluster in 0..count {
            for index in 0..per_cluster {
                let jitter = index as f32 / per_cluster as f32;
                points.push([cluster as f32 * 6.0 + jitter, jitter, 1.0 - jitter]);
            }
        }
        points
    }

    /// One tight cluster every hundred units: far enough apart that Morton
    /// order keeps each cluster in its own chunk, so a chunk index names a
    /// place in the cloud.
    fn spaced_clusters(count: usize, per_cluster: usize) -> Vec<[f32; 3]> {
        let mut points = Vec::with_capacity(count * per_cluster);
        for cluster in 0..count {
            for index in 0..per_cluster {
                let jitter = index as f32 / per_cluster as f32 * 0.01;
                points.push([cluster as f32 * 100.0 + jitter, jitter, jitter]);
            }
        }
        points
    }

    fn cloud_index(
        dir: &Path,
        points: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        per_chunk: usize,
    ) -> PathBuf {
        let mut bounds = Bounds::empty();
        for point in &points {
            bounds.extend(*point);
        }
        let grid = Grid::covering(bounds, 12);
        let mut source = Batches {
            points,
            colors,
            at: 0,
        };
        let out = dir.join("cloud.trovecloud");
        build_index(
            &mut source,
            grid,
            IndexConfig {
                points_per_chunk: per_chunk,
                sort_capacity: 4096,
                ..Default::default()
            },
            dir,
            &out,
        )
        .expect("index builds");
        out
    }

    /// A frustum with every plane rejecting everything.
    fn blind() -> Frustum {
        Frustum {
            planes: [Plane {
                normal: [0.0, 1.0, 0.0],
                distance: -1.0e9,
            }; 6],
        }
    }

    #[test]
    fn the_index_lives_beside_its_source() {
        assert_eq!(
            index_path_for(Path::new("/tmp/scan.ply")),
            PathBuf::from("/tmp/scan.trovecloud")
        );
        assert_eq!(
            index_path_for(Path::new("/tmp/scan")),
            PathBuf::from("/tmp/scan.trovecloud")
        );
    }

    #[test]
    fn an_index_opens_into_a_cloud() {
        let dir = scratch_dir("opens");
        let points = clusters(4, 64);
        let path = cloud_index(&dir, points.clone(), Vec::new(), 64);
        let cloud = IndexedCloud::open(&path).expect("opens");

        assert_eq!(cloud.point_count(), points.len() as u64);
        assert_eq!(cloud.chunk_count(), 4);
        assert!(!cloud.has_colors());
        assert_eq!(cloud.chunks_read(), 0);
        assert_eq!(cloud.points_loaded(), 0);
        assert!(!cloud.is_complete());
        assert!(cloud.bounds().min[0] <= 0.0 && cloud.bounds().max[0] >= 18.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The first chunk read is the one the camera is standing in — the whole
    /// point of ordering by visibility rather than by file order.
    #[test]
    fn the_chunk_nearest_the_camera_is_read_first() {
        let dir = scratch_dir("nearest");
        let path = cloud_index(&dir, clusters(4, 64), Vec::new(), 64);
        let index = CloudIndex::open(&path).expect("index opens");
        let mut cloud = IndexedCloud::open(&path).expect("cloud opens");
        // Room for a single chunk, so exactly one is read.
        cloud.set_resident_budget(80);

        // The first cluster runs from x = 0; the others are six units apart.
        let step = cloud.step(&Frustum::everything(), [0.5, 0.5, 0.5]);
        assert_eq!(step.chunks_read, 1);
        let resident: Vec<usize> = (0..cloud.chunk_count())
            .filter(|&chunk| cloud.is_chunk_resident(chunk))
            .collect();
        assert_eq!(resident.len(), 1);
        let bounds = index.chunk(resident[0]).expect("chunk exists").bounds;
        assert!(
            bounds.min[0] <= 0.5 && bounds.max[0] >= 0.5,
            "read the chunk at {bounds:?} instead of the one at the camera"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A budget smaller than the cloud must not spend itself on the near
    /// side: a whole-model view would then be one dense patch with holes
    /// everywhere else. Half the order is spread across the file, so a budget
    /// of three chunks reaches a cluster far from the camera.
    #[test]
    fn the_resident_set_spans_the_cloud_not_just_the_near_side() {
        let dir = scratch_dir("coverage");
        let path = cloud_index(&dir, spaced_clusters(8, 64), Vec::new(), 64);
        let index = CloudIndex::open(&path).expect("index opens");
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        // Three chunks: enough for one spread pick to make it in.
        cloud.set_resident_budget(192);

        let step = cloud.step(&Frustum::everything(), [0.05, 0.05, 0.05]);
        assert_eq!(step.chunks_read, 3);
        let far_side = (0..cloud.chunk_count())
            .filter(|&chunk| cloud.is_chunk_resident(chunk))
            .filter_map(|chunk| index.chunk(chunk))
            .any(|entry| entry.bounds.min[0] > 50.0);
        assert!(far_side, "the budget went entirely to the near side");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The budget is what makes a twenty-gigabyte cloud maintainable: once it
    /// is full the reader stops, even though chunks remain.
    #[test]
    fn the_resident_budget_bounds_the_cloud() {
        let dir = scratch_dir("budget");
        let path = cloud_index(&dir, clusters(8, 64), Vec::new(), 64);
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        cloud.set_resident_budget(200);

        let mut steps = 0;
        loop {
            let step = cloud.step(&Frustum::everything(), [0.5, 0.5, 0.5]);
            steps += 1;
            if step.complete {
                break;
            }
            assert!(steps < 50, "the reader should stop at the budget");
        }
        assert!(cloud.points_loaded() <= 200, "{}", cloud.points_loaded());
        assert!(
            cloud.chunks_read() < cloud.chunk_count(),
            "the budget must leave chunks unread"
        );
        assert!(cloud.is_complete(), "a full budget ends the walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// With room for everything, the walk reaches the end and the resident
    /// points are the whole cloud.
    #[test]
    fn reading_every_chunk_completes_the_cloud() {
        let dir = scratch_dir("complete");
        let points = clusters(5, 64);
        let path = cloud_index(&dir, points.clone(), Vec::new(), 64);
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        cloud.set_resident_budget(0);

        loop {
            let step = cloud.step(&Frustum::everything(), [0.5, 0.5, 0.5]);
            if step.complete {
                assert_eq!(step.chunks_read, step.chunks_total);
                break;
            }
        }
        assert_eq!(cloud.points_loaded(), points.len());
        assert_eq!(cloud.points_loaded(), cloud.point_count() as usize);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every state along the way is renderable, and colours come with the
    /// points.
    #[test]
    fn a_partly_read_cloud_renders_what_it_has() {
        let dir = scratch_dir("renders");
        let points = clusters(4, 64);
        let colors: Vec<[f32; 3]> = (0..points.len())
            .map(|index| [index as f32 / points.len() as f32, 0.25, 0.75])
            .collect();
        let path = cloud_index(&dir, points, colors, 64);
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        cloud.set_resident_budget(80);

        cloud.step(&Frustum::everything(), [0.5, 0.5, 0.5]);
        let mesh = cloud.render_mesh([0.5, 0.5, 0.5]);
        assert!(mesh.is_point_cloud());
        assert_eq!(mesh.vertex_count(), cloud.points_loaded());
        assert_eq!(mesh.colors.len(), mesh.positions.len());
        assert!(!mesh.bounds.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A frustum that sees nothing must not turn into an infinite "loading":
    /// the fallback is distance alone.
    #[test]
    fn a_blind_frustum_falls_back_to_distance() {
        let dir = scratch_dir("blind");
        let path = cloud_index(&dir, clusters(4, 64), Vec::new(), 64);
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        // Room for a single chunk, so "something was read" is one chunk.
        cloud.set_resident_budget(80);

        let step = cloud.step(&blind(), [0.5, 0.5, 0.5]);
        assert_eq!(step.chunks_read, 1, "the nearest chunk is still read");
        assert!(step.points_loaded > 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stepping after the cloud is complete is a no-op, not an error and not
    /// another read.
    #[test]
    fn stepping_a_finished_cloud_changes_nothing() {
        let dir = scratch_dir("finished");
        let path = cloud_index(&dir, clusters(3, 64), Vec::new(), 64);
        let mut cloud = IndexedCloud::open(&path).expect("opens");
        cloud.set_resident_budget(0);
        while !cloud.step(&Frustum::everything(), [0.0, 0.0, 0.0]).complete {}

        let before = cloud.points_loaded();
        let step = cloud.step(&Frustum::everything(), [0.0, 0.0, 0.0]);
        assert!(step.complete);
        assert_eq!(step.points_loaded, before);
        assert_eq!(cloud.points_loaded(), before);

        std::fs::remove_dir_all(&dir).ok();
    }
}
