//! Wavefront OBJ parser.
//!
//! Hand-written: the format is simple enough that a dependency-free parser
//! is shorter and more predictable than pulling in `tobj`. Faces are
//! fan-triangulated, so quads and n-gons work; texture coordinates are
//! ignored, and materials come in as per-vertex diffuse colours.
//!
//! Normals are used only when *every* face corner carries one — a partial set
//! would shade the wrong corners, so the renderer falls back to flat shading
//! instead. When they are used, a position that appears with more than one
//! normal is split into one vertex per (position, normal) pair: that is what
//! an OBJ means by it, and a cube with per-face normals (the usual Blender
//! export) has eight positions but twenty-four such pairs. Merging them by
//! position instead — keeping whichever normal came first — smooths a
//! faceted model into a pillow.
//!
//! Materials follow the same rule with colour as the third key. `usemtl`
//! names the material every face after it draws with, and the `mtllib` files
//! beside the OBJ give those names their diffuse (`Kd`) colours — read only
//! when a library is named, since a bare OBJ carries no colours at all. A
//! position shared across two materials is split, or one vertex would have to
//! be two colours at once; textures (`map_Kd`) are not read, because the
//! renderer takes flat colours and a texture belongs to a later, textured
//! pipeline.

use std::collections::HashMap;
use std::path::Path;

use super::types::Mesh;

/// The colour a face whose material has no `Kd` line — or names a material
/// the libraries never defined — is painted with: white, which is what an
/// uncoloured diffuse reads as under this renderer's lighting.
const MISSING_MATERIAL: [f32; 3] = [1.0, 1.0, 1.0];

/// Parse a Wavefront OBJ with no library resolution: any `mtllib` names a
/// material the map will never fill, so every material is white. What the
/// string-only callers (tests, a caller with no disk path) want.
pub fn load_obj(text: &str) -> Result<Mesh, String> {
    load_obj_at(Path::new(""), text)
}

