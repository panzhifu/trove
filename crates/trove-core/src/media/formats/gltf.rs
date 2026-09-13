//! glTF 2.0 parser (`.gltf` / `.glb`).
//!
//! Uses the `gltf` crate to handle the JSON schema, buffer resolution and
//! accessor types — these are too involved to hand-write. The parsed document
//! is then flattened into a single [`Mesh`] the same way OBJ/STL/PLY are,
//! so the renderer never needs to know the source format.
//!
//! Two things the flattened mesh has to get right: a glTF mesh lives in a
//! node, and a node's transform is what places it (every Blender or GLB
//! export carries at least a Y-up rotation), so the scene graph is walked and
//! each node's transform is applied to its vertices. And a primitive's mode
//! decides how its indices form triangles — reading a point or strip
//! primitive as a triangle list yields garbage, so each mode is converted
//! explicitly.

use std::collections::HashSet;
use std::path::Path;

use super::types::Mesh;

/// Load a glTF or GLB file. Buffers are resolved relative to the file.
pub fn load_gltf(path: &Path) -> Result<Mesh, String> {
    let (document, buffers, _images) =
        gltf::import(path).map_err(|e| format!("failed to load glTF: {e}"))?;

    let mut builder = Builder::default();
    for node in root_nodes(&document) {
        builder.add_node(&node, &buffers);
    }
    builder.finish()
}

/// The nodes the default scene draws — or, for a document that names no
/// scene, the first one, or finally every node no other node adopts.
///
/// Walking *all* nodes instead would draw each child a second time under its
/// own identity, without its parent's transform.
fn root_nodes(document: &gltf::Document) -> Vec<gltf::Node<'_>> {
    if let Some(scene) = document.default_scene() {
        return scene.nodes().collect();
    }
    if let Some(scene) = document.scenes().next() {
        return scene.nodes().collect();
    }
    let children: HashSet<usize> = document
        .nodes()
        .flat_map(|node| node.children().map(|child| child.index()))
        .collect();
    document
        .nodes()
        .filter(|node| !children.contains(&node.index()))
        .collect()
}

/// A glTF document flattened into one mesh.
#[derive(Default)]
struct Builder {
    positions: Vec<[f32; 3]>,
    /// Parallel to `positions`; placeholder zeros where a primitive carried
    /// none, dropped as a set by `finish` when `normals_complete` is false.
    normals: Vec<[f32; 3]>,
    /// Whether every vertex so far came with a normal.
    normals_complete: bool,
    /// Vertices of `POINTS` primitives. Used only when the document has no
    /// triangles at all: a mesh is either a surface or a cloud.
    points: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
}

impl Builder {
    fn add_node(&mut self, node: &gltf::Node<'_>, buffers: &[gltf::buffer::Data]) {
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                self.add_primitive(&primitive, node.transform(), buffers);
            }
        }
        for child in node.children() {
            self.add_node(&child, buffers);
        }
    }

    fn add_primitive(
        &mut self,
        primitive: &gltf::Primitive<'_>,
        transform: gltf::scene::Transform,
        buffers: &[gltf::buffer::Data],
    ) {
        let (translation, rotation, scale) = transform.decomposed();
        let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data[..]));
        let Some(positions) = reader.read_positions() else {
            return; // A primitive with no positions is not drawable.
        };
        let local: Vec<[f32; 3]> = positions
            .map(|position| transform_point(translation, rotation, scale, position))
            .collect();
        if local.is_empty() {
            return;
        }

        if primitive.mode() == gltf::mesh::Mode::Points {
            self.points.extend(local);
            return;
        }

        let base = self.positions.len() as u32;
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|normals| {
            normals
                .map(|normal| transform_normal(rotation, scale, normal))
                .collect()
        });
        match normals {
            Some(normals) if normals.len() == local.len() => {
                self.normals.extend(normals);
            }
            _ => {
                // One primitive without normals drops the whole set: shading
                // half a model smooth and half flat looks broken, and a
                // half-length array would index out of bounds.
                self.normals_complete = false;
                self.normals
                    .extend(std::iter::repeat_n([0.0, 0.0, 0.0], local.len()));
            }
        }
        self.positions.extend(local);

        let indices: Vec<u32> = match reader.read_indices() {
            Some(indices) => indices.into_u32().collect(),
            None => (0..(self.positions.len() - base as usize) as u32).collect(),
        };
        for triangle in triangles_of(primitive.mode(), &indices) {
            self.triangles
                .push([base + triangle[0], base + triangle[1], base + triangle[2]]);
        }
    }

    fn finish(self) -> Result<Mesh, String> {
        if !self.triangles.is_empty() {
            let normals = if self.normals_complete && self.normals.len() == self.positions.len() {
                self.normals
            } else {
                Vec::new()
            };
            return Mesh::finish(self.positions, normals, Vec::new(), self.triangles)
                .ok_or_else(|| "the glTF file contains no drawable geometry".to_string());
        }
        // No triangles: a `POINTS` document is a cloud, and the renderers
        // already know how to draw one.
        Mesh::finish_points(self.points, Vec::new(), Vec::new())
            .ok_or_else(|| "the glTF file contains no drawable geometry".to_string())
    }
}

