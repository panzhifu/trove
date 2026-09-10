//! Triangle-mesh loading for 3D model previews.
//!
//! Three text/binary formats are supported, all parsed in pure Rust with no
//! new dependencies: Wavefront OBJ, STL (ASCII and binary) and PLY (ASCII and
//! binary little/big endian). Everything is normalised into one [`Mesh`] so
//! the renderer never has to know where the triangles came from.
//!
//! Parsing is deliberately lenient: unknown lines, extra vertex properties
//! and unsupported primitives are skipped rather than failing the load. Only
//! a file that yields no triangle at all is an error.

use std::path::Path;

/// Axis-aligned bounds of a mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds {
    /// Bounds of an empty mesh.
    fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    /// Whether any vertex was folded in.
    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }

    fn extend(&mut self, p: [f32; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
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

/// A triangle mesh in model space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    /// Per-vertex normals; empty (or a different length than `positions`)
    /// means the file carries none and the renderer shades per face.
    pub normals: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
    pub bounds: Bounds,
}

impl Default for Bounds {
    fn default() -> Self {
        Self::empty()
    }
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

    /// Whether the model has usable per-vertex normals.
    pub fn has_vertex_normals(&self) -> bool {
        self.normals.len() == self.positions.len() && !self.normals.is_empty()
    }

    /// Build a mesh from raw positions/triangles, dropping degenerate
    /// triangles and recomputing the bounds. Returns `None` when nothing
    /// usable is left.
    fn finish(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        if positions.is_empty() || triangles.is_empty() {
            return None;
        }
        let count = positions.len() as u32;
        let triangles: Vec<[u32; 3]> = triangles
            .into_iter()
            .filter(|t| t[0] < count && t[1] < count && t[2] < count)
            .filter(|t| t[0] != t[1] && t[1] != t[2] && t[0] != t[2])
            .collect();
        if triangles.is_empty() {
            return None;
        }
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
        Some(Self {
            positions,
            normals,
            triangles,
            bounds,
        })
    }
}

/// Extensions this module can parse (lowercase, without the dot).
pub const MODEL_EXTENSIONS: [&str; 3] = ["obj", "stl", "ply"];

/// Whether `ext` (lowercase, without the dot) is a mesh format.
pub fn is_model_ext(ext: &str) -> bool {
    MODEL_EXTENSIONS.contains(&ext)
}

/// Load a mesh, dispatching on the file extension.
pub fn load(path: &Path) -> Result<Mesh, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !is_model_ext(&ext) {
        return Err(format!("unsupported model format: .{ext}"));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    match ext.as_str() {
        "obj" => {
            let text = String::from_utf8_lossy(&bytes);
            load_obj(&text)
        }
        "stl" => load_stl(&bytes),
        "ply" => load_ply(&bytes),
        _ => Err(format!("unsupported model format: .{ext}")),
    }
}

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
                let corners: Vec<(u32, u32)> = parts
                    .map(|token| resolve_obj_corner(token, positions.len(), normals.len()))
                    .collect();
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

    // Expand per-face normal indices into per-vertex normals when every face
    // carried one (otherwise the renderer shades flat).
    let vertex_normals = if normal_ids.len() == triangles.len()
        && normal_ids
            .iter()
            .all(|ids| ids.len() == 3 && ids.iter().all(|id| *id != u32::MAX))
        && !normals.is_empty()
    {
        let mut out = vec![[0.0f32; 3]; positions.len()];
        let mut seen = vec![false; positions.len()];
        for (tri, ids) in triangles.iter().zip(normal_ids.iter()) {
            for (slot, id) in tri.iter().zip(ids.iter()) {
                let idx = *slot as usize;
                if !seen[idx] {
                    out[idx] = normals[*id as usize];
                    seen[idx] = true;
                }
            }
        }
        out
    } else {
        Vec::new()
    };

    Mesh::finish(positions, vertex_normals, triangles)
        .ok_or_else(|| "the OBJ file contains no triangles".to_string())
}

/// Resolve one `f` corner (`v`, `v/vt`, `v//vn` or `v/vt/vn`) into a
/// (position, normal) index pair; OBJ indices are 1-based and may be
/// negative (relative to the end). `u32::MAX` marks "no normal".
fn resolve_obj_corner(token: &str, position_count: usize, normal_count: usize) -> (u32, u32) {
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
    let position = resolve(v, position_count).unwrap_or(0);
    let normal = n.and_then(|n| resolve(n, normal_count)).unwrap_or(u32::MAX);
    (position, normal)
}

