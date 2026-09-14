//! Spatial index for large point clouds.
//!
//! An octree over point positions that supports:
//! * **Frustum culling** — skip nodes outside the camera frustum
//! * **Distance-based LOD** — drop distant nodes to hit a target point count
//! * **Streaming-friendly** — build incrementally, query without allocation
//!
//! The index is built once at import / load time from a [`Mesh`] that holds
//! a point cloud (no triangles). It carries no reference to the original
//! data and can be cheaply cloned for the renderer to hold.

use crate::media::formats::types::{Bounds, Mesh};

/// Maximum points per octree leaf before it splits (unless `max_depth` is hit).
pub const MAX_POINTS_PER_NODE: usize = 1024;
/// Maximum tree depth. 20 levels → 1M³ resolution at unit scale.
pub const MAX_DEPTH: u32 = 20;

/// Colour given to a point in a coloured cloud that carries none of its own.
///
/// It is the neutral surface colour the renderers fall back to
/// ([`crate::media::render3d::MATERIAL`]), so a mixed cloud looks like one
/// cloud rather than two.
const POINT_FALLBACK_COLOR: [f32; 3] = crate::media::render3d::MATERIAL;

/// Byte order for binary data.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// An axis-aligned plane of a frustum.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    /// Unit normal pointing into the visible half-space.
    pub normal: [f32; 3],
    /// Distance from the origin along the normal.
    pub distance: f32,
}

impl Plane {
    /// Signed distance from the plane to a point. Positive = inside.
    #[inline]
    pub fn signed_distance(&self, point: [f32; 3]) -> f32 {
        let dot = self.normal[0] * point[0] + self.normal[1] * point[1] + self.normal[2] * point[2];
        dot + self.distance
    }
}

/// A view frustum: six planes (left, right, top, bottom, near, far).
#[derive(Debug, Clone)]
pub struct Frustum {
    pub planes: [Plane; 6],
}

impl Frustum {
    /// Build a frustum from a **row-major** view-projection matrix, whose
    /// depth maps to `0..=1` (the convention [`Framing::view_projection`]
    /// produces for wgpu).
    ///
    /// [`Framing::view_projection`]: crate::media::render3d::Framing::view_projection
    pub fn from_matrix(m: &[[f32; 4]; 4]) -> Self {
        let m = *m;
        // Plane normals point inward. Rows 0 and 1 give the four side planes
        // for either depth convention; the near and far planes are the part
        // that depends on it: with `0..=1` depth the near plane is `z = 0`
        // (row 2) and the far plane is `z = w` (row 3 - row 2). An OpenGL
        // `-1..=1` matrix would instead use `w ± z` for both, which would put
        // the near plane where the far one belongs.
        let mut planes = [Plane {
            normal: [0.0; 3],
            distance: 0.0,
        }; 6];

        // Left:  row3 + row0
        planes[0] = plane_from_rows(&m[3], &m[0]);
        // Right: row3 - row0
        planes[1] = plane_from_rows_neg(&m[3], &m[0]);
        // Bottom: row3 + row1
        planes[2] = plane_from_rows(&m[3], &m[1]);
        // Top:    row3 - row1
        planes[3] = plane_from_rows_neg(&m[3], &m[1]);
        // Near:   row2
        planes[4] = normalise_plane(m[2][0], m[2][1], m[2][2], m[2][3]);
        // Far:    row3 - row2
        planes[5] = plane_from_rows_neg(&m[3], &m[2]);

        Self { planes }
    }

    /// A frustum that contains everything, for the paths that want the
    /// visibility pass without any culling.
    ///
    /// Every plane is infinitely far out, so no finite box is ever rejected —
    /// which is what a caller wants to say "draw whatever LOD and point
    /// budget allow", not "cull against a camera".
    pub fn everything() -> Self {
        let axis = |normal: [f32; 3]| Plane {
            normal,
            distance: f32::INFINITY,
        };
        Self {
            planes: [
                axis([1.0, 0.0, 0.0]),
                axis([-1.0, 0.0, 0.0]),
                axis([0.0, 1.0, 0.0]),
                axis([0.0, -1.0, 0.0]),
                axis([0.0, 0.0, 1.0]),
                axis([0.0, 0.0, -1.0]),
            ],
        }
    }

    /// Test whether an AABB intersects the frustum (or is fully inside).
    /// Returns `true` if any part of the box is visible.
    pub fn intersects_bounds(&self, bounds: &Bounds) -> bool {
        let min = bounds.min;
        let max = bounds.max;
        for plane in &self.planes {
            // Find the corner of the box that is most opposite to the plane normal.
            let nx = if plane.normal[0] > 0.0 {
                min[0]
            } else {
                max[0]
            };
            let ny = if plane.normal[1] > 0.0 {
                min[1]
            } else {
                max[1]
            };
            let nz = if plane.normal[2] > 0.0 {
                min[2]
            } else {
                max[2]
            };
            if plane.signed_distance([nx, ny, nz]) < 0.0 {
                return false; // entirely outside this plane
            }
        }
        true
    }
}

/// Build a plane (a, b, c, d) from the sum of two matrix rows, normalised.
fn plane_from_rows(r3: &[f32; 4], r: &[f32; 4]) -> Plane {
    let a = r3[0] + r[0];
    let b = r3[1] + r[1];
    let c = r3[2] + r[2];
    let d = r3[3] + r[3];
    normalise_plane(a, b, c, d)
}