/// The triangles a primitive's index list describes, under its draw mode.
///
/// A mode that is not a triangle kind — lines, or points — contributes none.
fn triangles_of(mode: gltf::mesh::Mode, indices: &[u32]) -> Vec<[u32; 3]> {
    let mut triangles = Vec::new();
    match mode {
        gltf::mesh::Mode::Triangles => {
            for triangle in indices.as_chunks::<3>().0 {
                triangles.push(*triangle);
            }
        }
        gltf::mesh::Mode::TriangleStrip => {
            // Every window of three, with the winding alternated so the
            // strip's back faces do not flip on every other triangle.
            for (i, window) in indices.windows(3).enumerate() {
                triangles.push(if i % 2 == 0 {
                    [window[0], window[1], window[2]]
                } else {
                    [window[1], window[0], window[2]]
                });
            }
        }
        gltf::mesh::Mode::TriangleFan => {
            // Every triangle shares the fan's first vertex; a sliding window
            // would produce a strip instead, with the wrong triangles.
            for i in 1..indices.len().saturating_sub(1) {
                triangles.push([indices[0], indices[i], indices[i + 1]]);
            }
        }
        gltf::mesh::Mode::Points
        | gltf::mesh::Mode::Lines
        | gltf::mesh::Mode::LineLoop
        | gltf::mesh::Mode::LineStrip => {}
    }
    triangles
}

/// Apply a node's translation, rotation and scale to a point. The pieces
/// arrive decomposed because `Transform` is not `Copy` and the readers need
/// them inside their closures.
fn transform_point(
    translation: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    point: [f32; 3],
) -> [f32; 3] {
    let scaled = [
        point[0] * scale[0],
        point[1] * scale[1],
        point[2] * scale[2],
    ];
    let rotated = rotate(rotation, scaled);
    [
        rotated[0] + translation[0],
        rotated[1] + translation[1],
        rotated[2] + translation[2],
    ]
}

/// Apply a node's rotation to a normal, undoing its scale first — a normal
/// transforms by the inverse transpose, and for an axis-aligned scale that is
/// the reciprocal. The length is not restored here: both renderers normalise.
fn transform_normal(rotation: [f32; 4], scale: [f32; 3], normal: [f32; 3]) -> [f32; 3] {
    let unscaled = [
        if scale[0] != 0.0 {
            normal[0] / scale[0]
        } else {
            0.0
        },
        if scale[1] != 0.0 {
            normal[1] / scale[1]
        } else {
            0.0
        },
        if scale[2] != 0.0 {
            normal[2] / scale[2]
        } else {
            0.0
        },
    ];
    rotate(rotation, unscaled)
}

