//! Wavefront OBJ parser.
//!
//! Hand-written: the format is simple enough that a dependency-free parser
//! is shorter and more predictable than pulling in `tobj`. Faces are
//! fan-triangulated, so quads and n-gons work; texture coordinates and
//! materials are ignored.
//!
//! Normals are used only when *every* face corner carries one — a partial set
//! would shade the wrong corners, so the renderer falls back to flat shading
//! instead. When they are used, a position that appears with more than one
//! normal is split into one vertex per (position, normal) pair: that is what
//! an OBJ means by it, and a cube with per-face normals (the usual Blender
//! export) has eight positions but twenty-four such pairs. Merging them by
//! position instead — keeping whichever normal came first — smooths a
//! faceted model into a pillow.

use std::collections::HashMap;

use super::types::Mesh;

/// Parse a Wavefront OBJ. Faces are fan-triangulated, so quads and n-gons
/// work; texture coordinates and materials are ignored.
pub fn load_obj(text: &str) -> Result<Mesh, String> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    // Per-face normal indices, resolved into per-vertex normals at the end.
    let mut normal_ids: Vec<Vec<u32>> = Vec::new();

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
                }
            }
            _ => {}
        }
    }

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
        return Mesh::finish(positions, Vec::new(), Vec::new(), triangles)
            .ok_or_else(|| "the OBJ file contains no triangles".to_string());
    }

    let mut split_positions: Vec<[f32; 3]> = Vec::new();
    let mut split_normals: Vec<[f32; 3]> = Vec::new();
    let mut seen: HashMap<(u32, u32), u32> = HashMap::new();
    let mut split_triangles: Vec<[u32; 3]> = Vec::with_capacity(triangles.len());
    for (triangle, ids) in triangles.iter().zip(normal_ids.iter()) {
        let mut remapped = [0u32; 3];
        for slot in 0..3 {
            let position = triangle[slot];
            let normal = ids[slot];
            remapped[slot] = *seen.entry((position, normal)).or_insert_with(|| {
                split_positions.push(positions[position as usize]);
                split_normals.push(normals[normal as usize]);
                (split_positions.len() - 1) as u32
            });
        }
        split_triangles.push(remapped);
    }

    Mesh::finish(split_positions, split_normals, Vec::new(), split_triangles)
        .ok_or_else(|| "the OBJ file contains no triangles".to_string())
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
}
