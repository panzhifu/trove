//! Shared geometry types for the 3D model pipeline.
//!
//! Everything the model viewport, the two renderers and the off-screen path
//! see is the [`Mesh`] type defined here — the format-specific parsers in the
//! sibling modules normalise their output into it.

use std::collections::HashMap;

/// What a mesh's triangle winding says about it, for back-face culling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Winding {
    /// An open shell, or winding the file does not keep consistent. Both faces
    /// are visible: culling one would punch holes in it.
    #[default]
    TwoSided,
    /// A closed surface whose triangles face outward, counter-clockwise seen
    /// from outside.
    ClosedOutward,
    /// A closed surface wound inside-out. Culling its back faces would show
    /// the inside of the model, so the winding has to be flipped before
    /// culling anything.
    ClosedInward,
}

/// Triangles above which [`Mesh::winding`] gives up and reports two-sided.
const MAX_WINDING_CHECK_TRIANGLES: usize = 2_000_000;

/// Axis-aligned bounds of a mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds {
    /// Bounds of an empty mesh.
    pub(crate) fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    /// Whether any vertex was folded in.
    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }

    pub(crate) fn extend(&mut self, p: [f32; 3]) {
        for ((min, max), v) in self.min.iter_mut().zip(self.max.iter_mut()).zip(p) {
            *min = (*min).min(v);
            *max = (*max).max(v);
        }
    }

    /// Size along each axis.
    pub fn size(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    /// Center of the box.
    pub fn center(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// Longest edge, used to frame the model in the viewport.
    pub fn longest_edge(&self) -> f32 {
        let size = self.size();
        size[0].max(size[1]).max(size[2])
    }
}

impl Default for Bounds {
    fn default() -> Self {
        Self::empty()
    }
}

/// Usable geometry in model space: a triangle mesh, or a point cloud when the
/// file carries no faces.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    /// Per-vertex normals; empty (or a different length than `positions`)
    /// means the file carries none and the renderer shades per face.
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex colour in 0..=1, empty when the file carries none. Read by
    /// the point renderer; a triangle mesh shades from the material instead.
    pub colors: Vec<[f32; 3]>,
    /// Triangles; empty for a point cloud, which is drawn vertex by vertex.
    pub triangles: Vec<[u32; 3]>,
    pub bounds: Bounds,
}

impl Mesh {
    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Number of vertices.
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    /// A cloud of points rather than a surface: there is nothing to
    /// rasterise, so the renderers draw one sprite per vertex.
    pub fn is_point_cloud(&self) -> bool {
        self.triangles.is_empty() && !self.positions.is_empty()
    }

    /// What the frame-size heuristics count: triangles for a mesh, points for
    /// a cloud. Both grow the work per frame, so they share one knob.
    pub fn primitive_count(&self) -> usize {
        if self.is_point_cloud() {
            self.positions.len()
        } else {
            self.triangles.len()
        }
    }

    /// What the triangles' winding says about this mesh.
    ///
    /// A closed, consistently wound surface is the only case where a back face
    /// is guaranteed to be hidden: culling it then halves the fragments a
    /// renderer has to shade, and does it without changing the picture. An
    /// open shell or a file with inconsistent winding keeps both faces, or a
    /// single-sided scan would develop holes seen from behind.
    pub fn winding(&self) -> Winding {
        self.winding_capped(MAX_WINDING_CHECK_TRIANGLES)
    }

    /// [`Mesh::winding`] with the work cap made explicit, for tests.
    ///
    /// The check is a pass over the triangles with a hash map of edges, so it
    /// is only worth doing while the map stays smaller than the geometry it
    /// describes. Above the cap the mesh is reported as two-sided, which is
    /// always safe.
    pub(crate) fn winding_capped(&self, cap: usize) -> Winding {
        if self.triangles.is_empty() || self.triangles.len() > cap {
            return Winding::TwoSided;
        }
        // Closed and consistent: every directed edge occurs once and its
        // reverse occurs once. Anything else is a boundary or a fold.
        let mut edges: HashMap<(u32, u32), u32> = HashMap::with_capacity(self.triangles.len() * 3);
        for triangle in &self.triangles {
            for (from, to) in [
                (triangle[0], triangle[1]),
                (triangle[1], triangle[2]),
                (triangle[2], triangle[0]),
            ] {
                *edges.entry((from, to)).or_insert(0) += 1;
            }
        }
        for (&(from, to), &count) in &edges {
            if count != 1 || edges.get(&(to, from)) != Some(&1) {
                return Winding::TwoSided;
            }
        }
        // Which way it faces: the signed volume of a closed surface is
        // positive when its triangles wind counter-clockwise as seen from
        // outside.
        let mut volume = 0.0f64;
        for triangle in &self.triangles {
            let a = self.positions[triangle[0] as usize];
            let b = self.positions[triangle[1] as usize];
            let c = self.positions[triangle[2] as usize];
            volume += (a[0] as f64 * (b[1] as f64 * c[2] as f64 - b[2] as f64 * c[1] as f64)
                + a[1] as f64 * (b[2] as f64 * c[0] as f64 - b[0] as f64 * c[2] as f64)
                + a[2] as f64 * (b[0] as f64 * c[1] as f64 - b[1] as f64 * c[0] as f64))
                / 6.0;
        }
        if volume >= 0.0 {
            Winding::ClosedOutward
        } else {
            Winding::ClosedInward
        }
    }

