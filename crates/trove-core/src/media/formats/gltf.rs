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
    let (document, buffers, images) = match gltf::import(path) {
        Ok(imported) => (imported.0, imported.1, imported.2),
        Err(error) => fallback_import(path, error)?,
    };
    // Texture handle -> image index, resolved once: a material's texture info
    // carries only its position in this table.
    let texture_images: Vec<usize> = document
        .textures()
        .map(|texture| texture.source().index())
        .collect();

    let mut builder = Builder::default();
    for node in root_nodes(&document) {
        builder.add_node(&node, IDENTITY, &buffers, &images, &texture_images);
    }
    builder.finish()
}

/// Import without the validator, for the files it refuses on sight.
///
/// `import` fails outright when a document's `extensionsRequired` names an
/// extension this build of the crate was not compiled with — a Sketchfab
/// export asking for `KHR_materials_pbrSpecularGlossiness`, say — and the
/// refusal covers the whole file, geometry included. The geometry is still
/// perfectly readable, so the parse is retried without validation: the
/// extension's own material block is read where the crate knows it (the
/// feature is enabled), and ignored where it does not. A file that fails this
/// way too was never going to open, and the original error — which names the
/// extension — is the more useful one to report.
fn fallback_import(
    path: &Path,
    original: gltf::Error,
) -> Result<
    (
        gltf::Document,
        Vec<gltf::buffer::Data>,
        Vec<gltf::image::Data>,
    ),
    String,
> {
    let parsed = std::fs::read(path)
        .map_err(|e| format!("failed to load glTF: {e}"))
        .and_then(|bytes| {
            gltf::Gltf::from_slice_without_validation(&bytes)
                .map_err(|e| format!("failed to load glTF: {e}"))
        });
    match parsed {
        Ok(gltf) => {
            let buffers = gltf::import_buffers(&gltf.document, Some(path), gltf.blob)
                .map_err(|e| format!("failed to load glTF: {e}"))?;
            // Best-effort: the images only ever feed a textured primitive's
            // average colour, and a decode failure costs that colour, not the
            // model the first attempt could not open at all.
            let images =
                gltf::import_images(&gltf.document, Some(path), &buffers).unwrap_or_default();
            tracing::info!(
                path = %path.display(),
                %original,
                "glTF imported without validation"
            );
            Ok((gltf.document, buffers, images))
        }
        // The fallback got no further than the first attempt did; the error
        // that named the unsupported extension is the one worth keeping.
        Err(_) => Err(format!("failed to load glTF: {original}")),
    }
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
    /// none, dropped as a set by `finish` when `normals_incomplete` is set.
    normals: Vec<[f32; 3]>,
    /// Some vertex so far came without a normal.
    normals_incomplete: bool,
    /// Parallel to `positions`; the material colour each vertex is painted
    /// with — the primitive's base colour factor times its `COLOR_0`, when
    /// either is present. Placeholder zeros where a primitive carried neither,
    /// dropped as a set by `finish` when `colors_incomplete` is set: shading
    /// half a model by its materials and half by the flat default reads as a
    /// bug, so the set stands or falls together.
    colors: Vec<[f32; 3]>,
    /// Some vertex so far came without a material colour.
    colors_incomplete: bool,
    /// Vertices of `POINTS` primitives. Used only when the document has no
    /// triangles at all: a mesh is either a surface or a cloud.
    points: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
}