/// Parse a Wavefront OBJ from `path`, resolving `mtllib` libraries beside it.
/// Faces are fan-triangulated, so quads and n-gons work; texture coordinates
/// are ignored, and materials come in as per-vertex colours (see the module
/// docs).
pub fn load_obj_at(path: &Path, text: &str) -> Result<Mesh, String> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    // Per-face normal indices, resolved into per-vertex normals at the end.
    let mut normal_ids: Vec<Vec<u32>> = Vec::new();
    // The material colour of every face, in triangle order. `None` while the
    // file has named no material at all — the state of a bare OBJ, which then
    // keeps the flat default rather than a wall of white.
    let mut face_colors: Vec<Option<[f32; 3]>> = Vec::new();
    // The material the `usemtl` lines put the parser under.
    let mut current: Option<[f32; 3]> = None;
    // `usemtl` names resolve against the libraries the file lists, read once
    // when the first one is named (OBJs list their libraries before use).
    let mut libraries: HashMap<String, [f32; 3]> = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                let values = parse_floats(parts.take(3));
                if values.len() == 3 {
                    positions.push([values[0], values[1], values[2]]);
                }
            }
            Some("vn") => {
                let values = parse_floats(parts.take(3));
                if values.len() == 3 {
                    normals.push([values[0], values[1], values[2]]);
                }
            }
            Some("mtllib") => {
                for name in parts {
                    read_mtl(path, name, &mut libraries);
                }
            }
            Some("usemtl") => {
                let name = parts.next().unwrap_or_default();
                current = Some(libraries.get(name).copied().unwrap_or(MISSING_MATERIAL));
            }
            Some("f") => {
                // A corner with an index the file does not define makes the
                // whole face unusable: welding it to vertex 0, as a fallback
                // would, draws a triangle across the model.
                let corners: Option<Vec<(u32, u32)>> = parts
                    .map(|token| resolve_obj_corner(token, positions.len(), normals.len()))
                    .collect();
                let Some(corners) = corners else {
                    continue;
                };
                if corners.len() < 3 {
                    continue;
                }
                let ids: Vec<u32> = corners.iter().map(|(_, n)| *n).collect();
                for i in 1..corners.len() - 1 {
                    triangles.push([corners[0].0, corners[i].0, corners[i + 1].0]);
                    normal_ids.push(vec![ids[0], ids[i], ids[i + 1]]);
                    face_colors.push(current);
                }
            }
            _ => {}
        }
    }

    // With no material named anywhere, no colour is written at all: the mesh
    // keeps the flat default it always had, and `has_vertex_colors` stays
    // false.
    let colored = face_colors.iter().any(Option::is_some);

    // A complete set of per-face normals lets the model be shaded smoothly —
    // or faceted, since a cube's normals differ per face. Each (position,
    // normal) pair becomes its own vertex, which is how the file spells
    // "this corner faces this way".
    let complete = normal_ids.len() == triangles.len()
        && !normals.is_empty()
        && normal_ids
            .iter()
            .all(|ids| ids.iter().all(|id| *id != u32::MAX));
    if !complete {
        // Flat shading keeps the positions shared — unless two faces of one
        // position carry different materials, which forces the same split the
        // normals path does, keyed by colour instead.
        if !colored {
            return Mesh::finish(positions, Vec::new(), Vec::new(), triangles)
                .ok_or_else(|| "the OBJ file contains no triangles".to_string());
        }
        let mut split_positions: Vec<[f32; 3]> = Vec::new();
        let mut split_colors: Vec<[f32; 3]> = Vec::new();
        let mut seen: HashMap<(u32, [u32; 3]), u32> = HashMap::new();
        let mut split_triangles: Vec<[u32; 3]> = Vec::with_capacity(triangles.len());
        for (triangle, color) in triangles.iter().zip(face_colors.iter()) {
            let color = color.unwrap_or(MISSING_MATERIAL);
            let mut remapped = [0u32; 3];
            for slot in 0..3 {
                let position = triangle[slot];
                remapped[slot] = *seen.entry((position, color_key(color))).or_insert_with(|| {
                    split_positions.push(positions[position as usize]);
                    split_colors.push(color);
                    (split_positions.len() - 1) as u32
                });
            }
            split_triangles.push(remapped);
        }
        return Mesh::finish(split_positions, Vec::new(), split_colors, split_triangles)
            .ok_or_else(|| "the OBJ file contains no triangles".to_string());
    }

    let mut split_positions: Vec<[f32; 3]> = Vec::new();
    let mut split_normals: Vec<[f32; 3]> = Vec::new();
    let mut split_colors: Vec<[f32; 3]> = Vec::new();
    let mut seen: HashMap<(u32, u32, [u32; 3]), u32> = HashMap::new();
    let mut split_triangles: Vec<[u32; 3]> = Vec::with_capacity(triangles.len());
    for ((triangle, ids), color) in triangles
        .iter()
        .zip(normal_ids.iter())
        .zip(face_colors.iter())
    {
        let color = color.unwrap_or(MISSING_MATERIAL);
        let mut remapped = [0u32; 3];
        for slot in 0..3 {
            let position = triangle[slot];
            let normal = ids[slot];
            remapped[slot] = *seen
                .entry((position, normal, color_key(color)))
                .or_insert_with(|| {
                    split_positions.push(positions[position as usize]);
                    split_normals.push(normals[normal as usize]);
                    split_colors.push(color);
                    (split_positions.len() - 1) as u32
                });
        }
        split_triangles.push(remapped);
    }

    Mesh::finish(
        split_positions,
        split_normals,
        split_colors,
        split_triangles,
    )
    .ok_or_else(|| "the OBJ file contains no triangles".to_string())
}

/// A colour as a hash-map key: the floats' bit patterns, since `f32` has no
/// `Eq` and two colours are the same vertex exactly when their bits are.
fn color_key(color: [f32; 3]) -> [u32; 3] {
    [color[0].to_bits(), color[1].to_bits(), color[2].to_bits()]
}

/// Resolve one `f` corner (`v`, `v/vt`, `v//vn` or `v/vt/vn`) into a
/// (position, normal) index pair; OBJ indices are 1-based and may be
/// negative (relative to the end). `u32::MAX` marks "no normal", and `None`
/// means the corner names a vertex the file never defined.
fn resolve_obj_corner(
    token: &str,
    position_count: usize,
    normal_count: usize,
) -> Option<(u32, u32)> {
    let mut fields = token.split('/');
    let v = fields.next().unwrap_or("");
    let _tex = fields.next();
    let n = fields.next();
    let resolve = |raw: &str, count: usize| -> Option<u32> {
        let value: i64 = raw.parse().ok()?;
        let index = if value < 0 {
            count as i64 + value
        } else {
            value - 1
        };
        (index >= 0 && (index as usize) < count).then_some(index as u32)
    };
    let position = resolve(v, position_count)?;
    let normal = n.and_then(|n| resolve(n, normal_count)).unwrap_or(u32::MAX);
    Some((position, normal))
}

