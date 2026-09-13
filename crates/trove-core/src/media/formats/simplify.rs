//! Quadric Error Metrics (QEM) mesh simplification.
//!
//! Reduces triangle count while preserving visual quality. Produces multiple
//! LOD levels from a single high-poly mesh, each one carrying the surface it
//! came from — its triangles as well as its vertices — and the geometric
//! error at which it was produced. At render time the viewport picks the
//! coarsest level whose error stays under a screen-space threshold.
//!
//! Reference: Garland & Heckbert, "Surface Simplification Using Quadric
//! Error Metrics" (1997).

use crate::media::formats::types::{Bounds, Mesh};
use std::collections::BinaryHeap;

/// Target triangle counts for each LOD level, as a fraction of the original.
/// Levels cascade — each is produced by collapsing the previous one further —
/// so `error` grows monotonically down the list and the levels cost less
/// memory than four independent simplifications would.
const LOD_LEVELS: [f64; 4] = [1.0, 0.5, 0.25, 0.125];

/// Screen-space error threshold (in pixels) for LOD selection: a level whose
/// worst deviation projects to less than this is indistinguishable from the
/// original at the size it is drawn.
pub const PIXEL_ERROR_THRESHOLD: f64 = 2.0;

/// One level of detail: the vertices, the triangles that connect them, and
/// how far the level has moved from the original surface.
#[derive(Debug, Clone)]
pub struct SimplifiedLevel {
    pub vertices: Vec<MeshVertex>,
    /// Triangles, indexed into `vertices`. A level built from a triangle mesh
    /// always has at least one; a level with none would be drawn as a point
    /// cloud by both renderers.
    pub triangles: Vec<[u32; 3]>,
    /// Geometric deviation from the original, in model units: the square root
    /// of the largest quadric error reached while producing this level.
    pub error: f64,
}

/// A simplified mesh with one or more LOD levels.
#[derive(Debug, Clone)]
pub struct SimplifiedMesh {
    /// Bounds of the original mesh — the camera is framed against these, not
    /// against whichever level is currently drawn, so switching level cannot
    /// change the framing and feed back into the selection.
    pub bounds: Bounds,
    /// Whether the source mesh carried usable vertex normals. When it did
    /// not, the levels carry none either, so flat shading survives the swap;
    /// inventing normals here would silently smooth a faceted model.
    pub has_normals: bool,
    /// LOD levels, from finest (index 0) to coarsest.
    pub levels: Vec<SimplifiedLevel>,
}

/// Per-vertex data for the simplified mesh representation.
#[derive(Debug, Clone)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Quadric error matrix (10 unique symmetric 4×4 entries).
    pub quadric: [f64; 10],
}

/// An edge collapse candidate, ordered by error (min-heap).
///
/// The versions record how fresh the candidate is: a vertex whose quadric or
/// position has changed since the candidate was queued makes it stale, and it
/// is re-queued with the current cost rather than being popped on an error
/// that no longer applies.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Collapse {
    error: f64,
    remove: usize,
    target: usize,
    remove_version: u32,
    target_version: u32,
}

impl Eq for Collapse {}

impl PartialOrd for Collapse {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Reversed, so `BinaryHeap` — a max-heap — pops the cheapest collapse first.
impl Ord for Collapse {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .error
            .partial_cmp(&self.error)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Simplify a mesh into its LOD levels using QEM.
///
/// A mesh with no triangles is left alone: there is nothing to collapse, and
/// the caller keeps drawing it as the point cloud it is.
pub fn simplify_mesh(mesh: &Mesh) -> SimplifiedMesh {
    if mesh.triangles.is_empty() {
        return SimplifiedMesh {
            bounds: mesh.bounds,
            has_normals: false,
            levels: Vec::new(),
        };
    }

    let has_normals = mesh.has_vertex_normals();
    let mut vertices: Vec<MeshVertex> = mesh
        .positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| MeshVertex {
            position: pos,
            // Unused when the source carries none; kept so the struct stays
            // uniform and the average below is always well defined.
            normal: if has_normals {
                mesh.normals[i]
            } else {
                [0.0, 1.0, 0.0]
            },
            quadric: [0.0; 10],
        })
        .collect();