impl Builder {
    fn add_node(
        &mut self,
        node: &gltf::Node<'_>,
        parent: [[f32; 4]; 4],
        buffers: &[gltf::buffer::Data],
        images: &[gltf::image::Data],
        texture_images: &[usize],
    ) {
        // A node's placement is its own transform ON TOP OF every ancestor's:
        // the walk composes them, so a part nested under a rotated, scaled
        // root — the shape every Sketchfab or Blender export has — lands
        // where the scene puts it, not where its local transform alone would
        // leave it.
        let combined = mul4(parent, node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                self.add_primitive(&primitive, combined, buffers, images, texture_images);
            }
        }
        for child in node.children() {
            self.add_node(&child, combined, buffers, images, texture_images);
        }
    }

    fn add_primitive(
        &mut self,
        primitive: &gltf::Primitive<'_>,
        transform: [[f32; 4]; 4],
        buffers: &[gltf::buffer::Data],
        images: &[gltf::image::Data],
        texture_images: &[usize],
    ) {
        let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data[..]));
        let Some(positions) = reader.read_positions() else {
            return; // A primitive with no positions is not drawable.
        };
        let local: Vec<[f32; 3]> = positions
            .map(|position| transform_point(transform, position))
            .collect();
        if local.is_empty() {
            return;
        }

        if primitive.mode() == gltf::mesh::Mode::Points {
            self.points.extend(local);
            return;
        }

        let base = self.positions.len() as u32;
        let count = local.len();
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|normals| {
            normals
                .map(|normal| transform_normal(transform, normal))
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
                self.normals_incomplete = true;
                self.normals
                    .extend(std::iter::repeat_n([0.0, 0.0, 0.0], local.len()));
            }
        }
        self.positions.extend(local);

        // The material colour each corner is painted with: the `COLOR_0`
        // attribute times the primitive's base colour factor, per the glTF
        // spec — the factor multiplies everything the material draws. A
        // primitive with neither attribute nor a non-default factor opts out,
        // which is what drops the whole set when the model mixes the two.
        // What the primitive is painted with. A specular-glossiness material
        // has no metallic-roughness block — the crate hands back the default
        // for it — so its diffuse is the colour to read when the extension is
        // there. And a base-colour *texture* paints from its image: the flat
        // renderer cannot show the texture, but its average colour is a fair
        // stand-in, and without it the one textured part of an otherwise
        // material-coloured model would drag the whole set of colours down as
        // incomplete.
        let material = primitive.material();
        let base_texture = material
            .pbr_metallic_roughness()
            .base_color_texture()
            .or_else(|| {
                material
                    .pbr_specular_glossiness()
                    .and_then(|sg| sg.diffuse_texture())
            });
        let factor = match base_texture {
            Some(info) => texture_images
                .get(info.texture().index())
                .and_then(|image| images.get(*image))
                .map(|rgb| {
                    let [r, g, b] = average_rgb(rgb);
                    [r, g, b, 1.0]
                })
                .unwrap_or([1.0, 1.0, 1.0, 1.0]),
            None => material
                .pbr_specular_glossiness()
                .map(|sg| sg.diffuse_factor())
                .unwrap_or_else(|| material.pbr_metallic_roughness().base_color_factor()),
        };
        match reader.read_colors(0) {
            Some(colors) => {
                for color in colors.into_rgb_f32() {
                    self.colors.push([
                        color[0] * factor[0],
                        color[1] * factor[1],
                        color[2] * factor[2],
                    ]);
                }
                debug_assert_eq!(self.colors.len() - base as usize, count);
            }
            None if factor[..3] != [1.0, 1.0, 1.0] => {
                self.colors.extend(std::iter::repeat_n(
                    [factor[0], factor[1], factor[2]],
                    count,
                ));
            }
            None => {
                self.colors_incomplete = true;
                self.colors
                    .extend(std::iter::repeat_n([0.0, 0.0, 0.0], count));
            }
        }

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
            let normals = if !self.normals_incomplete && self.normals.len() == self.positions.len()
            {
                self.normals
            } else {
                Vec::new()
            };
            let colors = if !self.colors_incomplete && self.colors.len() == self.positions.len() {
                self.colors
            } else {
                Vec::new()
            };
            return Mesh::finish(self.positions, normals, colors, self.triangles)
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

/// The average colour of a decoded image, sampled on a coarse grid so a
/// 2048-pixel texture costs a few thousand reads. The renderer paints flat
/// colours, and this is the one honest flat colour a texture has.
fn average_rgb(image: &gltf::image::Data) -> [f32; 3] {
    let channels = match image.format {
        gltf::image::Format::R8G8B8 => 3,
        _ => 4,
    };
    let (width, height) = (image.width.max(1), image.height.max(1));
    // Cap the sample grid at 64×64 however large the image is.
    let (step_x, step_y) = ((width / 64).max(1), (height / 64).max(1));
    let mut sum = [0.0f64; 3];
    let mut count = 0u64;
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let at = ((y * width + x) as usize) * channels;
            let px = &image.pixels[at..at + 3];
            sum[0] += px[0] as f64;
            sum[1] += px[1] as f64;
            sum[2] += px[2] as f64;
            count += 1;
            x += step_x;
        }
        y += step_y;
    }
    if count == 0 {
        return [1.0, 1.0, 1.0];
    }
    let (r, g, b) = (
        sum[0] / count as f64,
        sum[1] / count as f64,
        sum[2] / count as f64,
    );
    // Into the 0..=1 range the vertex colours live in; sRGB bytes into linear
    // would be more correct, but the flat renderer shades in this same space
    // either way.
    [(r / 255.0) as f32, (g / 255.0) as f32, (b / 255.0) as f32]
}

