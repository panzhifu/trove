//! Meshlet partition: a triangle mesh cut into spatially compact clusters.
//!
//! A large mesh is drawn in full every frame even when most of it is off
//! screen: back-face culling removes the triangles facing away, but not the
//! ones behind the camera or beside it. Cutting the mesh into clusters, each
//! with one bounding box, lets the renderer test a few thousand boxes against
//! the frustum and skip every triangle inside the boxes it cannot see.
//!
//! Clustering is a Morton sort of the triangles' centroids: neighbouring
//! clusters cover neighbouring space, which is what makes the boxes tight
//! enough to be worth testing, and the sorted order is what makes each
//! cluster a *contiguous* run of triangles the renderer can draw in one call.
//!
//! Only the indexed layout (a mesh with vertex normals) is partitioned. The
//! flat-shaded layout expands every triangle into its own three vertices, so a
//! per-cluster draw there would be a draw per cluster over data that has no
//! indices to skip — more bookkeeping than triangles saved.

use super::types::{Bounds, Mesh};
use crate::media::index::order::{Grid, MAX_BITS, morton_code};

/// Triangles a meshlet holds at most.
///
/// A few thousand triangles is a box tight enough to cull with and few enough
/// clusters that testing them all per frame is trivial; smaller clusters make
/// the boxes tighter but the draw list longer.
pub const DEFAULT_MESHLET_TRIANGLES: usize = 4096;

/// Below this many triangles a mesh is drawn whole.
///
/// The visibility pass would cost more than the triangles it skips, and a
/// model this small already turns.
pub const MIN_MESHLET_TRIANGLES: usize = 8 * 1024;

/// One cluster of triangles and the box they occupy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meshlet {
    /// Box around the cluster's triangles, in model space.
    pub bounds: Bounds,
    /// First triangle of the cluster, in the reordered triangle list.
    pub first: u32,
    /// Triangles in the cluster.
    pub count: u32,
}

impl Meshlet {
    /// One past the last triangle.
    pub fn end(&self) -> u32 {
        self.first + self.count
    }

    /// Index-buffer range to draw, as `[start, end)`.
    pub fn index_range(&self) -> std::ops::Range<u32> {
        self.first * 3..self.end() * 3
    }
}

/// A partitioned mesh: its clusters, and the triangle order they imply.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshletSet {
    pub meshlets: Vec<Meshlet>,
    /// A permutation of the mesh's triangles: `order[i]` is the original
    /// triangle that has to sit at position `i` for the clusters to be
    /// contiguous. The renderer applies it to its index buffer.
    pub order: Vec<u32>,
}