    /// Whether the model has usable per-vertex normals.
    pub fn has_vertex_normals(&self) -> bool {
        self.normals.len() == self.positions.len() && !self.normals.is_empty()
    }

    /// Whether the model has a usable colour per vertex.
    pub fn has_vertex_colors(&self) -> bool {
        self.colors.len() == self.positions.len() && !self.colors.is_empty()
    }

    /// Assemble a mesh from geometry that was not read from a file — a
    /// simplified LOD level, above all.
    ///
    /// The bounds are computed here and the per-vertex arrays are validated,
    /// so a caller cannot hand the renderers normals that do not line up with
    /// its positions. Note what is *not* checked: passing no triangles builds
    /// a point cloud ([`Mesh::is_point_cloud`]), which is a legitimate cloud
    /// but also what a forgotten `triangles` argument looks like.
    pub fn from_parts(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        Self::assemble(positions, normals, colors, triangles)
    }

    /// Build a mesh from raw positions/triangles, dropping degenerate
    /// triangles and recomputing the bounds. Returns `None` when nothing
    /// usable is left.
    pub(crate) fn finish(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        let mesh = Self::assemble(positions, normals, colors, triangles)?;
        (!mesh.is_point_cloud()).then_some(mesh)
    }

    /// Build a point cloud: positions only, no triangles to filter.
    pub(crate) fn finish_points(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
    ) -> Option<Self> {
        Self::assemble(positions, normals, colors, Vec::new())
    }

    /// Shared tail of both constructors: checks the vertices are usable,
    /// drops degenerate triangles and computes the bounds.
    fn assemble(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        if positions.is_empty() {
            return None;
        }
        let count = positions.len() as u32;
        let triangles: Vec<[u32; 3]> = triangles
            .into_iter()
            .filter(|t| t[0] < count && t[1] < count && t[2] < count)
            .filter(|t| t[0] != t[1] && t[1] != t[2] && t[0] != t[2])
            .collect();
        let mut bounds = Bounds::empty();
        for p in &positions {
            bounds.extend(*p);
        }
        if bounds.is_empty() {
            return None;
        }
        let normals = if normals.len() == positions.len() {
            normals
        } else {
            Vec::new()
        };
        let colors = if colors.len() == positions.len() {
            colors
        } else {
            Vec::new()
        };
        Some(Self {
            positions,
            normals,
            colors,
            triangles,
            bounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh(positions: Vec<[f32; 3]>, triangles: Vec<[u32; 3]>) -> Mesh {
        Mesh::from_parts(positions, Vec::new(), Vec::new(), triangles).expect("mesh builds")
    }

    /// A cube, wound counter-clockwise as seen from outside.
    fn cube() -> Mesh {
        mesh(
            vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 1.0],
                [1.0, 1.0, 1.0],
                [0.0, 1.0, 1.0],
            ],
            vec![
                // z = 0 face, seen from outside (-z): clockwise in xy, so the
                // corners must be listed in that order.
                [0, 2, 1],
                [0, 3, 2],
                // z = 1 face, seen from outside (+z).
                [4, 5, 6],
                [4, 6, 7],
                // y = 0 face, outside is -y.
                [0, 1, 5],
                [0, 5, 4],
                // y = 1 face, outside is +y.
                [3, 7, 6],
                [3, 6, 2],
                // x = 0 face, outside is -x.
                [0, 4, 7],
                [0, 7, 3],
                // x = 1 face, outside is +x.
                [1, 2, 6],
                [1, 6, 5],
            ],
        )
    }

    #[test]
    fn a_closed_cube_winds_outward() {
        assert_eq!(cube().winding(), Winding::ClosedOutward);
    }

    /// The same surface with every triangle flipped is still closed, but it
    /// faces inward — culling its back faces would show the model's inside,
    /// so the renderer has to reverse it on the way into its buffers.
    #[test]
    fn a_flipped_cube_winds_inward() {
        let mut inside_out = cube();
        for triangle in &mut inside_out.triangles {
            triangle.swap(1, 2);
        }
        assert_eq!(inside_out.winding(), Winding::ClosedInward);
        assert_eq!(inside_out.bounds, cube().bounds);
    }

    /// An open shell has to keep both faces, or it disappears when seen from
    /// behind.
    #[test]
    fn an_open_shell_is_two_sided() {
        let quad = mesh(
            vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        assert_eq!(quad.winding(), Winding::TwoSided);
    }

    /// A cube with one face wound the other way is not consistently wound: it
    /// is neither closed nor safe to cull.
    #[test]
    fn inconsistent_winding_is_two_sided() {
        let mut broken = cube();
        broken.triangles[4].swap(1, 2);
        assert_eq!(broken.winding(), Winding::TwoSided);
    }

    /// The check is a pass with a hash map over the edges, so it is capped:
    /// past the cap the mesh is simply reported as two-sided.
    #[test]
    fn the_winding_check_gives_up_past_its_cap() {
        let cube = cube();
        assert_eq!(cube.winding_capped(11), Winding::TwoSided);
        assert_eq!(cube.winding_capped(12), Winding::ClosedOutward);
    }

    /// A point cloud has no winding to speak of, and must not be culled.
    #[test]
    fn a_point_cloud_is_two_sided() {
        let cloud = mesh(vec![[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]], Vec::new());
        assert!(cloud.is_point_cloud());
        assert_eq!(cloud.winding(), Winding::TwoSided);
    }
}