    for tri in &mesh.triangles {
        let v0 = vertices[tri[0] as usize].position;
        let v1 = vertices[tri[1] as usize].position;
        let v2 = vertices[tri[2] as usize].position;

        let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
        let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
        let n = cross(e1, e2);
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-9);
        let n = [n[0] / len, n[1] / len, n[2] / len];
        let d = -(n[0] * v0[0] + n[1] * v0[1] + n[2] * v0[2]);
        let plane = [n[0] as f64, n[1] as f64, n[2] as f64, d as f64];
        let q = outer_product(plane);

        for &vi in tri {
            add_quadric(&mut vertices[vi as usize].quadric, &q);
        }
    }

    let original_count = mesh.triangles.len();
    let mut levels = Vec::with_capacity(LOD_LEVELS.len());
    // The state each iteration collapses further; levels are snapshots of it.
    let mut current_vertices = vertices;
    let mut current_triangles = mesh.triangles.clone();
    let mut error = 0.0f64;

    for &ratio in &LOD_LEVELS {
        let target = ((original_count as f64) * ratio).max(1.0) as usize;
        if current_triangles.len() > target {
            let collapsed = collapse_edges(&current_vertices, &current_triangles, target);
            current_vertices = collapsed.vertices;
            current_triangles = collapsed.triangles;
            // The level's error is the worst collapse it took to get here,
            // carried forward so the coarser levels stay ordered.
            error = error.max(collapsed.error);
        }
        levels.push(SimplifiedLevel {
            vertices: current_vertices.clone(),
            triangles: current_triangles.clone(),
            error,
        });
    }

    SimplifiedMesh {
        bounds: mesh.bounds,
        has_normals,
        levels,
    }
}

/// The result of collapsing a mesh down to a triangle budget.
struct Collapsed {
    vertices: Vec<MeshVertex>,
    triangles: Vec<[u32; 3]>,
    /// The largest quadric error among the accepted collapses.
    error: f64,
}

/// Collapse edges until the triangle count reaches `target_triangles`.
///
/// Each accepted collapse merges the removed vertex into its neighbour: the
/// quadrics are summed, the merged vertex moves to the position that
/// minimises the merged quadric, and every triangle incident to the removed
/// vertex is rewired. Triangles that would double up a corner, and collapses
/// that would flip a face, are rejected — the first are dropped, the second
/// leave the candidate out of the heap entirely.
fn collapse_edges(
    vertices: &[MeshVertex],
    triangles: &[[u32; 3]],
    target_triangles: usize,
) -> Collapsed {
    let mut verts = vertices.to_vec();
    let mut tris = triangles.to_vec();
    let mut alive = vec![true; verts.len()];
    let mut tri_alive = vec![true; tris.len()];
    // Bumped whenever a vertex's quadric or position changes; a candidate
    // queued before that no longer reflects the surface it would collapse.
    let mut version = vec![0u32; verts.len()];
    // vertex → indices of the triangles that use it, kept live as collapses
    // are applied so a merge only touches the triangles that can be affected.
    let mut incident: Vec<Vec<usize>> = vec![Vec::new(); verts.len()];
    for (index, tri) in tris.iter().enumerate() {
        for &v in tri {
            incident[v as usize].push(index);
        }
    }

    let mut heap: BinaryHeap<Collapse> = BinaryHeap::new();
    // Every edge of every triangle, both ways: either endpoint may be the one
    // that disappears.
    for tri in &tris {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            queue(&mut heap, &verts, &version, a as usize, b as usize);
            queue(&mut heap, &verts, &version, b as usize, a as usize);
        }
    }

    let mut live_triangles = tris.len();
    let mut error = 0.0f64;

    while live_triangles > target_triangles {
        let Some(candidate) = heap.pop() else {
            break; // Nothing left worth collapsing.
        };
        let (remove, target) = (candidate.remove, candidate.target);
        if !alive[remove] || !alive[target] || remove == target {
            continue;
        }
        if version[remove] != candidate.remove_version
            || version[target] != candidate.target_version
        {
            // Stale cost: re-queue it against the surface as it is now.
            queue(&mut heap, &verts, &version, target, remove);
            continue;
        }

        let (cost, position) = collapse_cost(&verts[target], &verts[remove]);
        if !collapse_is_valid(&verts, &tris, &incident, remove, target, position) {
            continue; // Would fold the surface over itself; drop the pair.
        }

        // Commit: the merged quadric is what the position was solved for.
        verts[target].quadric = add_quadrics(&verts[target].quadric, &verts[remove].quadric);
        verts[target].position = position;
        verts[target].normal = normalize3(add3(verts[target].normal, verts[remove].normal));

        // Rewire the removed vertex's triangles onto the target. A triangle
        // that now repeats a corner has collapsed to an edge: it is dropped.
        // The list is taken rather than borrowed: the triangles are handed to
        // the target's list as they are walked.
        for index in std::mem::take(&mut incident[remove]) {
            if !tri_alive[index] {
                continue;
            }
            let tri = &mut tris[index];
            for corner in tri.iter_mut() {
                if *corner as usize == remove {
                    *corner = target as u32;
                }
            }
            if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
                tri_alive[index] = false;
                live_triangles -= 1;
            } else {
                incident[target].push(index);
            }
        }
        alive[remove] = false;
        version[remove] += 1;
        error = error.max(cost);

        // The target's quadric and position moved, and so did the surface
        // around every one of its neighbours: re-queue all of them. The
        // versions are bumped first, or the candidates just queued would look
        // stale the moment they were popped.
        version[target] += 1;
        let touched: Vec<usize> = incident[target]
            .iter()
            .filter(|&&index| tri_alive[index])
            .flat_map(|&index| tris[index])
            .map(|v| v as usize)
            .collect();
        for vertex in touched {
            if !alive[vertex] || vertex == target {
                continue;
            }
            version[vertex] += 1;
            queue(&mut heap, &verts, &version, target, vertex);
            queue(&mut heap, &verts, &version, vertex, target);
        }
    }

    compact(verts, tris, alive, tri_alive, error)
}