/// Rotate `v` by a `[x, y, z, w]` quaternion.
fn rotate(quaternion: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let q = [quaternion[0], quaternion[1], quaternion[2]];
    let w = quaternion[3];
    let uv = cross(q, v);
    let uuv = cross(q, uv);
    [
        v[0] + 2.0 * (w * uv[0] + uuv[0]),
        v[1] + 2.0 * (w * uv[1] + uuv[1]),
        v[2] + 2.0 * (w * uv[2] + uuv[2]),
    ]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal valid GLB (binary glTF) containing one triangle, with
    /// `node` spliced into the JSON for the scene's single node.
    fn triangle_glb(node: &str) -> std::path::PathBuf {
        let bin: Vec<u8> = {
            let mut v = Vec::new();
            // 3 vertices: (0,0,0), (1,0,0), (0,1,0)
            for &[x, y, z] in &[[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v.extend_from_slice(&z.to_le_bytes());
            }
            // 3 u32 indices: 0, 1, 2
            for idx in [0u32, 1, 2] {
                v.extend_from_slice(&idx.to_le_bytes());
            }
            v
        };

        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{}],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1}}]}}],
            "buffers": [{{"byteLength": {}}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#,
            node,
            bin.len()
        );

        let json_bytes = json.into_bytes();
        let json_pad = (4 - (json_bytes.len() % 4)) % 4;
        let bin_pad = (4 - (bin.len() % 4)) % 4;

        let total = 12 + 8 + json_bytes.len() + json_pad + 8 + bin.len() + bin_pad;

        let path =
            std::env::temp_dir().join(format!("trove-gltf-test-{}.glb", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&path).unwrap();

        // GLB header
        f.write_all(b"glTF").unwrap(); // magic
        f.write_all(&2u32.to_le_bytes()).unwrap(); // version
        f.write_all(&(total as u32).to_le_bytes()).unwrap(); // total length

        // JSON chunk
        f.write_all(&((json_bytes.len() + json_pad) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"JSON").unwrap();
        f.write_all(&json_bytes).unwrap();
        f.write_all(&vec![b' '; json_pad]).unwrap();

        // BIN chunk
        f.write_all(&((bin.len() + bin_pad) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"BIN\x00").unwrap();
        f.write_all(&bin).unwrap();
        f.write_all(&vec![0u8; bin_pad]).unwrap();

        path
    }

    #[test]
    fn triangle_parses_from_glb() {
        let path = triangle_glb(r#"{"mesh": 0}"#);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.triangles[0], [0, 1, 2]);
        assert_eq!(mesh.bounds.max, [1.0, 1.0, 0.0]);
    }

    /// The node's transform is what places the geometry. Ignoring it — the
    /// old behaviour, which read `document.meshes()` directly — puts every
    /// scaled, rotated or moved model in the wrong place at the wrong size.
    #[test]
    fn a_node_transform_is_applied() {
        let path = triangle_glb(
            r#"{"mesh": 0, "translation": [10.0, 0.0, -2.0], "scale": [2.0, 2.0, 2.0]}"#,
        );
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // The triangle is (0,0,0), (1,0,0), (0,1,0). Scaled by two it spans
        // x and y over 0..2, then the translation moves it to x 10..12,
        // y 0..2, z = -2.
        assert_eq!(mesh.bounds.min, [10.0, 0.0, -2.0]);
        assert_eq!(mesh.bounds.max, [12.0, 2.0, -2.0]);
    }

    /// A quarter turn about Z maps (1,0,0) to (0,1,0); the height of the
    /// triangle follows it.
    #[test]
    fn a_node_rotation_is_applied() {
        // sin(45°) = cos(45°) = √2/2 gives a 90° rotation about Z.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let path = triangle_glb(&format!(r#"{{"mesh": 0, "rotation": [0, 0, {s}, {s}]}}"#));
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // (0,0,0) stays; (1,0,0) becomes (0,1,0); (0,1,0) becomes (-1,0,0).
        let expected = [[0.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        assert_eq!(mesh.positions.len(), expected.len());
        for want in expected {
            assert!(
                mesh.positions
                    .iter()
                    .any(|got| { (0..3).all(|axis| (got[axis] - want[axis]).abs() < 1e-4) }),
                "no vertex near {want:?} in {:?}",
                mesh.positions
            );
        }
    }

    /// A strip alternates winding; reading it as a triangle list instead
    /// yields half the triangles and the wrong ones.
    #[test]
    fn triangle_strips_and_fans_are_converted() {
        let strip = triangles_of(gltf::mesh::Mode::TriangleStrip, &[0, 1, 2, 3]);
        assert_eq!(strip, vec![[0, 1, 2], [2, 1, 3]]);
        let fan = triangles_of(gltf::mesh::Mode::TriangleFan, &[0, 1, 2, 3]);
        assert_eq!(fan, vec![[0, 1, 2], [0, 2, 3]]);
        // A list is taken three at a time, and a run that does not divide by
        // three is not a triangle.
        assert_eq!(
            triangles_of(gltf::mesh::Mode::Triangles, &[0, 1, 2, 3, 4, 5, 6]),
            vec![[0, 1, 2], [3, 4, 5]]
        );
        // Lines and points are not surfaces.
        assert!(triangles_of(gltf::mesh::Mode::Points, &[0, 1, 2]).is_empty());
        assert!(triangles_of(gltf::mesh::Mode::LineStrip, &[0, 1, 2]).is_empty());
    }

    /// A quaternion's rotation is its own inverse at the opposite angle, and
    /// the identity leaves a vector alone.
    #[test]
    fn rotate_matches_the_quaternion_axes() {
        let identity = rotate([0.0, 0.0, 0.0, 1.0], [1.0, 2.0, 3.0]);
        assert_eq!(identity, [1.0, 2.0, 3.0]);
        let half = std::f32::consts::FRAC_1_SQRT_2;
        // 90° about Z: x -> y, y -> -x.
        let turned = rotate([0.0, 0.0, half, half], [1.0, 0.0, 0.0]);
        assert!(turned[0].abs() < 1e-5 && (turned[1] - 1.0).abs() < 1e-5);
    }
}