/// Read one MTL library named by `mtllib`, collecting each material's diffuse
/// colour. Best-effort by design: a library the disk does not have (an OBJ
/// downloaded without its `.mtl`, say) leaves every material white rather
/// than failing the model, which would trade a wrong colour for no model.
///
/// `Kd` is the one line read — the diffuse colour, which is what this
/// renderer shades with. Textures are not followed, and the other colours
/// (`Ka`, `Ks`) belong to lighting models the viewport does not run.
fn read_mtl(obj_path: &Path, name: &str, into: &mut HashMap<String, [f32; 3]>) {
    let Some(path) = obj_path.parent().map(|dir| dir.join(name)) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("newmtl") => {
                current = parts.next().map(String::from);
            }
            Some("Kd") => {
                let values = parse_floats(parts.take(3));
                if let (Some(material), values) = (current.as_ref(), values.as_slice())
                    && values.len() == 3
                {
                    // MTL colours are 0..1, the same range the vertex colours
                    // the renderers take.
                    into.insert(material.clone(), [values[0], values[1], values[2]]);
                }
            }
            _ => {}
        }
    }
}

/// Parse up to three floats, stopping at the first unparsable token.
fn parse_floats<'a>(tokens: impl Iterator<Item = &'a str>) -> Vec<f32> {
    tokens
        .map(|t| t.trim().parse::<f32>())
        .take_while(|r| r.is_ok())
        .filter_map(Result::ok)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUBE_OBJ: &str = "\
# a unit cube
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
v 0 0 1
v 1 0 1
v 1 1 1
v 0 1 1
vn 0 0 1
f 1/1/1 2/1/1 3/1/1 4/1/1
f 5 6 7 8
";

    #[test]
    fn obj_quads_are_fan_triangulated() {
        let mesh = load_obj(CUBE_OBJ).expect("cube parses");
        assert_eq!(mesh.vertex_count(), 8);
        assert_eq!(mesh.triangle_count(), 4);
        assert_eq!(mesh.bounds.min, [0.0, 0.0, 0.0]);
        assert_eq!(mesh.bounds.max, [1.0, 1.0, 1.0]);
        assert_eq!(mesh.bounds.longest_edge(), 1.0);
        assert_eq!(mesh.bounds.center(), [0.5, 0.5, 0.5]);
        // The first face carried a normal, the second did not, so the mesh
        // is shaded flat.
        assert!(!mesh.has_vertex_normals());
    }

    #[test]
    fn obj_negative_indices_and_full_normals() {
        let obj = "\
v 0 0 0
v 1 0 0
v 0 1 0
vn 0 0 1
f -3//1 -2//1 -1//1
";
        let mesh = load_obj(obj).expect("triangle parses");
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.triangles[0], [0, 1, 2]);
        assert!(mesh.has_vertex_normals());
    }

    /// A cube whose faces carry their own normals has eight positions but
    /// twenty-four corners. Each (position, normal) pair is a vertex of its
    /// own, or the six faces would share one normal each and the cube would
    /// be shaded as a rounded blob.
    #[test]
    fn a_corner_used_with_two_normals_is_split() {
        // One position, two faces at right angles, each with its own normal.
        let obj = "\
v 0 0 0
v 1 0 0
v 0 1 0
v 0 0 1
vn 0 0 1
vn 0 1 0
f 1//1 2//1 3//1
f 1//2 4//2 2//2
";
        let mesh = load_obj(obj).expect("two faces parse");
        assert_eq!(mesh.triangle_count(), 2);
        // Position 0 is used by both faces with a different normal, so it
        // appears twice; the other four corners are single-use.
        assert_eq!(mesh.vertex_count(), 6, "{:?}", mesh.positions);
        assert!(mesh.has_vertex_normals());
        // The two vertices at the shared position differ only in normal.
        let shared: Vec<usize> = mesh
            .positions
            .iter()
            .enumerate()
            .filter(|(_, p)| **p == [0.0, 0.0, 0.0])
            .map(|(i, _)| i)
            .collect();
        assert_eq!(shared.len(), 2);
        assert_ne!(mesh.normals[shared[0]], mesh.normals[shared[1]]);
        // Both normals are the ones the file declared.
        let mut normals: Vec<[f32; 3]> = shared.iter().map(|&i| mesh.normals[i]).collect();
        normals.sort_by(|a, b| a[1].partial_cmp(&b[1]).unwrap());
        assert_eq!(normals, vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]);
        // And every triangle indexes a vertex that exists.
        for tri in &mesh.triangles {
            for &vertex in tri {
                assert!((vertex as usize) < mesh.vertex_count());
            }
        }
    }

    /// A face naming a vertex the file never defined is dropped, not welded
    /// to vertex 0.
    #[test]
    fn a_face_with_an_undefined_index_is_dropped() {
        let obj = "\
v 0 0 0
v 1 0 0
v 0 1 0
f 1 2 3
f 1 2 99
";
        let mesh = load_obj(obj).expect("the valid face parses");
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.triangles[0], [0, 1, 2]);
    }

    #[test]
    fn obj_without_geometry_is_an_error() {
        assert!(load_obj("# nothing here\nv 0 0 0\n").is_err());
    }

    /// A pair of files on disk: the OBJ text and, when given, the MTL library
    /// it names — `load_obj_at` resolves libraries beside the file, so the
    /// material tests need both.
    fn obj_on_disk(obj: &str, mtl: Option<(&str, &str)>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-obj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.obj");
        std::fs::write(&path, obj).unwrap();
        if let Some((name, text)) = mtl {
            std::fs::write(dir.join(name), text).unwrap();
        }
        path
    }

    /// The diffuse colours an MTL library gives each `usemtl` group ride
    /// through the parse as per-vertex colours.
    #[test]
    fn obj_materials_become_vertex_colours() {
        let obj = "\
mtllib model.mtl
v 0 0 0
v 1 0 0
v 0 1 0
usemtl red
f 1 2 3
";
        let path = obj_on_disk(obj, Some(("model.mtl", "newmtl red\nKd 1 0 0\n")));
        let mesh =
            load_obj_at(&path, &std::fs::read_to_string(&path).unwrap()).expect("triangle parses");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert!(mesh.has_vertex_colors());
        assert_eq!(mesh.colors[0], [1.0, 0.0, 0.0]);
    }

    /// One position shared by two materials cannot be one colour, so it is
    /// split the way a shared normal is — the same position appears twice,
    /// once per colour.
    #[test]
    fn a_corner_used_with_two_materials_is_split() {
        let obj = "\
mtllib model.mtl
v 0 0 0
v 1 0 0
v 0 1 0
v 0 0 1
usemtl red
f 1 2 3
usemtl green
f 1 4 2
";
        let path = obj_on_disk(
            obj,
            Some((
                "model.mtl",
                "newmtl red\nKd 1 0 0\nnewmtl green\nKd 0 1 0\n",
            )),
        );
        let mesh =
            load_obj_at(&path, &std::fs::read_to_string(&path).unwrap()).expect("two faces parse");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(mesh.triangle_count(), 2);
        // Position 0 is drawn red by one face and green by the other, so it
        // exists twice; positions 1..3 are single-material.
        let reds = mesh
            .colors
            .iter()
            .filter(|c| **c == [1.0, 0.0, 0.0])
            .count();
        let greens = mesh
            .colors
            .iter()
            .filter(|c| **c == [0.0, 1.0, 0.0])
            .count();
        assert_eq!(reds, 3);
        assert_eq!(greens, 3);
        assert_eq!(mesh.vertex_count(), 6);
    }

    /// A usemtl whose library never made it to the disk paints white rather
    /// than failing the model, and a bare OBJ with no material at all keeps
    /// its colours empty.
    #[test]
    fn a_missing_library_leaves_white_and_a_bare_obj_stays_uncoloured() {
        let obj = "\
mtllib gone.mtl
v 0 0 0
v 1 0 0
v 0 1 0
usemtl red
f 1 2 3
";
        let path = obj_on_disk(obj, None);
        let mesh =
            load_obj_at(&path, &std::fs::read_to_string(&path).unwrap()).expect("triangle parses");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert!(mesh.has_vertex_colors());
        assert_eq!(mesh.colors[0], MISSING_MATERIAL);

        let bare = load_obj("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").expect("bare parses");
        assert!(!bare.has_vertex_colors());
    }
}