/// Drop what the collapses removed and renumber what is left, so the level's
/// arrays are dense — the renderer indexes them directly.
fn compact(
    verts: Vec<MeshVertex>,
    tris: Vec<[u32; 3]>,
    alive: Vec<bool>,
    tri_alive: Vec<bool>,
    error: f64,
) -> Collapsed {
    let mut remap = vec![u32::MAX; verts.len()];
    let mut kept = Vec::with_capacity(verts.len());
    for (index, vertex) in verts.into_iter().enumerate() {
        if alive[index] {
            remap[index] = kept.len() as u32;
            kept.push(vertex);
        }
    }
    let triangles: Vec<[u32; 3]> = tris
        .into_iter()
        .zip(tri_alive)
        .filter(|(tri, live)| {
            // A live triangle only ever references live vertices, but one
            // left pointing at a removed vertex would index a slot that no
            // longer exists — drop it rather than remap `u32::MAX` into the
            // buffer.
            *live && tri.iter().all(|&v| remap[v as usize] != u32::MAX)
        })
        .map(|(tri, _)| tri.map(|v| remap[v as usize]))
        .collect();
    Collapsed {
        vertices: kept,
        triangles,
        error,
    }
}

/// Queue one collapse candidate with the cost the surface has right now.
fn queue(
    heap: &mut BinaryHeap<Collapse>,
    verts: &[MeshVertex],
    version: &[u32],
    target: usize,
    remove: usize,
) {
    if target == remove || target >= verts.len() || remove >= verts.len() {
        return;
    }
    let (error, _) = collapse_cost(&verts[target], &verts[remove]);
    heap.push(Collapse {
        error,
        remove,
        target,
        remove_version: version[remove],
        target_version: version[target],
    });
}

/// The cost of merging two vertices and the position to merge them at: the
/// quadric error of their summed quadric, minimised over space.
fn collapse_cost(target: &MeshVertex, remove: &MeshVertex) -> (f64, [f32; 3]) {
    let merged = add_quadrics(&target.quadric, &remove.quadric);
    let position = optimal_position(&merged, target.position, remove.position);
    let point = [position[0], position[1], position[2], 1.0];
    (
        quadric_error(&merged, point),
        [position[0] as f32, position[1] as f32, position[2] as f32],
    )
}