/// Build a plane from (row3 - row), normalised.
fn plane_from_rows_neg(r3: &[f32; 4], r: &[f32; 4]) -> Plane {
    let a = r3[0] - r[0];
    let b = r3[1] - r[1];
    let c = r3[2] - r[2];
    let d = r3[3] - r[3];
    normalise_plane(a, b, c, d)
}

fn normalise_plane(a: f32, b: f32, c: f32, d: f32) -> Plane {
    let len = (a * a + b * b + c * c).sqrt().max(1e-9);
    Plane {
        normal: [a / len, b / len, c / len],
        distance: d / len,
    }
}

/// One node in the octree.
#[derive(Debug, Clone)]
pub struct OctreeNode {
    /// Bounds of this node's region.
    pub bounds: Bounds,
    /// Indices into the original point array. Empty for interior nodes.
    pub point_indices: Vec<usize>,
    /// Eight children. `None` for leaf nodes.
    pub children: Option<Box<[OctreeNode; 8]>>,
    /// Depth in the tree (root = 0).
    pub depth: u32,
}

impl OctreeNode {
    /// Whether this node is a leaf.
    #[inline]
    pub fn is_leaf(&self) -> bool {
        self.children.is_none()
    }
}

/// Octree spatial index over point positions.
///
/// Built from a point cloud, queried by frustum + distance to produce a
/// simplified point set for rendering.
#[derive(Debug, Clone)]
pub struct Octree {
    root: OctreeNode,
    /// Original points (positions). The indices stored in nodes point here.
    positions: Vec<[f32; 3]>,
    /// Optional per-point colors (parallel to `positions`).
    colors: Vec<[f32; 3]>,
    /// Optional per-point normals (parallel to `positions`).
    normals: Vec<[f32; 3]>,
}

impl Octree {
    /// Build an octree from a point cloud [`Mesh`]. Returns `None` if the
    /// mesh holds no points.
    pub fn from_point_cloud(mesh: &Mesh) -> Option<Self> {
        if mesh.positions.is_empty() {
            return None;
        }
        let positions = mesh.positions.clone();
        let colors = mesh.colors.clone();
        let normals = mesh.normals.clone();

        // Compute overall bounds.
        let mut bounds = Bounds::empty();
        let mut all_finite = false;
        for p in &positions {
            if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
                bounds.extend(*p);
                all_finite = true;
            }
        }
        if !all_finite {
            return None;
        }

        // Slightly inflate zero-volume bounds (flat clouds).
        let size = bounds.size();
        for (axis, extent) in size.iter().enumerate() {
            if *extent < 1e-6 {
                let c = (bounds.min[axis] + bounds.max[axis]) * 0.5;
                bounds.min[axis] = c - 0.5;
                bounds.max[axis] = c + 0.5;
            }
        }

        let indices: Vec<usize> = positions
            .iter()
            .enumerate()
            .filter(|(_, p)| p.iter().all(|value| value.is_finite()))
            .map(|(index, _)| index)
            .collect();

        let root = Self::build_node(bounds, &positions, indices, 0);