/// Parse an STL file, detecting the ASCII and binary layouts.
pub fn load_stl(bytes: &[u8]) -> Result<Mesh, String> {
    if let Some(count) = binary_stl_triangle_count(bytes) {
        return load_stl_binary(bytes, count);
    }
    let text = String::from_utf8_lossy(bytes);
    if text.trim_start().to_ascii_lowercase().starts_with("solid") {
        return load_stl_ascii(&text);
    }
    Err("not a readable STL file".into())
}

/// Triangle count of a binary STL, or `None` when the size does not match
/// the fixed 84-byte header + 50-byte triangle layout.
fn binary_stl_triangle_count(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 84 {
        return None;
    }
    let declared = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let expected = 84usize.checked_add(declared.checked_mul(50)?)?;
    (expected == bytes.len()).then_some(declared)
}

/// Parse the fixed binary STL layout: 80-byte header, `u32` count, then
/// 12 `f32` (facet normal + three vertices) and a `u16` attribute per face.
fn load_stl_binary(bytes: &[u8], count: usize) -> Result<Mesh, String> {
    let mut positions = Vec::with_capacity(count * 3);
    let mut triangles = Vec::with_capacity(count);
    for i in 0..count {
        let base = 84 + i * 50 + 12; // skip the facet normal
        for corner in 0..3 {
            let offset = base + corner * 12;
            positions.push([
                f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()),
                f32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()),
                f32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()),
            ]);
        }
        let first = (i * 3) as u32;
        triangles.push([first, first + 1, first + 2]);
    }
    Mesh::finish(positions, Vec::new(), triangles)
        .ok_or_else(|| "the STL file contains no triangles".to_string())
}

/// Parse the ASCII STL layout: three `vertex x y z` lines per `facet normal`.
pub fn load_stl_ascii(text: &str) -> Result<Mesh, String> {
    let mut positions = Vec::new();
    let mut triangles = Vec::new();
    let mut pending: Vec<[f32; 3]> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if !matches!(parts.next(), Some("vertex") | Some("Vertex")) {
            continue;
        }
        let values = parse_floats(parts.take(3));
        if values.len() == 3 {
            pending.push([values[0], values[1], values[2]]);
            if pending.len() == 3 {
                let first = positions.len() as u32;
                positions.extend(pending.drain(..));
                triangles.push([first, first + 1, first + 2]);
            }
        }
    }
    Mesh::finish(positions, Vec::new(), triangles)
        .ok_or_else(|| "the STL file contains no triangles".to_string())
}

/// One PLY property declaration.
struct PlyProperty {
    name: String,
    /// Scalar type, or the item type of a list.
    ty: PlyType,
    /// Element type of a list property (count type in front of the values).
    count_ty: Option<PlyType>,
}

/// PLY scalar types, mapped to their byte width and conversion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlyType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl PlyType {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }

    fn width(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

/// PLY byte order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlyEndian {
    Little,
    Big,
}

/// One `element` block of a PLY header.
struct PlyElement {
    name: String,
    count: usize,
    properties: Vec<PlyProperty>,
}