/// Whether merging `remove` into `target` at `position` would fold the
/// surface over itself.
///
/// Every triangle that touches either vertex is checked for a normal that
/// reverses. Without this a greedy collapse happily turns a sphere inside out
/// at the places where the quadric is flat in both directions.
fn collapse_is_valid(
    verts: &[MeshVertex],
    tris: &[[u32; 3]],
    incident: &[Vec<usize>],
    remove: usize,
    target: usize,
    position: [f32; 3],
) -> bool {
    for &index in incident[remove].iter().chain(&incident[target]) {
        let tri = tris[index];
        let old = face_normal(&tri.map(|v| verts[v as usize].position));
        // The three corners as they will be after the merge; a repeated
        // corner means the triangle degenerates, which is allowed (it is
        // dropped) and has no normal to compare.
        let mut moved = [[0.0f32; 3]; 3];
        for slot in 0..3 {
            let vertex = tri[slot] as usize;
            moved[slot] = if vertex == remove || vertex == target {
                position
            } else {
                verts[vertex].position
            };
        }
        if moved[0] == moved[1] || moved[1] == moved[2] || moved[0] == moved[2] {
            continue;
        }
        let new = face_normal(&moved);
        if dot3(old, new) <= 0.0 {
            return false;
        }
    }
    true
}

fn face_normal(corners: &[[f32; 3]; 3]) -> [f32; 3] {
    cross(sub3(corners[1], corners[0]), sub3(corners[2], corners[0]))
}

/// Select the LOD level to draw for a given camera distance and viewport.
///
/// `vertical_fov_deg` has to be the field of view the framing uses, so the
/// model-units-to-pixels conversion matches the picture actually drawn.
/// Returns the coarsest level whose deviation projects to less than
/// [`PIXEL_ERROR_THRESHOLD`] pixels, or the finest level when even that is
/// too coarse to guarantee it.
pub fn select_lod(
    simplified: &SimplifiedMesh,
    camera_distance: f64,
    viewport_height: f32,
    vertical_fov_deg: f32,
) -> Option<usize> {
    if simplified.levels.is_empty() {
        return None;
    }
    // World height spanned by the viewport at the model's distance, so a
    // model-space error turns into the pixels it covers on screen.
    let world_height =
        2.0 * camera_distance.max(1e-6) * (vertical_fov_deg as f64 * 0.5).to_radians().tan();
    let pixels_per_unit = viewport_height as f64 / world_height.max(1e-9);

    for (index, level) in simplified.levels.iter().enumerate().rev() {
        if level.error * pixels_per_unit <= PIXEL_ERROR_THRESHOLD {
            return Some(index);
        }
    }
    Some(0)
}

fn outer_product(p: [f64; 4]) -> [f64; 10] {
    let mut q = [0.0; 10];
    let idx = |r: usize, c: usize| -> usize {
        let (r, c) = if r <= c { (r, c) } else { (c, r) };
        c * (c + 1) / 2 + r
    };
    for r in 0..4 {
        for c in r..4 {
            q[idx(r, c)] = p[r] * p[c];
        }
    }
    q
}

/// The sum of two quadrics: what the error of merging their vertices uses.
fn add_quadrics(a: &[f64; 10], b: &[f64; 10]) -> [f64; 10] {
    std::array::from_fn(|i| a[i] + b[i])
}

fn add_quadric(dst: &mut [f64; 10], src: &[f64; 10]) {
    for i in 0..10 {
        dst[i] += src[i];
    }
}