/// Partition `mesh` into meshlets, or `None` when it is not worth it.
///
/// `None` for a point cloud, for a mesh under [`MIN_MESHLET_TRIANGLES`], and
/// for the flat-shaded layout — see the module docs.
pub fn partition(mesh: &Mesh, max_triangles: usize) -> Option<MeshletSet> {
    if !mesh.has_vertex_normals() || mesh.triangles.len() < MIN_MESHLET_TRIANGLES {
        return None;
    }
    let max_triangles = max_triangles.max(1);

    // The triangle order, by the Morton code of each triangle's centroid.
    let grid = Grid::covering(mesh.bounds, MAX_BITS);
    let mut keyed: Vec<(u64, u32)> = mesh
        .triangles
        .iter()
        .enumerate()
        .map(|(index, triangle)| {
            (
                morton_code(grid.quantise(centroid(mesh, *triangle))),
                index as u32,
            )
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    let order: Vec<u32> = keyed.into_iter().map(|(_, triangle)| triangle).collect();

    let mut meshlets = Vec::with_capacity(order.len().div_ceil(max_triangles));
    for (chunk, triangles) in order.chunks(max_triangles).enumerate() {
        let mut bounds = Bounds::empty();
        for &triangle in triangles {
            for corner in mesh.triangles[triangle as usize] {
                bounds.extend(mesh.positions[corner as usize]);
            }
        }
        meshlets.push(Meshlet {
            bounds,
            first: (chunk * max_triangles) as u32,
            count: triangles.len() as u32,
        });
    }
    Some(MeshletSet { meshlets, order })
}

/// The centre of a triangle: what is clustered, so a long thin triangle lands
/// in the cluster its bulk is in rather than the one its first corner is in.
fn centroid(mesh: &Mesh, triangle: [u32; 3]) -> [f32; 3] {
    let corner = |index: u32| mesh.positions[index as usize];
    let (a, b, c) = (
        corner(triangle[0]),
        corner(triangle[1]),
        corner(triangle[2]),
    );
    [
        (a[0] + b[0] + c[0]) / 3.0,
        (a[1] + b[1] + c[1]) / 3.0,
        (a[2] + b[2] + c[2]) / 3.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A triangulated plane of `n`×`n` quads, with normals: the indexed layout
    /// the partitioner is for.
    fn grid(n: usize) -> Mesh {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                positions.push([x as f32, 0.0, z as f32]);
                normals.push([0.0, 1.0, 0.0]);
            }
        }
        let mut triangles = Vec::new();
        let row = (n + 1) as u32;
        for z in 0..n as u32 {
            for x in 0..n as u32 {
                let corner = z * row + x;
                triangles.push([corner, corner + 1, corner + row]);
                triangles.push([corner + 1, corner + row + 1, corner + row]);
            }
        }
        Mesh::from_parts(positions, normals, Vec::new(), triangles).expect("grid builds")
    }

    fn cloud(n: usize) -> Mesh {
        let positions = (0..n).map(|i| [i as f32, 0.0, 0.0]).collect();
        Mesh::from_parts(positions, Vec::new(), Vec::new(), Vec::new()).expect("cloud builds")
    }

    /// Every triangle lands in exactly one cluster, and the order the renderer
    /// is handed is a permutation — a duplicate or a dropped triangle here
    /// would show as a hole or a wrong face on screen.
    #[test]
    fn a_partition_covers_every_triangle_once() {
        let mesh = grid(80); // 12 800 triangles
        let set = partition(&mesh, DEFAULT_MESHLET_TRIANGLES).expect("large mesh partitions");

        let mut seen = set.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..mesh.triangles.len() as u32).collect::<Vec<_>>());

        assert_eq!(
            set.meshlets.iter().map(|m| m.count as usize).sum::<usize>(),
            mesh.triangles.len()
        );
        assert!(
            set.meshlets.len() > 1,
            "the test needs more than one cluster"
        );
        for meshlet in &set.meshlets {
            assert!(meshlet.count as usize <= DEFAULT_MESHLET_TRIANGLES);
        }
    }

    /// Each cluster's box actually contains its triangles, which is what makes
    /// culling by that box safe: a box smaller than its contents would drop
    /// visible geometry.
    #[test]
    fn a_cluster_box_contains_its_triangles() {
        let mesh = grid(80);
        let set = partition(&mesh, DEFAULT_MESHLET_TRIANGLES).expect("partitions");

        for meshlet in &set.meshlets {
            for position in meshlet.first..meshlet.end() {
                let triangle = mesh.triangles[set.order[position as usize] as usize];
                for corner in triangle {
                    let p = mesh.positions[corner as usize];
                    for axis in 0..3 {
                        assert!(
                            p[axis] >= meshlet.bounds.min[axis] - 1e-4
                                && p[axis] <= meshlet.bounds.max[axis] + 1e-4,
                            "triangle corner {p:?} outside {:?}",
                            meshlet.bounds
                        );
                    }
                }
            }
        }
        // The clusters together cover the whole model's extent.
        let mut union = Bounds::empty();
        for meshlet in &set.meshlets {
            union.extend(meshlet.bounds.min);
            union.extend(meshlet.bounds.max);
        }
        assert_eq!(union.min, mesh.bounds.min);
        assert_eq!(union.max, mesh.bounds.max);
    }

    /// A mesh that already turns is left alone, and a point cloud is not a
    /// mesh to cluster.
    #[test]
    fn small_meshes_and_clouds_are_left_whole() {
        assert!(partition(&grid(40), DEFAULT_MESHLET_TRIANGLES).is_none());
        assert!(partition(&cloud(20_000), DEFAULT_MESHLET_TRIANGLES).is_none());
    }

    /// The flat-shaded layout has no indices to skip, so it is not partitioned
    /// however large it is.
    #[test]
    fn a_flat_shaded_mesh_is_not_partitioned() {
        let mesh = grid(80);
        let flat = Mesh::from_parts(mesh.positions, Vec::new(), Vec::new(), mesh.triangles)
            .expect("mesh builds");
        assert!(partition(&flat, DEFAULT_MESHLET_TRIANGLES).is_none());
    }

    /// A single cluster when the budget covers the whole mesh, so the caller's
    /// "all visible → one draw" shortcut can still fire.
    #[test]
    fn a_budget_that_covers_the_mesh_makes_one_cluster() {
        let mesh = grid(80);
        let set = partition(&mesh, mesh.triangles.len()).expect("partitions");
        assert_eq!(set.meshlets.len(), 1);
        assert_eq!(set.meshlets[0].count as usize, mesh.triangles.len());
    }
}