/// Parse a PLY file (ASCII or binary, either endianness): reads the header,
/// pulls `x y z` (+ optional `nx ny nz`) from the vertex element and the
/// index list of the face element, then fan-triangulates.
pub fn load_ply(bytes: &[u8]) -> Result<Mesh, String> {
    let header_end = find_ply_header_end(bytes).ok_or("not a PLY file")?;
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header.lines().map(str::trim);
    if lines.next().map(|l| l.to_ascii_lowercase()) != Some("ply".to_string()) {
        return Err("not a PLY file".into());
    }

    let mut format = String::new();
    let mut elements: Vec<PlyElement> = Vec::new();
    for line in lines {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("format") => format = parts.next().unwrap_or("").to_ascii_lowercase(),
            Some("element") => {
                let name = parts.next().unwrap_or("").to_ascii_lowercase();
                let count = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                elements.push(PlyElement {
                    name,
                    count,
                    properties: Vec::new(),
                });
            }
            Some("property") => {
                let Some(element) = elements.last_mut() else {
                    continue;
                };
                let first = parts.next().unwrap_or("");
                if first == "list" {
                    let count_ty = PlyType::parse(parts.next().unwrap_or(""));
                    let ty = PlyType::parse(parts.next().unwrap_or(""));
                    let name = parts.next().unwrap_or("").to_ascii_lowercase();
                    if let (Some(count_ty), Some(ty)) = (count_ty, ty) {
                        element.properties.push(PlyProperty {
                            name,
                            ty,
                            count_ty: Some(count_ty),
                        });
                    }
                } else if let Some(ty) = PlyType::parse(first) {
                    element.properties.push(PlyProperty {
                        name: parts.next().unwrap_or("").to_ascii_lowercase(),
                        ty,
                        count_ty: None,
                    });
                }
            }
            _ => {}
        }
    }

    let endian = match format.as_str() {
        "ascii" => None,
        "binary_little_endian" => Some(PlyEndian::Little),
        "binary_big_endian" => Some(PlyEndian::Big),
        other => return Err(format!("unsupported PLY format: {other}")),
    };

    let mut positions = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut triangles = Vec::new();
    let body = &bytes[header_end..];
    let mut binary_cursor = std::io::Cursor::new(body);
    let ascii_body = String::from_utf8_lossy(body);
    let mut tokens = ascii_body.split_whitespace();

    for element in &elements {
        let is_vertex = element.name == "vertex";
        let is_face = element.name == "face";
        for _ in 0..element.count {
            let mut values: Vec<(String, Vec<f64>)> = Vec::with_capacity(element.properties.len());
            for property in &element.properties {
                let mut read = |endian: Option<PlyEndian>| -> Option<Vec<f64>> {
                    match endian {
                        None => {
                            let count = property.count_ty.map(|_| 1).unwrap_or(1);
                            let mut out = Vec::with_capacity(count);
                            for _ in 0..count {
                                out.push(tokens.next()?.parse::<f64>().ok()?);
                            }
                            if property.count_ty.is_some() {
                                // A list: the first token was the length.
                                let length = out[0].max(0.0) as usize;
                                let mut items = Vec::with_capacity(length);
                                for _ in 0..length {
                                    items.push(tokens.next()?.parse::<f64>().ok()?);
                                }
                                return Some(items);
                            }
                            Some(out)
                        }
                        Some(order) => {
                            if let Some(counter) = property.count_ty {
                                let length =
                                    read_scalar(&mut binary_cursor, counter, order)? as usize;
                                let mut items = Vec::with_capacity(length);
                                for _ in 0..length {
                                    items.push(read_scalar(
                                        &mut binary_cursor,
                                        property.ty,
                                        order,
                                    )?);
                                }
                                Some(items)
                            } else {
                                Some(vec![read_scalar(&mut binary_cursor, property.ty, order)?])
                            }
                        }
                    }
                };
                let Some(v) = read(endian) else {
                    return Err("the PLY body ends early".into());
                };
                values.push((property.name.clone(), v));
            }
            let scalar = |name: &str| -> Option<f32> {
                values
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, v)| v.first())
                    .map(|v| *v as f32)
            };
            if is_vertex {
                if let (Some(x), Some(y), Some(z)) = (scalar("x"), scalar("y"), scalar("z")) {
                    positions.push([x, y, z]);
                    if let (Some(nx), Some(ny), Some(nz)) =
                        (scalar("nx"), scalar("ny"), scalar("nz"))
                    {
                        normals.push([nx, ny, nz]);
                    }
                }
            } else if is_face {
                let indices: Vec<u32> = values
                    .iter()
                    .filter(|(name, _)| name == "vertex_indices" || name == "vertex_index")
                    .flat_map(|(_, v)| v.iter().map(|i| *i as u32))
                    .collect();
                for i in 1..indices.len().saturating_sub(1) {
                    triangles.push([indices[0], indices[i], indices[i + 1]]);
                }
            }
        }
    }

    Mesh::finish(positions, normals, triangles)
        .ok_or_else(|| "the PLY file contains no triangles".to_string())
}