        Some(Self {
            root,
            positions,
            colors,
            normals,
        })
    }

    /// Build a node recursively, splitting when the point count is too high
    /// and the depth limit has not been reached.
    fn build_node(
        bounds: Bounds,
        positions: &[[f32; 3]],
        indices: Vec<usize>,
        depth: u32,
    ) -> OctreeNode {
        let total_points = indices.len();

        // Decide whether to split.
        let should_split = total_points > MAX_POINTS_PER_NODE && depth < MAX_DEPTH;

        if !should_split {
            return OctreeNode {
                bounds,
                point_indices: indices,
                children: None,
                depth,
            };
        }

        // Split into eight octants.
        let center = [
            (bounds.min[0] + bounds.max[0]) * 0.5,
            (bounds.min[1] + bounds.max[1]) * 0.5,
            (bounds.min[2] + bounds.max[2]) * 0.5,
        ];

        // Bucket each point into one of eight children.
        let mut buckets: [Vec<usize>; 8] = std::array::from_fn(|_| Vec::new());
        for &idx in &indices {
            let p = positions[idx];
            let octant = ((p[0] >= center[0]) as usize)
                | (((p[1] >= center[1]) as usize) << 1)
                | (((p[2] >= center[2]) as usize) << 2);
            buckets[octant].push(idx);
        }

        // Build child bounds and recurse.
        let children: [OctreeNode; 8] = std::array::from_fn(|i| {
            let x_hi = (i & 1) != 0;
            let y_hi = (i & 2) != 0;
            let z_hi = (i & 4) != 0;
            let child_bounds = Bounds {
                min: [
                    if x_hi { center[0] } else { bounds.min[0] },
                    if y_hi { center[1] } else { bounds.min[1] },
                    if z_hi { center[2] } else { bounds.min[2] },
                ],
                max: [
                    if x_hi { bounds.max[0] } else { center[0] },
                    if y_hi { bounds.max[1] } else { center[1] },
                    if z_hi { bounds.max[2] } else { center[2] },
                ],
            };
            Self::build_node(child_bounds, positions, buckets[i].clone(), depth + 1)
        });

        OctreeNode {
            bounds,
            point_indices: Vec::new(),
            children: Some(Box::new(children)),
            depth,
        }
    }

    /// Query the octree for points visible from the given camera position
    /// within the frustum. Returns owned indices into the original point
    /// array, culled to `max_points` by dropping the most distant nodes.
    pub fn query_frustum(
        &self,
        frustum: &Frustum,
        camera_pos: [f32; 3],
        max_points: usize,
    ) -> Vec<usize> {
        let mut result = Vec::with_capacity(max_points.min(self.positions.len()));
        Self::collect_visible(
            &self.root,
            frustum,
            camera_pos,
            max_points,
            &self.positions,
            &mut result,
        );
        result
    }

    /// Visible-node collection. Pushes visible leaf points into `out`,
    /// stopping once `max_points` is reached.
    fn collect_visible(
        node: &OctreeNode,
        frustum: &Frustum,
        camera_pos: [f32; 3],
        max_points: usize,
        positions: &[[f32; 3]],
        out: &mut Vec<usize>,
    ) {
        if out.len() >= max_points {
            return;
        }

        // Frustum cull.
        if !frustum.intersects_bounds(&node.bounds) {
            return;
        }

        if node.is_leaf() {
            // Leaf: push points (or a subset if we'd exceed the budget).
            let remaining = max_points.saturating_sub(out.len());
            if remaining >= node.point_indices.len() {
                out.extend(&node.point_indices);
            } else {
                // Take the closest points to the camera.
                let mut sorted = node.point_indices.clone();
                sorted.sort_by(|&a, &b| {
                    let da = dist2(positions[a], camera_pos);
                    let db = dist2(positions[b], camera_pos);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
                out.extend(sorted.iter().take(remaining));
            }
            return;
        }

        // Interior node: recurse into children, nearest first.
        if let Some(ref children) = node.children {
            let mut child_order: Vec<usize> = (0..8).collect();
            child_order.sort_by(|&a, &b| {
                let da = dist2(bounds_center(&children[a].bounds), camera_pos);
                let db = dist2(bounds_center(&children[b].bounds), camera_pos);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            for &i in &child_order {
                Self::collect_visible(
                    &children[i],
                    frustum,
                    camera_pos,
                    max_points,
                    positions,
                    out,
                );
                if out.len() >= max_points {
                    return;
                }
            }
        }
    }

    /// Build a point cloud mesh from the LOD selection. Returns a new `Mesh`
    /// with at most `max_points` points: the visible set thinned evenly, so
    /// the sample covers the whole of it rather than its near side alone.
    pub fn to_mesh_lod(&self, frustum: &Frustum, camera_pos: [f32; 3], max_points: usize) -> Mesh {
        let mut visible = Vec::new();
        Self::collect_visible(
            &self.root,
            frustum,
            camera_pos,
            usize::MAX,
            &self.positions,
            &mut visible,
        );
        let indices = spread_sample(visible, max_points);

        // Build the simplified mesh.
        let point_count = indices.len();
        let mut positions = Vec::with_capacity(point_count);
        let has_colors = self.colors.len() == self.positions.len();
        let has_normals = self.normals.len() == self.positions.len();
        let mut colors = if has_colors {
            Vec::with_capacity(point_count)
        } else {
            Vec::new()
        };
        let mut normals = if has_normals {
            Vec::with_capacity(point_count)
        } else {
            Vec::new()
        };

        for &idx in &indices {
            positions.push(self.positions[idx]);
            if has_colors {
                colors.push(self.colors[idx]);
            }
            if has_normals {
                normals.push(self.normals[idx]);
            }
        }

        let bounds = bounds_from_points(&positions);
        Mesh {
            positions,
            normals,
            colors,
            triangles: Vec::new(),
            bounds,
        }
    }

    /// Total number of points in the index.
    pub fn total_points(&self) -> usize {
        self.positions.len()
    }

    /// Root node bounds (overall cloud bounds).
    pub fn bounds(&self) -> Bounds {
        self.root.bounds
    }

    /// Maximum depth of the tree (for debugging / tuning).
    pub fn max_depth(&self) -> u32 {
        node_max_depth(&self.root)
    }
}

// ---------------------------------------------------------------------------
// Incremental / streaming octree (SimLOD-style).
// ---------------------------------------------------------------------------

/// A streaming octree that grows as points are inserted one at a time.
///
/// Unlike [`Octree::from_point_cloud`] which requires all points up front,
/// this structure can be built incrementally — points arrive from a file
/// reader, are inserted into the tree, and the renderer can query visible
/// nodes at any time. Intermediate results are valid and useful.
///
/// Implementation: we keep a flat array of all positions and rebuild the
/// octree whenever it needs to grow. Rebuilding is O(n log n) but happens
/// rarely (only when the root bounds double or a leaf overflows), so the
/// amortized cost per insertion is O(1) for bounded clouds and O(log n)
/// for growing clouds.
pub struct StreamingOctree {
    root: OctreeNode,
    positions: Vec<[f32; 3]>,
    colors: Vec<[f32; 3]>,
    bounds: Bounds,
    /// Bounds grown by 2× each time a point falls outside. Rebuilds on grow.
    allocated_bounds: Bounds,
    /// Dirty flag: set when points are added since last rebuild.
    dirty: bool,
    /// Points inserted since last rebuild.
    pending: Vec<usize>,
    /// Points kept at most; `0` for no limit.
    ///
    /// A cloud larger than memory has to degrade, and the only useful way to
    /// degrade is in detail: keeping the first *n* points of a file would show
    /// one corner of the scan, so instead the tree thins out evenly as it
    /// fills (see [`StreamingOctree::decimate`]) and every region keeps a
    /// share of what it had. Memory stays bounded, and the shape of the model
    /// stays recognisable however large the file is.
    budget: usize,
    /// One in this many arriving points is kept. Doubles each time the budget
    /// is exceeded, so thinning happens once per doubling rather than on every
    /// insert.
    stride: u64,
    /// Points offered so far, for `stride`: a counter, not a position.
    seen: u64,
}

impl StreamingOctree {
    /// Create an empty streaming octree with a guess for the initial bounds.
    /// The bounds will automatically expand if points fall outside.
    pub fn new(initial_bounds: Bounds) -> Self {
        Self {
            root: OctreeNode {
                bounds: initial_bounds,
                point_indices: Vec::new(),
                children: None,
                depth: 0,
            },
            positions: Vec::new(),
            colors: Vec::new(),
            bounds: initial_bounds,
            allocated_bounds: initial_bounds,
            dirty: false,
            pending: Vec::new(),
            budget: 0,
            stride: 1,
            seen: 0,
        }
    }

    /// Create with default bounds covering a unit cube at origin.
    pub fn empty() -> Self {
        let mut b = Bounds::empty();
        b.extend([-1.0, -1.0, -1.0]);
        b.extend([1.0, 1.0, 1.0]);
        Self::new(b)
    }

    /// Insert a single point. Amortized O(1) for bounded clouds.
    pub fn insert_point(&mut self, position: [f32; 3], color: Option<[f32; 3]>) {
        // Over budget, the tree has already thinned itself and raised `stride`;
        // this is where the thinning takes effect, so the work of reading the
        // rest of a twenty-gigabyte file is skipped rather than done and
        // thrown away.
        self.seen += 1;
        if self.stride > 1 && !self.seen.is_multiple_of(self.stride) {
            return;
        }
        // A point with a non-finite coordinate has nowhere to sit: see
        // `expand_bounds_to_include`. Dropping it here keeps the tree's own
        // arrays finite, which the bounds growth and the LOD walk both rely
        // on. It still counted as offered (`seen`), so a stream's progress and
        // its thinning stride are unaffected.
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }

        let index = self.positions.len();
        self.positions.push(position);
        match color {
            Some(color) => {
                // A cloud that has colours anywhere has them everywhere: the
                // points that arrived before the file's first colour are
                // back-filled, or the array would stay shorter than `positions`
                // and every colour would be dropped as unusable.
                while self.colors.len() < index {
                    self.colors.push(POINT_FALLBACK_COLOR);
                }
                self.colors.push(color);
            }
            None => {
                if !self.colors.is_empty() {
                    // Pad, so the two arrays stay parallel.
                    self.colors.push(POINT_FALLBACK_COLOR);
                }
            }
        }

        // Check if we need to expand bounds.
        if !bounds_contains(&self.allocated_bounds, position) {
            // Double bounds and rebuild.
            self.expand_bounds_to_include(position);
        }

        self.pending.push(index);
        self.dirty = true;

        // Past the budget, thin the whole tree: halving every region equally
        // is what keeps the model's extent and its shape while the point count
        // comes back down.
        if self.budget > 0 && self.positions.len() > self.budget {
            self.decimate();
        }

        // Rebuild if too many pending or a leaf is likely over budget.
        let total = self.positions.len();
        let pending = self.pending.len();
        // Rebuild when pending exceeds ~10% of total or >10000 points.
        if pending > 10000 || (total > 0 && pending * 10 > total) {
            self.rebuild();
        }
    }

    /// Insert many points at once.
    ///
    /// The same rules as [`StreamingOctree::insert_point`] — the stride skips
    /// what earlier thinning rejected, colours stay parallel to positions, the
    /// budget is enforced — but the batch is appended in one pass and the tree
    /// is rebuilt once at the end. Routing a batch through `insert_point`
    /// instead rebuilds every ten thousand points, which makes loading a chunk
    /// cost the size of everything already resident: the index reader hands
    /// over whole chunks, and that is the difference between reading its
    /// regions and re-indexing the cloud for each one.
    pub fn insert_points(&mut self, positions: &[[f32; 3]], colors: &[[f32; 3]]) {
        let has_colors = !colors.is_empty();
        for (index, position) in positions.iter().enumerate() {
            self.seen += 1;
            if self.stride > 1 && !self.seen.is_multiple_of(self.stride) {
                continue;
            }
            // Same rule as `insert_point`: a non-finite coordinate is not a
            // position, and letting one through would spin the bounds growth
            // below without end.
            if !position.iter().all(|value| value.is_finite()) {
                continue;
            }
            let at = self.positions.len();
            self.positions.push(*position);
            if has_colors {
                while self.colors.len() < at {
                    self.colors.push(POINT_FALLBACK_COLOR);
                }
                self.colors
                    .push(colors.get(index).copied().unwrap_or(POINT_FALLBACK_COLOR));
            } else if !self.colors.is_empty() {
                // Pad, so the two arrays stay parallel.
                self.colors.push(POINT_FALLBACK_COLOR);
            }
            if !bounds_contains(&self.allocated_bounds, *position) {
                self.expand_bounds_to_include(*position);
            }
            self.pending.push(at);
        }
        if self.positions.is_empty() {
            return;
        }
        self.dirty = true;
        if self.budget == 0 {
            self.rebuild();
            return;
        }
        // Thinning happens once per doubling, but a batch can arrive several
        // doublings over the budget at once — a whole chunk against a small
        // budget — so keep halving until it fits. One `decimate` per batch
        // would let the tree settle well above its budget until the next
        // batch arrived, which is exactly the memory bound the budget exists
        // to hold. (`decimate` rebuilds, so no extra one is needed.)
        while self.positions.len() > self.budget {
            self.decimate();
        }
    }

    /// Keep every other point and start skipping arrivals, so the cloud fits
    /// its budget again.
    ///
    /// Amortised over a whole stream this is linear: after each thinning the
    /// tree can accept another budget's worth of points before the next one,
    /// so a file of `n` points with budget `b` pays `n / b` passes over at
    /// most `2b` points.
    fn decimate(&mut self) {
        let kept: Vec<[f32; 3]> = self.positions.iter().step_by(2).copied().collect();
        let colors: Vec<[f32; 3]> = if self.colors.len() == self.positions.len() {
            self.colors.iter().step_by(2).copied().collect()
        } else {
            Vec::new()
        };
        self.positions = kept;
        self.colors = colors;
        // Half of what is here now has to last until the next thinning, so
        // take one in two arrivals from here on.
        self.stride = self.stride.saturating_mul(2).max(2);
        self.pending.clear();
        self.dirty = true;
        self.rebuild();
    }

    /// Cap how many points this tree holds; `0` for no limit. Set before the
    /// first insert.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    /// Points kept: the tree's size, which the budget bounds.
    pub fn kept_points(&self) -> usize {
        self.positions.len()
    }

    /// Force a rebuild of the octree from all current positions.
    pub fn rebuild(&mut self) {
        if self.positions.is_empty() {
            return;
        }
        // Compute actual bounds.
        let mut actual = Bounds::empty();
        for p in &self.positions {
            actual.extend(*p);
        }
        self.bounds = actual;

        // Ensure allocated bounds cover actual bounds. `actual` is built with
        // the NaN-ignoring `extend`, so it comes out empty only when there was
        // nothing finite to cover — and there is no box to grow to then, so
        // the loop has to be skipped rather than run without end.
        while !actual.is_empty()
            && (!bounds_contains(&self.allocated_bounds, actual.min)
                || !bounds_contains(&self.allocated_bounds, actual.max))
        {
            self.double_allocated_bounds();
        }

        let indices: Vec<usize> = (0..self.positions.len()).collect();
        self.root = Octree::build_node(self.allocated_bounds, &self.positions, indices, 0);
        self.dirty = false;
        self.pending.clear();
    }

    /// Total points inserted so far.
    pub fn total_points(&self) -> usize {
        self.positions.len()
    }

    /// Current bounds.
    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Whether the tree needs a rebuild before querying.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Query visible points (frustum + LOD + budget). Returns indices into
    /// the positions array. Rebuilds first if dirty.
    pub fn query_frustum(
        &mut self,
        frustum: &Frustum,
        camera_pos: [f32; 3],
        max_points: usize,
    ) -> Vec<usize> {
        if self.dirty {
            self.rebuild();
        }
        let mut result = Vec::with_capacity(max_points.min(self.positions.len()));
        Octree::collect_visible(
            &self.root,
            frustum,
            camera_pos,
            max_points,
            &self.positions,
            &mut result,
        );
        result
    }

    /// Build a point cloud mesh for rendering, thinned evenly to `max_points`.
    pub fn to_mesh_lod(
        &mut self,
        frustum: &Frustum,
        camera_pos: [f32; 3],
        max_points: usize,
    ) -> Mesh {
        if self.dirty {
            self.rebuild();
        }
        // Frustum cull, then spread the point budget over everything visible.
        let mut visible = Vec::new();
        Octree::collect_visible(
            &self.root,
            frustum,
            camera_pos,
            usize::MAX,
            &self.positions,
            &mut visible,
        );
        let indices = spread_sample(visible, max_points);

        let count = indices.len();
        let has_colors = self.colors.len() == self.positions.len();
        let mut positions = Vec::with_capacity(count);
        let mut colors = if has_colors {
            Vec::with_capacity(count)
        } else {
            Vec::new()
        };
        for &idx in &indices {
            positions.push(self.positions[idx]);
            if has_colors {
                colors.push(self.colors[idx]);
            }
        }

        let bounds = bounds_from_points(&positions);
        Mesh {
            positions,
            normals: Vec::new(),
            colors,
            triangles: Vec::new(),
            bounds,
        }
    }

    /// Grow the allocated box until it holds `point`.
    ///
    /// The loop is only sound for a finite point. `bounds_contains` compares
    /// directly, and every comparison with NaN is false, so a NaN would make
    /// `double_allocated_bounds` run forever — the box grows, but the point
    /// can never be inside it. Callers drop non-finite points before getting
    /// here; this guard is what keeps a stray one from hanging the load.
    fn expand_bounds_to_include(&mut self, point: [f32; 3]) {
        if !point.iter().all(|value| value.is_finite()) {
            return;
        }
        while !bounds_contains(&self.allocated_bounds, point) {
            self.double_allocated_bounds();
        }
        self.dirty = true;
    }

    fn double_allocated_bounds(&mut self) {
        let center = bounds_center(&self.allocated_bounds);
        let size = self.allocated_bounds.size();
        self.allocated_bounds = Bounds {
            min: [
                center[0] - size[0],
                center[1] - size[1],
                center[2] - size[2],
            ],
            max: [
                center[0] + size[0],
                center[1] + size[1],
                center[2] + size[2],
            ],
        };
    }
}

/// Check if bounds contain a point (with small epsilon).
fn bounds_contains(bounds: &Bounds, point: [f32; 3]) -> bool {
    point[0] >= bounds.min[0]
        && point[0] <= bounds.max[0]
        && point[1] >= bounds.min[1]
        && point[1] <= bounds.max[1]
        && point[2] >= bounds.min[2]
        && point[2] <= bounds.max[2]
}

// ---------------------------------------------------------------------------
// Free functions.
// ---------------------------------------------------------------------------

fn node_max_depth(node: &OctreeNode) -> u32 {
    if let Some(ref children) = node.children {
        children
            .iter()
            .map(node_max_depth)
            .max()
            .unwrap_or(node.depth)
    } else {
        node.depth
    }
}

fn bounds_center(b: &Bounds) -> [f32; 3] {
    [
        (b.min[0] + b.max[0]) * 0.5,
        (b.min[1] + b.max[1]) * 0.5,
        (b.min[2] + b.max[2]) * 0.5,
    ]
}

#[inline]
/// Thin a visible-point list to `max_points` by taking every `k`-th entry.
///
/// The tree walk hands the list over nearest-leaf first, so a stride through
/// it still reaches the far leaves: the sample covers the model instead of
/// only its near side. Keeping the nearest `max_points` — the obvious reading
/// of a point budget — is what turns a large cloud's preview into one dense
/// patch of it, with no points left for the rest of the silhouette.
fn spread_sample(indices: Vec<usize>, max_points: usize) -> Vec<usize> {
    if max_points == 0 || indices.len() <= max_points {
        return indices;
    }
    let stride = indices.len().div_ceil(max_points).max(1);
    indices.into_iter().step_by(stride).collect()
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

fn bounds_from_points(points: &[[f32; 3]]) -> Bounds {
    let mut b = Bounds::empty();
    for p in points {
        b.extend(*p);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frustum_culls_boxes_outside_it() {
        // A symmetric perspective camera at the origin looking down -Z, with
        // w = -z, a 90° vertical field of view, and depth mapped to 0..=1:
        // the convention `Framing::view_projection` produces for wgpu.
        let (near, far) = (1.0f32, 10.0f32);
        let z_scale = -far / (far - near);
        let z_bias = -far * near / (far - near);
        let m = [
            [1.0f32, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, z_scale, z_bias],
            [0.0, 0.0, -1.0, 0.0],
        ];
        let frustum = Frustum::from_matrix(&m);
        let box_at = |z: f32, extent: f32| Bounds {
            min: [-extent, -extent, z - 0.5],
            max: [extent, extent, z + 0.5],
        };

        // In front of the camera, inside the sides: visible.
        assert!(frustum.intersects_bounds(&box_at(-5.0, 1.0)));
        // Behind the camera: not visible.
        assert!(!frustum.intersects_bounds(&box_at(5.0, 1.0)));
        // Off to the side, outside the 45° half-angle at its depth.
        let side = Bounds {
            min: [20.0, -1.0, -6.0],
            max: [21.0, 1.0, -5.0],
        };
        assert!(!frustum.intersects_bounds(&side));
        // In front of the near plane and beyond the far plane: both culled.
        // A frustum built with the OpenGL `-1..=1` extraction gets these two
        // the wrong way round, which is what pins the depth convention here.
        assert!(!frustum.intersects_bounds(&box_at(-0.5, 0.1)));
        assert!(!frustum.intersects_bounds(&box_at(-20.0, 1.0)));
    }

    #[test]
    fn octree_builds_and_queries() {
        // 10000 random points in a small cube at origin.
        let mut positions = Vec::new();
        for i in 0..10000 {
            let t = i as f32 * 0.0001;
            positions.push([t.fract(), (t * 7.0).fract(), (t * 13.0).fract()]);
        }

        let mesh = Mesh {
            positions: positions.clone(),
            normals: Vec::new(),
            colors: Vec::new(),
            triangles: Vec::new(),
            bounds: Bounds::empty(),
        };

        let octree = Octree::from_point_cloud(&mesh).expect("octree builds");
        assert_eq!(octree.total_points(), 10000);

        // Build a frustum that covers everything: a large box around origin.
        let frustum = Frustum {
            planes: [
                Plane {
                    normal: [1.0, 0.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [-1.0, 0.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, 1.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, -1.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, 0.0, 1.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, 0.0, -1.0],
                    distance: 10.0,
                },
            ],
        };

        let visible = octree.query_frustum(&frustum, [0.0, 0.0, 0.0], usize::MAX);
        assert_eq!(visible.len(), 10000);

        // Query with a small budget.
        let limited = octree.query_frustum(&frustum, [0.0, 0.0, 0.0], 100);
        assert_eq!(limited.len(), 100);

        // to_mesh_lod produces a valid mesh.
        let lod = octree.to_mesh_lod(&frustum, [0.0, 0.0, 0.0], 500);
        assert_eq!(lod.positions.len(), 500);
        assert!(lod.is_point_cloud());
    }

    #[test]
    fn octree_frustum_culling_works() {
        // Points in front of the camera (z < 0).
        let mut positions = Vec::new();
        for i in 0..1000 {
            let t = i as f32 * 0.001;
            positions.push([t.fract() * 2.0 - 1.0, (t * 7.0).fract() * 2.0 - 1.0, -10.0]);
        }

        let mesh = Mesh {
            positions,
            normals: Vec::new(),
            colors: Vec::new(),
            triangles: Vec::new(),
            bounds: Bounds::empty(),
        };

        let octree = Octree::from_point_cloud(&mesh).expect("octree builds");
        assert_eq!(octree.total_points(), 1000);

        // A frustum that only sees points with z < 0 (looking down -Z).
        let front_frustum = Frustum {
            planes: [
                Plane {
                    normal: [0.0, 0.0, -1.0],
                    distance: 0.0,
                }, // near at z=0
                Plane {
                    normal: [0.0, 0.0, 1.0],
                    distance: 100.0,
                }, // far at z=-100
                Plane {
                    normal: [1.0, 0.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [-1.0, 0.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, 1.0, 0.0],
                    distance: 10.0,
                },
                Plane {
                    normal: [0.0, -1.0, 0.0],
                    distance: 10.0,
                },
            ],
        };

        let visible = octree.query_frustum(&front_frustum, [0.0, 0.0, 0.0], usize::MAX);
        assert_eq!(visible.len(), 1000, "points at z=-10 should be visible");

        // Points behind the camera should be culled.
        let mut behind_positions = Vec::new();
        for _ in 0..100 {
            behind_positions.push([0.0, 0.0, 10.0]);
        }
        let behind_mesh = Mesh {
            positions: behind_positions,
            normals: Vec::new(),
            colors: Vec::new(),
            triangles: Vec::new(),
            bounds: Bounds::empty(),
        };
        let behind_octree = Octree::from_point_cloud(&behind_mesh).expect("builds");
        let visible = behind_octree.query_frustum(&front_frustum, [0.0, 0.0, 0.0], usize::MAX);
        assert_eq!(visible.len(), 0, "points at z=10 should be culled");
    }

    /// Child bounds are what frustum culling trusts: a node whose box does
    /// not cover the points it holds makes them vanish as soon as the view is
    /// narrowed — what a mis-built octant looks like from the outside.
    #[test]
    fn every_node_bounds_its_own_points() {
        // Spread over all eight octants (`fract` keeps the sign of a negative
        // number), with enough points that the root splits and the split
        // recurses.
        let mut positions = Vec::new();
        for i in 0..4096 {
            let t = i as f32 * 0.37;
            positions.push([
                (t.sin() * 3.0).fract(),
                (t.cos() * 5.0).fract(),
                (t * 1.7).sin().fract(),
            ]);
        }
        let mesh = Mesh {
            positions,
            normals: Vec::new(),
            colors: Vec::new(),
            triangles: Vec::new(),
            bounds: Bounds::empty(),
        };
        let octree = Octree::from_point_cloud(&mesh).expect("octree builds");

        /// Points held by this node and its descendants.
        fn check(node: &OctreeNode, positions: &[[f32; 3]]) -> usize {
            for &index in &node.point_indices {
                let point = positions[index];
                for axis in 0..3 {
                    assert!(
                        point[axis] >= node.bounds.min[axis] - 1e-6
                            && point[axis] <= node.bounds.max[axis] + 1e-6,
                        "point {point:?} outside node bounds {:?}",
                        node.bounds
                    );
                }
            }
            node.point_indices.len()
                + node.children.as_ref().map_or(0, |children| {
                    children.iter().map(|child| check(child, positions)).sum()
                })
        }

        // Every point is reachable through the tree, and the walk went
        // through interior nodes rather than one flat root.
        assert_eq!(check(&octree.root, &octree.positions), 4096);
        assert!(
            !octree.root.is_leaf(),
            "the cloud must split for this to test anything"
        );
    }

    /// Past its budget the tree thins evenly instead of growing without
    /// limit: a cloud bigger than memory has to lose detail, not the process.
    #[test]
    fn a_budgeted_tree_keeps_its_size_and_its_extent() {
        let mut octree = StreamingOctree::empty();
        octree.set_budget(1_000);
        // Twenty budgets' worth, spread over a wide box so an extent that
        // collapsed would be obvious.
        for index in 0..20_000 {
            let t = index as f32 / 20_000.0;
            octree.insert_point([t * 100.0, -t * 100.0, 0.0], None);
        }
        octree.rebuild();

        let kept = octree.kept_points();
        assert!(kept <= 2_000, "the budget must bound the tree: {kept} kept");
        assert!(kept >= 500, "thinning must not gut the cloud: {kept} kept");

        // Thinning is even, so the whole extent survives: the first and last
        // points of the stream are still represented.
        let bounds = octree.bounds();
        assert!(bounds.min[0] < 20.0, "{bounds:?}");
        assert!(bounds.max[0] > 80.0, "{bounds:?}");
    }

    /// The points the tree gives back after thinning are still a usable cloud:
    /// every kept point is one that was inserted, and the LOD selection still
    /// produces a renderable mesh.
    #[test]
    fn a_budgeted_tree_still_renders() {
        let mut octree = StreamingOctree::empty();
        octree.set_budget(2_000);
        for index in 0..12_000 {
            let t = index as f32 * 0.01;
            octree.insert_point([t.sin(), t.cos(), t * 0.001], None);
        }
        octree.rebuild();
        let mesh = octree.to_mesh_lod(&full_frustum(), [0.0, 0.0, 0.0], 1_000);
        assert!(mesh.vertex_count() > 0 && mesh.vertex_count() <= 1_000);
        for position in &mesh.positions {
            assert!(position[0].is_finite() && position[1].is_finite());
        }
    }

    /// A batch insert produces the same tree as inserting point by point:
    /// the bulk path exists so a reader does not rebuild the cloud every ten
    /// thousand points, not so it gets a different cloud.
    #[test]
    fn a_batch_insert_matches_point_by_point() {
        let positions: Vec<[f32; 3]> = (0..12_000)
            .map(|index| {
                let t = index as f32 * 0.01;
                [t.sin(), t.cos(), t * 0.001]
            })
            .collect();
        let colors: Vec<[f32; 3]> = (0..12_000)
            .map(|index| [index as f32 / 12_000.0, 0.5, 0.25])
            .collect();

        let mut one = StreamingOctree::empty();
        for (point, color) in positions.iter().zip(&colors) {
            one.insert_point(*point, Some(*color));
        }
        let mut batch = StreamingOctree::empty();
        batch.insert_points(&positions, &colors);

        assert_eq!(one.kept_points(), batch.kept_points());
        let frustum = full_frustum();
        let a = one.to_mesh_lod(&frustum, [0.0, 0.0, 0.0], 12_000);
        let b = batch.to_mesh_lod(&frustum, [0.0, 0.0, 0.0], 12_000);
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.colors, b.colors);
    }

    /// A non-finite point must not be inserted. `expand_bounds_to_include`
    /// loops until its box holds the point, and a NaN is never inside any box
    /// — every comparison against it is false — so one would spin the loader
    /// forever at 100% CPU. It must not poison the bounds either, because the
    /// LOD walk and the camera framing both read them.
    #[test]
    fn non_finite_points_do_not_hang_or_poison_the_tree() {
        let mut octree = StreamingOctree::empty();
        octree.insert_points(
            &[
                [0.0, 0.0, 0.0],
                [f32::NAN, 1.0, 2.0],
                [1.0, 1.0, 1.0],
                [f32::INFINITY, 0.0, 0.0],
                [f32::NEG_INFINITY, 0.0, 0.0],
            ],
            &[],
        );
        octree.rebuild();
        assert_eq!(octree.kept_points(), 2, "only the finite points are kept");
        let bounds = octree.bounds();
        assert!(bounds.min.iter().all(|value| value.is_finite()));
        assert!(bounds.max.iter().all(|value| value.is_finite()));

        // The single-point path takes the same guard, and a tree that only
        // ever saw NaN still answers a query instead of looping.
        let mut one = StreamingOctree::empty();
        one.insert_point([f32::NAN; 3], None);
        one.insert_point([2.0, 2.0, 2.0], None);
        one.rebuild();
        assert_eq!(one.kept_points(), 1);
        let bounds = one.bounds();
        assert!(bounds.min.iter().all(|value| value.is_finite()));
    }

    /// A point budget thins the cloud evenly. Keeping the nearest points
    /// instead leaves a whole-model view showing one patch of the model, with
    /// no points at all for its far side.
    #[test]
    fn a_point_budget_still_covers_the_whole_cloud() {
        let mut octree = StreamingOctree::empty();
        for index in 0..20_000 {
            let t = index as f32 / 20_000.0;
            octree.insert_point([t * 100.0, t * 100.0, 0.0], None);
        }
        // The camera sits past one end, so "nearest" and "everywhere" are
        // different sets.
        let mesh = octree.to_mesh_lod(&full_frustum(), [-100.0, -100.0, 0.0], 500);
        assert!(mesh.vertex_count() > 0 && mesh.vertex_count() <= 500);
        let bounds = mesh.bounds;
        assert!(bounds.min[0] < 10.0, "the near end is missing: {bounds:?}");
        assert!(bounds.max[0] > 90.0, "the far end is missing: {bounds:?}");
    }

    /// A frustum that sees everything, for the cloud tests.
    fn full_frustum() -> Frustum {
        let plane = |normal: [f32; 3]| Plane {
            normal,
            distance: 1e6,
        };
        Frustum {
            planes: [
                plane([1.0, 0.0, 0.0]),
                plane([-1.0, 0.0, 0.0]),
                plane([0.0, 1.0, 0.0]),
                plane([0.0, -1.0, 0.0]),
                plane([0.0, 0.0, 1.0]),
                plane([0.0, 0.0, -1.0]),
            ],
        }
    }

    /// A coloured cloud whose first points arrive before its first colour has
    /// to keep those colours: the array is back-filled, or it would stay
    /// shorter than `positions` and every colour would be dropped.
    #[test]
    fn colours_arriving_late_are_back_filled() {
        let mut octree = StreamingOctree::empty();
        octree.insert_point([0.0, 0.0, 0.0], None);
        octree.insert_point([1.0, 0.0, 0.0], Some([1.0, 0.5, 0.25]));
        octree.rebuild();

        let mesh = octree.to_mesh_lod(&full_frustum(), [0.0, 0.0, 0.0], 16);
        assert!(
            mesh.has_vertex_colors(),
            "the late colour must not be dropped: {:?}",
            mesh.colors
        );
        assert_eq!(mesh.colors.len(), 2);
        // The point that carried no colour takes the renderer's own neutral.
        assert_eq!(mesh.colors[0], crate::media::render3d::MATERIAL);
        assert_eq!(mesh.colors[1], [1.0, 0.5, 0.25]);
    }
}