/// Apply a composed node transform to a point. The matrix is glTF's own
/// column-major layout: `m[column][row]`, translation in the last column.
fn transform_point(m: [[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * point[0] + m[1][0] * point[1] + m[2][0] * point[2] + m[3][0],
        m[0][1] * point[0] + m[1][1] * point[1] + m[2][1] * point[2] + m[3][1],
        m[0][2] * point[0] + m[1][2] * point[1] + m[2][2] * point[2] + m[3][2],
    ]
}

/// Apply a composed node transform to a normal: the inverse transpose of the
/// rotation-and-scale part, which is what keeps a normal perpendicular under a
/// non-uniform scale. The length is not restored here: both renderers
/// normalise. A degenerate scale has no inverse — the raw matrix is applied
/// instead, and the normalised result is whatever it comes out as.
fn transform_normal(m: [[f32; 4]; 4], normal: [f32; 3]) -> [f32; 3] {
    // The three columns of the rotation-and-scale block.
    let c0 = [m[0][0], m[0][1], m[0][2]];
    let c1 = [m[1][0], m[1][1], m[1][2]];
    let c2 = [m[2][0], m[2][1], m[2][2]];
    let det = dot(c0, cross(c1, c2));
    if det.abs() < 1e-12 {
        return [
            m[0][0] * normal[0] + m[1][0] * normal[1] + m[2][0] * normal[2],
            m[0][1] * normal[0] + m[1][1] * normal[1] + m[2][1] * normal[2],
            m[0][2] * normal[0] + m[1][2] * normal[1] + m[2][2] * normal[2],
        ];
    }
    // The inverse's columns are the cross products of the other two, scaled
    // by one determinant; the transpose then mixes equal components across
    // them.
    let one = 1.0 / det;
    let i0 = scale3(cross(c1, c2), one);
    let i1 = scale3(cross(c2, c0), one);
    let i2 = scale3(cross(c0, c1), one);
    [
        i0[0] * normal[0] + i1[0] * normal[1] + i2[0] * normal[2],
        i0[1] * normal[0] + i1[1] * normal[1] + i2[1] * normal[2],
        i0[2] * normal[0] + i1[2] * normal[1] + i2[2] * normal[2],
    ]
}

/// Compose two column-major transforms: `mul4(parent, local)` places a node
/// inside its parent, the parent's frame applied after the node's own.
fn mul4(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

/// The walk's starting frame: the first node's transform composes against it.
pub(crate) const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn scale3(v: [f32; 3], k: f32) -> [f32; 3] {
    [v[0] * k, v[1] * k, v[2] * k]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
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

    /// Build a minimal valid GLB (binary glTF) wrapping the given JSON,
    /// against a binary chunk of one triangle.
    fn glb(json: String) -> std::path::PathBuf {
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

    /// Build a minimal valid GLB containing one triangle, with `node` spliced
    /// into the JSON for the scene's single node.
    fn triangle_glb(node: &str) -> std::path::PathBuf {
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
            node, 48
        );
        glb(json)
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

    /// The primitive's base colour factor is what its vertices are painted
    /// with — the one material property this flat-colour renderer can show.
    #[test]
    fn a_materials_base_colour_becomes_vertex_colours() {
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{{"mesh": 0}}],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 0}}]}}],
            "materials": [{{"pbrMetallicRoughness": {{"baseColorFactor": [1.0, 0.5, 0.0, 1.0]}}}}],
            "buffers": [{{"byteLength": 48}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#
        );
        let path = glb(json);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert!(
            mesh.has_vertex_colors(),
            "the material colour comes through"
        );
        assert_eq!(mesh.colors[0], [1.0, 0.5, 0.0]);
        assert_eq!(mesh.colors[2], [1.0, 0.5, 0.0]);
    }

    /// A primitive with neither `COLOR_0` nor a non-default base colour
    /// factor has no material to preview, so the mesh keeps the flat default
    /// it always had.
    #[test]
    fn a_gltf_without_materials_keeps_no_colours() {
        let path = triangle_glb(r#"{"mesh": 0}"#);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert!(!mesh.has_vertex_colors());
        assert!(mesh.colors.is_empty());
    }

    /// A node's placement is its own transform on top of every ancestor's.
    /// The walk used to apply only the local transform, which dropped a part
    /// nested under a rotated or scaled root out of the scene entirely — the
    /// shape every Sketchfab export has.
    #[test]
    fn an_ancestors_transform_is_applied_to_a_nested_mesh() {
        let json = r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [
                {{"children": [1], "scale": [2.0, 2.0, 2.0], "translation": [10.0, 0.0, 0.0]}},
                {{"mesh": 0, "translation": [0.0, 1.0, 0.0]}}
            ],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1}}]}}],
            "buffers": [{{"byteLength": 48}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#;
        let path = glb(json.replace("{{", "{").replace("}}", "}"));
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // The child moves its triangle to (0,1,0); the parent scales that by
        // two and moves it to x=10: (10,2,0)..(12,4,0). Applied in the other
        // order, or not at all, the bounds say so.
        assert_eq!(mesh.bounds.min, [10.0, 2.0, 0.0]);
        assert_eq!(mesh.bounds.max, [12.0, 4.0, 0.0]);
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
}