/// Read one scalar value from a binary PLY body.
fn read_scalar(cursor: &mut std::io::Cursor<&[u8]>, ty: PlyType, order: PlyEndian) -> Option<f64> {
    use std::io::Read as _;
    let mut buf = [0u8; 8];
    let width = ty.width();
    cursor.read_exact(&mut buf[..width]).ok()?;
    let slice = &buf[..width];
    let be = order == PlyEndian::Big;
    Some(match ty {
        PlyType::I8 => i8::from_ne_bytes([slice[0]]) as f64,
        PlyType::U8 => slice[0] as f64,
        PlyType::I16 => {
            let b = [slice[0], slice[1]];
            (if be {
                i16::from_be_bytes(b)
            } else {
                i16::from_le_bytes(b)
            }) as f64
        }
        PlyType::U16 => {
            let b = [slice[0], slice[1]];
            (if be {
                u16::from_be_bytes(b)
            } else {
                u16::from_le_bytes(b)
            }) as f64
        }
        PlyType::I32 => {
            let b: [u8; 4] = slice.try_into().ok()?;
            (if be {
                i32::from_be_bytes(b)
            } else {
                i32::from_le_bytes(b)
            }) as f64
        }
        PlyType::U32 => {
            let b: [u8; 4] = slice.try_into().ok()?;
            (if be {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }) as f64
        }
        PlyType::F32 => {
            let b: [u8; 4] = slice.try_into().ok()?;
            (if be {
                f32::from_be_bytes(b)
            } else {
                f32::from_le_bytes(b)
            }) as f64
        }
        PlyType::F64 => {
            let b: [u8; 8] = slice.try_into().ok()?;
            if be {
                f64::from_be_bytes(b)
            } else {
                f64::from_le_bytes(b)
            }
        }
    })
}

/// Offset just past the `end_header` line.
fn find_ply_header_end(bytes: &[u8]) -> Option<usize> {
    let needle = b"end_header";
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            let mut end = index + needle.len();
            // Skip the rest of the line (CR/LF variants).
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            return Some((end + 1).min(bytes.len()));
        }
        index += 1;
    }
    None
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

    #[test]
    fn obj_without_geometry_is_an_error() {
        assert!(load_obj("# nothing here\nv 0 0 0\n").is_err());
    }

    #[test]
    fn ascii_stl_reads_vertices() {
        let stl = "\
solid test
  facet normal 0 0 1
    outer loop
      vertex 0 0 0
      vertex 1 0 0
      vertex 0 1 0
    endloop
  endfacet
endsolid test
";
        let mesh = load_stl(stl.as_bytes()).expect("ascii stl parses");
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.bounds.max, [1.0, 1.0, 0.0]);
        assert!(!mesh.has_vertex_normals());
    }

    /// Build a one-triangle binary STL the way exporters write it.
    fn binary_stl(triangle: [[f32; 3]; 3]) -> Vec<u8> {
        let mut out = vec![0u8; 80];
        out[..6].copy_from_slice(b"binary");
        out.extend_from_slice(&1u32.to_le_bytes());
        for _ in 0..3 {
            out.extend_from_slice(&0f32.to_le_bytes()); // facet normal
        }
        for vertex in triangle {
            for value in vertex {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // attribute byte count
        out
    }

    #[test]
    fn binary_stl_is_detected_by_its_size() {
        let bytes = binary_stl([[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 3.0, 0.0]]);
        assert_eq!(bytes.len(), 134);
        let mesh = load_stl(&bytes).expect("binary stl parses");
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.bounds.max, [2.0, 3.0, 0.0]);
    }

    #[test]
    fn ascii_ply_reads_vertices_and_faces() {
        let ply = "\
ply
format ascii 1.0
element vertex 3
property float x
property float y
property float z
element face 1
property list uchar int vertex_indices
end_header
0 0 0
1 0 0
0 1 0
3 0 1 2
";
        let mesh = load_ply(ply.as_bytes()).expect("ascii ply parses");
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.triangles[0], [0, 1, 2]);
    }

    #[test]
    fn binary_ply_is_read_with_the_declared_endianness() {
        let header = "\
ply
format binary_little_endian 1.0
element vertex 3
property float x
property float y
property float z
element face 1
property list uchar int vertex_indices
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        for vertex in [[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
            for value in vertex {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes.push(3);
        for index in [0i32, 1, 2] {
            bytes.extend_from_slice(&index.to_le_bytes());
        }
        let mesh = load_ply(&bytes).expect("binary ply parses");
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.bounds.max, [1.0, 1.0, 0.0]);
    }

    #[test]
    fn ply_skips_extra_properties_and_unknown_elements() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
property uchar red
property uchar green
property uchar blue
element face 0
property list uchar int vertex_indices
end_header
0 0 0 255 0 0
1 1 1 0 255 0
";
        // Two vertices but no face: nothing to render.
        assert!(load_ply(ply.as_bytes()).is_err());
    }

    #[test]
    fn model_extensions_are_recognised() {
        assert!(is_model_ext("obj") && is_model_ext("stl") && is_model_ext("ply"));
        assert!(!is_model_ext("png"));
    }
}