fn quadric_error(q: &[f64; 10], v: [f64; 4]) -> f64 {
    let mut err = 0.0;
    let idx = |r: usize, c: usize| -> usize {
        let (r, c) = if r <= c { (r, c) } else { (c, r) };
        c * (c + 1) / 2 + r
    };
    for r in 0..4 {
        for c in r..4 {
            let factor = if r == c { 1.0 } else { 2.0 };
            err += factor * q[idx(r, c)] * v[r] * v[c];
        }
    }
    err
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Unit-length copy of `a`, or `a` unchanged when it is too short to scale.
fn normalize3(a: [f32; 3]) -> [f32; 3] {
    let length = dot3(a, a).sqrt();
    if length > 1e-12 {
        [a[0] / length, a[1] / length, a[2] / length]
    } else {
        a
    }
}

/// The position that minimises the quadric error of the merged quadric, or
/// the midpoint when the system is singular — the midpoint keeps the vertex
/// on the surface's side instead of snapping it to one endpoint.
fn optimal_position(q: &[f64; 10], p1: [f32; 3], p2: [f32; 3]) -> [f64; 4] {
    let a = [[q[0], q[1], q[2]], [q[1], q[3], q[4]], [q[2], q[4], q[5]]];
    let b = [-q[6], -q[7], -q[8]];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-9 {
        return [
            (p1[0] as f64 + p2[0] as f64) * 0.5,
            (p1[1] as f64 + p2[1] as f64) * 0.5,
            (p1[2] as f64 + p2[2] as f64) * 0.5,
            1.0,
        ];
    }
    let inv_det = 1.0 / det;
    let x = inv_det
        * (b[0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
            - a[0][1] * (b[1] * a[2][2] - a[1][2] * b[2])
            + a[0][2] * (b[1] * a[2][1] - a[1][1] * b[2]));
    let y = inv_det
        * (a[0][0] * (b[1] * a[2][2] - a[1][2] * b[2])
            - b[0] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * b[2] - b[1] * a[2][0]));
    let z = inv_det
        * (a[0][0] * (a[1][1] * b[2] - b[1] * a[2][1])
            - a[0][1] * (a[1][0] * b[2] - b[1] * a[2][0])
            + b[0] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]));
    [x, y, z, 1.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad_mesh() -> Mesh {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let triangles = vec![[0, 1, 2], [0, 2, 3]];
        Mesh::from_parts(positions, vec![[0.0, 0.0, 1.0]; 4], Vec::new(), triangles)
            .expect("quad builds")
    }

    /// A subdivided grid, so there is something to collapse without folding
    /// the surface inside out.
    fn grid_mesh(steps: usize) -> Mesh {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut triangles = Vec::new();
        for y in 0..=steps {
            for x in 0..=steps {
                positions.push([x as f32 / steps as f32, y as f32 / steps as f32, 0.0]);
                normals.push([0.0, 0.0, 1.0]);
            }
        }
        let row = (steps + 1) as u32;
        for y in 0..steps as u32 {
            for x in 0..steps as u32 {
                let i = y * row + x;
                triangles.push([i, i + 1, i + row + 1]);
                triangles.push([i, i + row + 1, i + row]);
            }
        }
        Mesh::from_parts(positions, normals, Vec::new(), triangles).expect("grid builds")
    }

    /// The whole point of keeping triangles per level: a caller that only got
    /// vertices back would have to draw the level as a point cloud.
    #[test]
    fn every_level_carries_its_topology() {
        let simplified = simplify_mesh(&grid_mesh(8));
        assert_eq!(simplified.levels.len(), LOD_LEVELS.len());
        for (index, level) in simplified.levels.iter().enumerate() {
            assert!(
                !level.triangles.is_empty(),
                "level {index} has no triangles"
            );
            // Every triangle indexes a vertex that exists.
            for tri in &level.triangles {
                for &vertex in tri {
                    assert!((vertex as usize) < level.vertices.len());
                }
            }
            // And no triangle degenerated into a repeated corner.
            for tri in &level.triangles {
                assert!(tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2]);
            }
        }
    }

    /// Triangle counts have to actually fall, or the levels are decoration.
    #[test]
    fn coarser_levels_have_fewer_triangles() {
        let simplified = simplify_mesh(&grid_mesh(16));
        let counts: Vec<usize> = simplified
            .levels
            .iter()
            .map(|level| level.triangles.len())
            .collect();
        assert!(counts[0] > counts[1], "{counts:?}");
        assert!(counts[1] >= counts[2], "{counts:?}");
        assert!(counts[2] >= counts[3], "{counts:?}");
        assert!(counts[1] <= counts[0] / 2 + 8, "halving: {counts:?}");
    }

    /// The error reported for a level has to grow as the level gets coarser,
    /// or `select_lod` cannot order them.
    #[test]
    fn error_grows_with_coarseness() {
        let simplified = simplify_mesh(&grid_mesh(16));
        for pair in simplified.levels.windows(2) {
            assert!(
                pair[1].error >= pair[0].error,
                "{} then {}",
                pair[0].error,
                pair[1].error
            );
        }
        assert_eq!(simplified.levels[0].error, 0.0);
    }

    /// A flat grid can be coarsened to almost nothing without moving off the
    /// plane, so the coarsest level has to be selectable.
    #[test]
    fn a_flat_surface_selects_the_coarsest_level() {
        let simplified = simplify_mesh(&grid_mesh(16));
        // A viewport-sized model: 800 px across, at the default framing.
        let lod = select_lod(&simplified, 2.18, 800.0, 35.0).expect("a level");
        assert_eq!(
            lod,
            simplified.levels.len() - 1,
            "flat geometry is free to coarsen"
        );
    }

    /// Far away everything is sub-pixel, so the coarsest level is right.
    #[test]
    fn a_distant_model_is_drawn_coarse() {
        let simplified = simplify_mesh(&grid_mesh(16));
        let far = select_lod(&simplified, 1000.0, 400.0, 35.0).expect("a level");
        assert_eq!(far, simplified.levels.len() - 1);
    }

    /// A level that would show is not chosen: the finest level wins when the
    /// error would be visible.
    #[test]
    fn a_visible_error_keeps_the_finest_level() {
        // A sphere-like surface has real error under collapse.
        let simplified = simplify_mesh(&icosphere());
        let near = select_lod(&simplified, 0.5, 2000.0, 35.0).expect("a level");
        assert_eq!(near, 0, "an error of many pixels must not be accepted");
    }

    /// A coarse triangle soup that keeps its shape: a subdivided icosahedron.
    fn icosphere() -> Mesh {
        // A ring of points with a pole, subdivided in rings — enough
        // curvature that collapsing moves vertices off the surface.
        let mut positions = Vec::new();
        let mut triangles = Vec::new();
        let rings = 24;
        let around = 48;
        for ring in 0..=rings {
            let phi = std::f32::consts::PI * ring as f32 / rings as f32;
            for step in 0..around {
                let theta = std::f32::consts::TAU * step as f32 / around as f32;
                positions.push([phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()]);
            }
        }
        for ring in 0..rings as u32 {
            for step in 0..around as u32 {
                let next = (step + 1) % around as u32;
                let a = ring * around as u32 + step;
                let b = ring * around as u32 + next;
                let c = (ring + 1) * around as u32 + step;
                let d = (ring + 1) * around as u32 + next;
                triangles.push([a, b, d]);
                triangles.push([a, d, c]);
            }
        }
        Mesh::from_parts(positions, Vec::new(), Vec::new(), triangles).expect("sphere builds")
    }

    /// The collapse position is solved from the summed quadric, so a flat
    /// surface's vertices stay on it instead of drifting.
    #[test]
    fn collapsing_a_flat_grid_keeps_it_flat() {
        let mesh = grid_mesh(4);
        let simplified = simplify_mesh(&mesh);
        let coarsest = simplified.levels.last().expect("a level");
        for vertex in &coarsest.vertices {
            assert!(
                vertex.position[2].abs() < 1e-6,
                "vertex left the plane: {:?}",
                vertex.position
            );
        }
    }

    /// A mesh with no normals must not gain them: the level would then be
    /// shaded smooth where the original was faceted.
    #[test]
    fn levels_do_not_invent_normals() {
        let flat = Mesh::from_parts(
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            Vec::new(),
            Vec::new(),
            vec![[0, 1, 2]],
        )
        .expect("triangle builds");
        assert!(!flat.has_vertex_normals());
        let simplified = simplify_mesh(&flat);
        assert!(!simplified.has_normals);

        let smooth = quad_mesh();
        assert!(smooth.has_vertex_normals());
        assert!(simplify_mesh(&smooth).has_normals);
    }

    /// A point cloud has nothing to collapse, and must not be turned into a
    /// triangle mesh by a zero-size level.
    #[test]
    fn a_point_cloud_gets_no_levels() {
        let cloud = Mesh::from_parts(
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("cloud builds");
        assert!(cloud.is_point_cloud());
        let simplified = simplify_mesh(&cloud);
        assert!(simplified.levels.is_empty());
        assert!(select_lod(&simplified, 1.0, 100.0, 35.0).is_none());
    }

    #[test]
    fn quadric_error_at_self_is_zero() {
        let q = [0.0; 10];
        let err = quadric_error(&q, [1.0, 2.0, 3.0, 1.0]);
        assert!(err.abs() < 1e-9);
    }

    /// The error of a plane's own quadric is zero anywhere on the plane and
    /// the squared distance off it, which is what makes it a usable metric.
    #[test]
    fn quadric_error_measures_distance_to_the_plane() {
        let plane = [0.0, 0.0, 1.0, 0.0]; // the z = 0 plane
        let q = outer_product(plane);
        assert!(quadric_error(&q, [3.0, 4.0, 0.0, 1.0]).abs() < 1e-9);
        let off = quadric_error(&q, [0.0, 0.0, 2.0, 1.0]);
        assert!((off - 4.0).abs() < 1e-9, "{off}");
    }
}
