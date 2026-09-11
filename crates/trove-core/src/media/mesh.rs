//! Geometry loading for 3D model previews.
//!
//! Three text/binary formats are supported, all parsed in pure Rust on top
//! of the raw file bytes: Wavefront OBJ, STL (ASCII and binary) and PLY
//! (ASCII and binary little/big endian). Everything is normalised into one
//! [`Mesh`] so the renderer never has to know where the triangles came from.
//!
//! PLY also arrives as a *point cloud* — a scan or a photogrammetry export
//! with no `face` element at all — which is why a [`Mesh`] may hold positions
//! without triangles. OBJ and STL stay triangle-only: their stray vertices are
//! a broken export rather than a cloud.
//!
//! A PLY may also carry a colour per vertex. Point clouds are where that
//! matters (a scan without its colours is grey noise), so the point renderer
//! uses them; a triangle mesh still shades from the material, because its GPU
//! layout carries no colour attribute.
//!
//! Parsing is deliberately lenient: unknown lines, extra vertex properties
//! and unsupported primitives are skipped rather than failing the load. Only
//! a file that yields no usable vertex at all is an error.

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

    /// Whether the model has usable per-vertex normals.
    pub fn has_vertex_normals(&self) -> bool {
        self.normals.len() == self.positions.len() && !self.normals.is_empty()
    }

    /// Whether the model has a usable colour per vertex.
    pub fn has_vertex_colors(&self) -> bool {
        self.colors.len() == self.positions.len() && !self.colors.is_empty()
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

/// Extensions this module can parse (lowercase, without the dot).
pub const MODEL_EXTENSIONS: [&str; 3] = ["obj", "stl", "ply"];

/// Whether `ext` (lowercase, without the dot) is a mesh format.
pub fn is_model_ext(ext: &str) -> bool {
    MODEL_EXTENSIONS.contains(&ext)
}

/// Ceiling on a model file read whole into memory: the parsed mesh costs a
/// multiple of the file, so a stray multi-gigabyte export must fail fast
/// instead of dragging the machine into swap.
const MAX_MODEL_FILE_BYTES: u64 = 2 << 30;

/// Load a mesh, dispatching on the file extension.
pub fn load(path: &Path) -> Result<Mesh, String> {
    load_capped(path, MAX_MODEL_FILE_BYTES)
}

/// [`load`] with an overridable size cap, so the limit itself can be tested.
fn load_capped(path: &Path, limit: u64) -> Result<Mesh, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !is_model_ext(&ext) {
        return Err(format!("unsupported model format: .{ext}"));
    }
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > limit {
        return Err(format!(
            "the model file is larger than the {} GiB preview limit",
            limit >> 30
        ));
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

    Mesh::finish(positions, vertex_normals, Vec::new(), triangles)
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
    Mesh::finish(positions, Vec::new(), Vec::new(), triangles)
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
                positions.append(&mut pending);
                triangles.push([first, first + 1, first + 2]);
            }
        }
    }
    Mesh::finish(positions, Vec::new(), Vec::new(), triangles)
        .ok_or_else(|| "the STL file contains no triangles".to_string())
}

/// One PLY property declaration.
pub(crate) struct PlyProperty {
    pub(crate) name: String,
    /// Scalar type, or the item type of a list.
    pub(crate) ty: PlyType,
    /// Element type of a list property (count type in front of the values).
    pub(crate) count_ty: Option<PlyType>,
}

/// PLY scalar types, mapped to their byte width and conversion.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlyType {
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
    pub(crate) fn parse(name: &str) -> Option<Self> {
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

    pub(crate) fn width(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Whether the type holds an integer. Colour channels are 0..=255 in
    /// every integer flavour but 0..=1 as floats, and the header is what says
    /// which one a file uses.
    fn is_integer(self) -> bool {
        !matches!(self, Self::F32 | Self::F64)
    }
}

/// PLY byte order.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlyEndian {
    Little,
    Big,
}

/// One `element` block of a PLY header.
pub(crate) struct PlyElement {
    pub(crate) name: String,
    pub(crate) count: usize,
    pub(crate) properties: Vec<PlyProperty>,
}

/// Error for a body shorter than the header promised.
pub(crate) const BODY_END: &str = "the PLY body ends early";

/// ASCII bodies below this size parse serially: handing lines to the thread
/// pool would cost more than the tokens do.
const ASCII_PARALLEL_MIN_BYTES: usize = 256 << 10;

/// `u8` colour channels pre-scaled to 0..=1 — the one integer width real
/// files use — so the hot loops never divide.
const U8_UNIT: [f32; 256] = {
    let mut table = [0.0f32; 256];
    let mut value = 0;
    while value < 256 {
        table[value] = (value as f64 / 255.0) as f32;
        value += 1;
    }
    table
};

/// The parts of a PLY body worth keeping, filled by the ASCII and binary
/// readers alike.
#[derive(Default)]
pub(crate) struct PlyBody {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
}

/// Where the values worth reading sit inside the `vertex` element: column
/// indices resolved once from the header, so the body loops index directly
/// instead of searching property names for every vertex.
pub(crate) struct VertexColumns {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) z: usize,
    pub(crate) normals: Option<[usize; 3]>,
    pub(crate) colors: Option<[usize; 3]>,
}

/// Resolve the vertex columns; `None` when the element carries no scalar
/// x/y/z at all. Normals and colours are all-or-nothing: one missing
/// channel drops the whole set rather than half-reading it.
pub(crate) fn vertex_columns(properties: &[PlyProperty]) -> Option<VertexColumns> {
    let scalar = |name: &str| {
        properties
            .iter()
            .position(|p| p.name == name && p.count_ty.is_none())
    };
    let triple = |names: [&str; 3]| -> Option<[usize; 3]> {
        Some([scalar(names[0])?, scalar(names[1])?, scalar(names[2])?])
    };
    Some(VertexColumns {
        x: scalar("x")?,
        y: scalar("y")?,
        z: scalar("z")?,
        normals: triple(["nx", "ny", "nz"]),
        // `red green blue` is the spec spelling; MeshLab and CloudCompare
        // write `diffuse_*` often enough to take both.
        colors: triple(["red", "green", "blue"])
            .or_else(|| triple(["diffuse_red", "diffuse_green", "diffuse_blue"])),
    })
}

/// Whether a face property holds the polygon index list.
pub(crate) fn is_indices(name: &str) -> bool {
    name == "vertex_indices" || name == "vertex_index"
}

/// Parse a PLY file (ASCII or binary, either endianness): reads the header,
/// resolves the interesting properties to column indices once, then pulls
/// the vertex element and the face index lists straight into typed arrays —
/// binary vertex rows through their fixed stride, ASCII lines in parallel
/// chunks. A file with no `face` element becomes a point cloud.
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

    let body = &bytes[header_end..];
    let parsed = match endian {
        Some(order) => load_ply_binary(body, &elements, order)?,
        None => load_ply_ascii(body, &elements)?,
    };

    // A `face` element is what separates a surface from a cloud; a header
    // that declares one but yields no usable triangle is still an error.
    let declared_faces = elements.iter().any(|element| element.name == "face");
    if declared_faces {
        Mesh::finish(
            parsed.positions,
            parsed.normals,
            parsed.colors,
            parsed.triangles,
        )
        .ok_or_else(|| "the PLY file contains no triangles".to_string())
    } else {
        Mesh::finish_points(parsed.positions, parsed.normals, parsed.colors)
            .ok_or_else(|| "the PLY file contains no vertices".to_string())
    }
}

/// Decode a binary body. Fixed-size vertex rows go through their stride in
/// one bounds check per row; every list-bearing element — faces above all —
/// is walked row by row.
fn load_ply_binary(
    body: &[u8],
    elements: &[PlyElement],
    order: PlyEndian,
) -> Result<PlyBody, String> {
    let mut out = PlyBody::default();
    let mut cursor = 0usize;
    for element in elements {
        let fixed = element.properties.iter().all(|p| p.count_ty.is_none());
        let columns = (element.name == "vertex")
            .then(|| vertex_columns(&element.properties))
            .flatten();
        if element.name == "vertex"
            && fixed
            && let Some(cols) = &columns
        {
            read_binary_vertices(body, &mut cursor, element, cols, order, &mut out)?;
            continue;
        }
        if element.name == "face" || element.name == "vertex" || !fixed {
            let indices_col = (element.name == "face")
                .then(|| {
                    element
                        .properties
                        .iter()
                        .position(|p| p.count_ty.is_some() && is_indices(&p.name))
                })
                .flatten();
            read_binary_rows(
                body,
                &mut cursor,
                element,
                order,
                indices_col,
                columns.as_ref(),
                &mut out,
            )?;
        } else {
            // Unknown fixed-size element: jump over its records whole.
            let stride: usize = element.properties.iter().map(|p| p.ty.width()).sum();
            let size = element.count.checked_mul(stride).ok_or(BODY_END)?;
            cursor = cursor
                .checked_add(size)
                .filter(|end| *end <= body.len())
                .ok_or(BODY_END)?;
        }
    }
    Ok(out)
}

/// The hot loop of every binary point cloud: rows of a fixed stride, values
/// read at offsets computed from the header, nothing allocated per vertex.
#[allow(clippy::too_many_arguments)]
fn read_binary_vertices(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    order: PlyEndian,
    out: &mut PlyBody,
) -> Result<(), String> {
    let properties = &element.properties;
    let stride: usize = properties.iter().map(|p| p.ty.width()).sum();
    // The whole element must lie inside the body: a header lying about its
    // count fails here instead of sizing an array from the lie.
    let end = (*cursor)
        .checked_add(element.count.checked_mul(stride).ok_or(BODY_END)?)
        .filter(|end| *end <= body.len())
        .ok_or(BODY_END)?;
    let offset = |col: usize| {
        properties[..col]
            .iter()
            .map(|p| p.ty.width())
            .sum::<usize>()
    };
    let ty = |col: usize| properties[col].ty;
    let (x, y, z) = (
        (offset(cols.x), ty(cols.x)),
        (offset(cols.y), ty(cols.y)),
        (offset(cols.z), ty(cols.z)),
    );
    let normals = cols.normals.map(|n| n.map(|c| (offset(c), ty(c))));
    let colors = cols.colors.map(|n| n.map(|c| (offset(c), ty(c))));

    out.positions.reserve(element.count);
    if cols.normals.is_some() {
        out.normals.reserve(element.count);
    }
    if cols.colors.is_some() {
        out.colors.reserve(element.count);
    }
    for row in body[*cursor..end].chunks_exact(stride) {
        // Row length is exactly `stride` and every column offset plus width
        // fits it, so the reads below are in bounds by construction.
        out.positions.push([
            scalar_row(row, x.0, x.1, order) as f32,
            scalar_row(row, y.0, y.1, order) as f32,
            scalar_row(row, z.0, z.1, order) as f32,
        ]);
        if let Some(n) = normals {
            out.normals.push([
                scalar_row(row, n[0].0, n[0].1, order) as f32,
                scalar_row(row, n[1].0, n[1].1, order) as f32,
                scalar_row(row, n[2].0, n[2].1, order) as f32,
            ]);
        }
        if let Some(c) = colors {
            out.colors.push([
                color_row(row, c[0].0, c[0].1, order),
                color_row(row, c[1].0, c[1].1, order),
                color_row(row, c[2].0, c[2].1, order),
            ]);
        }
    }
    *cursor = end;
    Ok(())
}

/// Row-by-row walk for everything the fixed-stride path cannot take: faces
/// for their index list, vertex elements carrying list properties, unknown
/// elements just to advance past them.
pub(crate) fn read_binary_rows(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    order: PlyEndian,
    indices_col: Option<usize>,
    columns: Option<&VertexColumns>,
    out: &mut PlyBody,
) -> Result<(), String> {
    let mut indices: Vec<u32> = Vec::new();
    let mut scalars = vec![0.0f64; element.properties.len()];
    for _ in 0..element.count {
        indices.clear();
        for (col, property) in element.properties.iter().enumerate() {
            match property.count_ty {
                Some(count_ty) => {
                    let length =
                        scalar_at(body, *cursor, count_ty, order).ok_or(BODY_END)? as usize;
                    *cursor += count_ty.width();
                    if Some(col) == indices_col {
                        for _ in 0..length {
                            indices.push(
                                scalar_at(body, *cursor, property.ty, order).ok_or(BODY_END)?
                                    as u32,
                            );
                            *cursor += property.ty.width();
                        }
                    } else {
                        let skip = length.checked_mul(property.ty.width()).ok_or(BODY_END)?;
                        *cursor = (*cursor)
                            .checked_add(skip)
                            .filter(|at| *at <= body.len())
                            .ok_or(BODY_END)?;
                    }
                }
                None => {
                    scalars[col] = scalar_at(body, *cursor, property.ty, order).ok_or(BODY_END)?;
                    *cursor += property.ty.width();
                }
            }
        }
        if let Some(cols) = columns {
            push_vertex_record(out, cols, &element.properties, &scalars);
        }
        if indices_col.is_some() {
            push_fan(&mut out.triangles, &indices);
        }
    }
    Ok(())
}

/// Decode an ASCII body, record per line: the vertex element is cut into
/// line chunks parsed on the rayon pool, everything else walks serially.
fn load_ply_ascii(body: &[u8], elements: &[PlyElement]) -> Result<PlyBody, String> {
    let mut out = PlyBody::default();
    let mut cursor = 0usize;
    for element in elements {
        let fixed = element.properties.iter().all(|p| p.count_ty.is_none());
        if element.name == "vertex"
            && fixed
            && let Some(cols) = vertex_columns(&element.properties)
        {
            read_ascii_vertices(body, &mut cursor, element, &cols, &mut out)?;
            continue;
        }
        let indices_col = (element.name == "face")
            .then(|| {
                element
                    .properties
                    .iter()
                    .position(|p| p.count_ty.is_some() && is_indices(&p.name))
            })
            .flatten();
        let columns = (element.name == "vertex")
            .then(|| vertex_columns(&element.properties))
            .flatten();
        read_ascii_rows(
            body,
            &mut cursor,
            element,
            indices_col,
            columns.as_ref(),
            &mut out,
        )?;
    }
    Ok(out)
}

/// ASCII vertex element without list properties: records are whole lines, so
/// the section is cut at newline boundaries, the chunks parsed in parallel
/// and concatenated in file order.
fn read_ascii_vertices(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    out: &mut PlyBody,
) -> Result<(), String> {
    if element.count == 0 {
        return Ok(());
    }
    // Fast section end: the records are the first `count` newline-terminated
    // runs, so the `count`-th newline closes the section — one vectorised
    // scan instead of walking every line. The parsed record sum verifies it;
    // a blank line between records (or a last record with no trailing
    // newline) shifts the section, and the exact line scanner then redoes it.
    // Real exports never take that path.
    let fast_end = memchr::memchr_iter(b'\n', &body[*cursor..])
        .nth(element.count - 1)
        .map(|at| *cursor + at + 1);
    let (end, parsed) = match fast_end {
        Some(end) => match parse_vertex_section(&body[*cursor..end], element, cols)? {
            parsed if parsed.records == element.count => (end, parsed),
            _ => exact_vertex_section(body, *cursor, element, cols)?,
        },
        None => exact_vertex_section(body, *cursor, element, cols)?,
    };
    *cursor = end;
    out.positions.extend_from_slice(&parsed.body.positions);
    out.normals.extend_from_slice(&parsed.body.normals);
    out.colors.extend_from_slice(&parsed.body.colors);
    Ok(())
}

/// Exact section end + parse, for sections the newline count cannot size:
/// blank lines between records, or a last record with no trailing newline.
fn exact_vertex_section(
    body: &[u8],
    start: usize,
    element: &PlyElement,
    cols: &VertexColumns,
) -> Result<(usize, ParsedSection), String> {
    let mut scan = start;
    for _ in 0..element.count {
        next_record_line(body, &mut scan).ok_or(BODY_END)?;
    }
    let parsed = parse_vertex_section(&body[start..scan], element, cols)?;
    Ok((scan, parsed))
}

/// One vertex section's parse: how many records it yielded and the arrays.
struct ParsedSection {
    records: usize,
    body: PlyBody,
}

/// Cut a whole-line section into chunks and parse them — serially for small
/// sections, on the rayon pool otherwise.
fn parse_vertex_section(
    section: &[u8],
    element: &PlyElement,
    cols: &VertexColumns,
) -> Result<ParsedSection, String> {
    if section.len() < ASCII_PARALLEL_MIN_BYTES || element.count < 2 {
        return parse_ascii_vertex_chunk(section, &element.properties, cols);
    }
    use rayon::prelude::*;

    let chunks = rayon::current_num_threads().min(element.count).max(1);
    // Chunk starts on line boundaries: split the section by bytes, then snap
    // every interior boundary forward to the next newline. Records per chunk
    // may differ by a line or two; the section end fixes the total.
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(chunks);
    let mut start = 0usize;
    for k in 1..=chunks {
        let end = if k == chunks {
            section.len()
        } else {
            snap_to_line_start(section, k * section.len() / chunks)
        };
        ranges.push((start, end));
        start = end;
    }
    let parts = ranges
        .par_iter()
        .map(|&(a, b)| parse_ascii_vertex_chunk(&section[a..b], &element.properties, cols))
        .collect::<Result<Vec<_>, String>>()?;
    let mut parsed = ParsedSection {
        records: 0,
        body: PlyBody::default(),
    };
    for part in parts {
        parsed.records += part.records;
        parsed
            .body
            .positions
            .extend_from_slice(&part.body.positions);
        parsed.body.normals.extend_from_slice(&part.body.normals);
        parsed.body.colors.extend_from_slice(&part.body.colors);
    }
    Ok(parsed)
}

/// Advance `at` to the start of the next line (just past a newline), so a
/// chunk cut by byte offset still begins on a record boundary.
fn snap_to_line_start(section: &[u8], at: usize) -> usize {
    if at == 0 || section[at - 1] == b'\n' {
        return at;
    }
    section[at..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(section.len(), |i| at + i + 1)
}

/// Parse a run of whole vertex lines into its own arrays. Every non-empty
/// line is one record; columns past the last interesting one are ignored.
fn parse_ascii_vertex_chunk(
    chunk: &[u8],
    properties: &[PlyProperty],
    cols: &VertexColumns,
) -> Result<ParsedSection, String> {
    let mut part = PlyBody::default();
    // What each column feeds: 1-3 the position, 4-6 the normal, 7-9 the
    // colour channel, 0 nothing worth a parse.
    let last_col = [cols.x, cols.y, cols.z]
        .into_iter()
        .chain(cols.normals.into_iter().flatten())
        .chain(cols.colors.into_iter().flatten())
        .max()
        .unwrap_or(0);
    let mut plan = vec![0u8; last_col + 1];
    plan[cols.x] = 1;
    plan[cols.y] = 2;
    plan[cols.z] = 3;
    if let Some([a, b, c]) = cols.normals {
        plan[a] = 4;
        plan[b] = 5;
        plan[c] = 6;
    }
    if let Some([r, g, b]) = cols.colors {
        plan[r] = 7;
        plan[g] = 8;
        plan[b] = 9;
    }

    let mut at = 0usize;
    while at < chunk.len() {
        // Plain byte scan: memchr's per-call setup costs more than the loop
        // on lines this short.
        let end = chunk[at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(chunk.len(), |i| at + i);
        let line = &chunk[at..end];
        at = (end + 1).min(chunk.len());
        let mut tokens = Tokens::new(line);
        let Some(first) = tokens.next_token() else {
            continue; // a blank line between records
        };
        let mut record = [0.0f64; 9];
        let mut column = 0usize;
        let mut token = Some(first);
        while let Some(value) = token {
            if column > last_col {
                break;
            }
            let slot = plan[column];
            if slot != 0 {
                record[(slot - 1) as usize] = token_f64(value).ok_or(BODY_END)?;
            }
            column += 1;
            token = tokens.next_token();
        }
        if column <= last_col {
            return Err(BODY_END.into());
        }
        part.positions
            .push([record[0] as f32, record[1] as f32, record[2] as f32]);
        if cols.normals.is_some() {
            part.normals
                .push([record[3] as f32, record[4] as f32, record[5] as f32]);
        }
        if let Some([r, g, b]) = cols.colors {
            part.colors.push([
                unit_colour(record[6], properties[r].ty),
                unit_colour(record[7], properties[g].ty),
                unit_colour(record[8], properties[b].ty),
            ]);
        }
    }
    Ok(ParsedSection {
        records: part.positions.len(),
        body: part,
    })
}

/// Row-by-row ASCII walk for everything the parallel path cannot take: faces
/// for their index list, vertex elements carrying list properties, unknown
/// elements to skip their records.
fn read_ascii_rows(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    indices_col: Option<usize>,
    columns: Option<&VertexColumns>,
    out: &mut PlyBody,
) -> Result<(), String> {
    let mut indices: Vec<u32> = Vec::new();
    let mut scalars = vec![0.0f64; element.properties.len()];
    for _ in 0..element.count {
        let line = next_record_line(body, cursor).ok_or(BODY_END)?;
        let mut tokens = Tokens::new(line);
        indices.clear();
        for (col, property) in element.properties.iter().enumerate() {
            match property.count_ty {
                Some(_) => {
                    // A list: the first token is its length.
                    let length = token_f64(tokens.next_token().ok_or(BODY_END)?)
                        .ok_or(BODY_END)?
                        .max(0.0) as usize;
                    if Some(col) == indices_col {
                        for _ in 0..length {
                            let index = token_f64(tokens.next_token().ok_or(BODY_END)?)
                                .ok_or(BODY_END)? as u32;
                            indices.push(index);
                        }
                    } else {
                        for _ in 0..length {
                            tokens.next_token().ok_or(BODY_END)?;
                        }
                    }
                }
                None => {
                    scalars[col] =
                        token_f64(tokens.next_token().ok_or(BODY_END)?).ok_or(BODY_END)?;
                }
            }
        }
        if let Some(cols) = columns {
            push_vertex_record(out, cols, &element.properties, &scalars);
        }
        if indices_col.is_some() {
            push_fan(&mut out.triangles, &indices);
        }
    }
    Ok(())
}

/// Advance to the next non-empty line and return it without its terminator;
/// blank lines separate records, they are not records.
pub(crate) fn next_record_line<'a>(body: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    while *cursor < body.len() {
        let rest = &body[*cursor..];
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        *cursor = (*cursor + end + 1).min(body.len());
        let line = &rest[..end];
        if !line.iter().all(|&b| b.is_ascii_whitespace()) {
            return Some(line);
        }
    }
    None
}

/// Whitespace-separated tokens of one ASCII record line, read as bytes —
/// the body is never copied into a `String` to split it.
pub(crate) struct Tokens<'a> {
    line: &'a [u8],
    at: usize,
}

impl<'a> Tokens<'a> {
    #[inline]
    pub(crate) fn new(line: &'a [u8]) -> Self {
        Self { line, at: 0 }
    }

    #[inline]
    pub(crate) fn next_token(&mut self) -> Option<&'a [u8]> {
        while self.at < self.line.len() && self.line[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        let start = self.at;
        while self.at < self.line.len() && !self.line[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        (start < self.at).then(|| &self.line[start..self.at])
    }
}

/// Parse one ASCII token as a number; real files hold plain decimals, and
/// the check costs far less than the copy a lossy UTF-8 pass would make.
#[inline]
pub(crate) fn token_f64(token: &[u8]) -> Option<f64> {
    std::str::from_utf8(token).ok()?.parse().ok()
}

/// Read one scalar at a fixed offset of a bounds-checked record. The caller
/// guarantees `at + width ≤ record.len()` — the stride loop does, because
/// the offsets come from the header and sum within the row — so the reads
/// index directly instead of shipping an `Option` per value.
pub(crate) fn scalar_row(record: &[u8], at: usize, ty: PlyType, order: PlyEndian) -> f64 {
    match (ty, order) {
        (PlyType::I8, _) => record[at] as i8 as f64,
        (PlyType::U8, _) => record[at] as f64,
        (PlyType::I16, PlyEndian::Little) => {
            i16::from_le_bytes(record[at..at + 2].try_into().unwrap()) as f64
        }
        (PlyType::I16, PlyEndian::Big) => {
            i16::from_be_bytes(record[at..at + 2].try_into().unwrap()) as f64
        }
        (PlyType::U16, PlyEndian::Little) => {
            u16::from_le_bytes(record[at..at + 2].try_into().unwrap()) as f64
        }
        (PlyType::U16, PlyEndian::Big) => {
            u16::from_be_bytes(record[at..at + 2].try_into().unwrap()) as f64
        }
        (PlyType::I32, PlyEndian::Little) => {
            i32::from_le_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::I32, PlyEndian::Big) => {
            i32::from_be_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::U32, PlyEndian::Little) => {
            u32::from_le_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::U32, PlyEndian::Big) => {
            u32::from_be_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::F32, PlyEndian::Little) => {
            f32::from_le_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::F32, PlyEndian::Big) => {
            f32::from_be_bytes(record[at..at + 4].try_into().unwrap()) as f64
        }
        (PlyType::F64, PlyEndian::Little) => {
            f64::from_le_bytes(record[at..at + 8].try_into().unwrap())
        }
        (PlyType::F64, PlyEndian::Big) => {
            f64::from_be_bytes(record[at..at + 8].try_into().unwrap())
        }
    }
}

/// [`scalar_row`] for a record whose bounds are not yet proven: one range
/// check in front of the read.
pub(crate) fn scalar_at(record: &[u8], at: usize, ty: PlyType, order: PlyEndian) -> Option<f64> {
    record.get(at..at + ty.width())?;
    Some(scalar_row(record, at, ty, order))
}

/// A colour channel straight out of a binary row: `u8` goes through the
/// pre-scaled table, every other width through the generic scalar.
pub(crate) fn color_row(row: &[u8], at: usize, ty: PlyType, order: PlyEndian) -> f32 {
    match ty {
        PlyType::U8 => U8_UNIT[row[at] as usize],
        _ => unit_colour(scalar_row(row, at, ty, order), ty),
    }
}

/// One colour channel: 0..=255 in an integer field, 0..=1 as a float — the
/// declared type travels with the header — clamped so a broken file still
/// renders.
pub(crate) fn unit_colour(value: f64, ty: PlyType) -> f32 {
    if ty.is_integer() {
        ((value / 255.0) as f32).clamp(0.0, 1.0)
    } else {
        (value as f32).clamp(0.0, 1.0)
    }
}

/// Fold one already-read record into the output arrays.
fn push_vertex_record(
    out: &mut PlyBody,
    cols: &VertexColumns,
    properties: &[PlyProperty],
    scalars: &[f64],
) {
    out.positions.push([
        scalars[cols.x] as f32,
        scalars[cols.y] as f32,
        scalars[cols.z] as f32,
    ]);
    if let Some([a, b, c]) = cols.normals {
        out.normals
            .push([scalars[a] as f32, scalars[b] as f32, scalars[c] as f32]);
    }
    if let Some([r, g, b]) = cols.colors {
        out.colors.push([
            unit_colour(scalars[r], properties[r].ty),
            unit_colour(scalars[g], properties[g].ty),
            unit_colour(scalars[b], properties[b].ty),
        ]);
    }
}

/// Fan-triangulate one polygon of vertex indices.
pub(crate) fn push_fan(triangles: &mut Vec<[u32; 3]>, indices: &[u32]) {
    for i in 1..indices.len().saturating_sub(1) {
        triangles.push([indices[0], indices[i], indices[i + 1]]);
    }
}

/// Offset just past the `end_header` line.
pub(crate) fn find_ply_header_end(bytes: &[u8]) -> Option<usize> {
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
    fn a_ply_without_faces_loads_as_a_point_cloud() {
        let ply = "\
ply
format ascii 1.0
element vertex 3
property float x
property float y
property float z
end_header
0 0 0
1 2 3
-1 0 0.5
";
        let mesh = load_ply(ply.as_bytes()).expect("a cloud is not an error");
        assert_eq!(mesh.vertex_count(), 3);
        assert!(mesh.is_point_cloud());
        // A cloud counts its points where a mesh counts triangles.
        assert_eq!(mesh.primitive_count(), 3);
        assert_eq!(mesh.bounds.min, [-1.0, 0.0, 0.0]);
        assert_eq!(mesh.bounds.max, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_binary_ply_point_cloud_reads_its_vertices() {
        let header = "\
ply
format binary_little_endian 1.0
element vertex 2
property float x
property float y
property float z
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        for point in [[0f32, 0.0, 0.0], [2.0, 1.0, 0.0]] {
            for value in point {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        let mesh = load_ply(&bytes).expect("binary cloud parses");
        assert!(mesh.is_point_cloud());
        assert_eq!(mesh.bounds.max, [2.0, 1.0, 0.0]);
    }

    /// A declared `face` element makes the file a surface: yielding no
    /// triangle is then a broken export, not an empty cloud.
    #[test]
    fn a_declared_face_element_still_requires_triangles() {
        let ply = "\
ply
format ascii 1.0
element vertex 1
property float x
property float y
property float z
element face 0
property list uchar int vertex_indices
end_header
0 0 0
";
        assert!(load_ply(ply.as_bytes()).is_err());
    }

    /// A coloured scan: `red green blue` as `uchar` scales to 0..=1.
    #[test]
    fn a_ply_point_cloud_reads_its_vertex_colours() {
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
end_header
0 0 0 255 0 0
1 0 0 0 128 255
";
        let mesh = load_ply(ply.as_bytes()).expect("a coloured cloud parses");
        assert!(mesh.is_point_cloud());
        assert!(mesh.has_vertex_colors());
        assert_eq!(
            mesh.colors,
            vec![[1.0, 0.0, 0.0], [0.0, 128.0 / 255.0, 1.0]]
        );
    }

    /// A file that writes its colours as floats already holds 0..=1: scaling
    /// them by 255 the way an integer channel is would blow the cloud out to
    /// white.
    #[test]
    fn float_colour_channels_are_taken_as_they_are() {
        let ply = "\
ply
format ascii 1.0
element vertex 1
property float x
property float y
property float z
property float red
property float green
property float blue
end_header
0 0 0 0.25 0.5 1.0
";
        let mesh = load_ply(ply.as_bytes()).expect("float colours parse");
        assert_eq!(mesh.colors, vec![[0.25, 0.5, 1.0]]);
    }

    /// MeshLab and CloudCompare spell the channels `diffuse_*`; a scan from
    /// either must still come out coloured.
    #[test]
    fn diffuse_colour_channels_are_accepted() {
        let ply = "\
ply
format ascii 1.0
element vertex 1
property float x
property float y
property float z
property uchar diffuse_red
property uchar diffuse_green
property uchar diffuse_blue
end_header
0 0 0 10 20 30
";
        let mesh = load_ply(ply.as_bytes()).expect("diffuse colours parse");
        assert!(mesh.has_vertex_colors());
        assert_eq!(
            mesh.colors,
            vec![[10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0]]
        );
    }

    /// A colour that is present on only some vertices is dropped wholesale:
    /// half a colour array would shade the wrong point.
    #[test]
    fn an_incomplete_colour_array_is_dropped() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
property uchar red
end_header
0 0 0 255
1 0 0 0
";
        let mesh = load_ply(ply.as_bytes()).expect("a red-only cloud parses");
        assert!(!mesh.has_vertex_colors());
        assert!(mesh.colors.is_empty());
    }

    /// Colours must not disturb the triangle path: a mesh keeps its geometry,
    /// and the GPU layout for a model still carries no colour attribute.
    #[test]
    fn a_coloured_mesh_still_loads_its_triangles() {
        let ply = "\
ply
format ascii 1.0
element vertex 3
property float x
property float y
property float z
property uchar red
property uchar green
property uchar blue
element face 1
property list uchar int vertex_indices
end_header
0 0 0 255 0 0
1 0 0 0 255 0
0 1 0 0 0 255
3 0 1 2
";
        let mesh = load_ply(ply.as_bytes()).expect("a coloured mesh parses");
        assert!(!mesh.is_point_cloud());
        assert_eq!(mesh.triangles.len(), 1);
        // Read anyway, so a later consumer need not reparse, but the triangle
        // renderer ignores them.
        assert!(mesh.has_vertex_colors());
    }

    /// OBJ and STL do not get the cloud treatment: a stray vertex there is a
    /// broken export, which `obj_without_geometry_is_an_error` pins down.
    #[test]
    fn model_extensions_are_recognised() {
        assert!(is_model_ext("obj") && is_model_ext("stl") && is_model_ext("ply"));
        assert!(!is_model_ext("png"));
    }

    /// Big-endian binary: the byte order declared in the header travels
    /// with every value, colours included.
    #[test]
    fn binary_ply_big_endian_reads_vertices_and_colours() {
        let header = "\
ply
format binary_big_endian 1.0
element vertex 2
property float x
property float y
property float z
property uchar red
property uchar green
property uchar blue
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        let points = [[0f32, 0.0, 0.0], [1.0, 2.0, 3.0]];
        let colours = [[255u8, 0, 0], [0, 128, 255]];
        for (point, colour) in points.iter().zip(colours.iter()) {
            for value in point {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            bytes.extend_from_slice(colour);
        }
        let mesh = load_ply(&bytes).expect("big-endian ply parses");
        assert!(mesh.is_point_cloud());
        assert_eq!(mesh.bounds.max, [1.0, 2.0, 3.0]);
        assert_eq!(
            mesh.colors,
            vec![[1.0, 0.0, 0.0], [0.0, 128.0 / 255.0, 1.0]]
        );
    }

    /// An ASCII cloud large enough to take the parallel line-chunk path: no
    /// record may be lost or reordered at the chunk seams.
    #[test]
    fn a_large_ascii_ply_parses_in_parallel_chunks() {
        let mut ply = String::from(
            "ply\nformat ascii 1.0\nelement vertex 30000\n\
             property float x\nproperty float y\nproperty float z\n\
             property uchar red\nproperty uchar green\nproperty uchar blue\n\
             end_header\n",
        );
        for i in 0..30000u32 {
            let v = i as f32 * 0.25;
            ply.push_str(&format!("{v:.3} {v:.3} {v:.3} 255 255 255\n"));
        }
        let mesh = load_ply(ply.as_bytes()).expect("a large cloud parses");
        assert_eq!(mesh.vertex_count(), 30000);
        assert!(mesh.has_vertex_colors());
        assert_eq!(mesh.positions[0], [0.0; 3]);
        assert_eq!(mesh.positions[29999], [7499.75; 3]);
    }

    /// A header that promises far more vertices than the body holds must
    /// fail, not size an array from the lie.
    #[test]
    fn a_binary_header_lying_about_its_count_errors() {
        let header = "\
ply
format binary_little_endian 1.0
element vertex 1000000000
property float x
property float y
property float z
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(&0f32.to_le_bytes());
        assert!(load_ply(&bytes).is_err());
    }

    /// The size cap in `load`: a file past the limit is refused before it
    /// is read, without needing a multi-gigabyte fixture.
    #[test]
    fn oversized_model_files_are_refused() {
        let path = std::env::temp_dir().join("trove-ply-cap-test.ply");
        std::fs::write(&path, b"ply\nformat ascii 1.0\nend_header\n").unwrap();
        let err = load_capped(&path, 4).expect_err("a file over the cap is refused");
        std::fs::remove_file(&path).ok();
        assert!(err.contains("larger than"));
    }

    /// Faces arrive as polygons: a quad fan-triangulates into two triangles.
    #[test]
    fn ascii_ply_quads_fan_triangulate() {
        let ply = "\
ply
format ascii 1.0
element vertex 4
property float x
property float y
property float z
element face 1
property list uchar int vertex_indices
end_header
0 0 0
1 0 0
1 1 0
0 1 0
4 0 1 2 3
";
        let mesh = load_ply(ply.as_bytes()).expect("a quad parses");
        assert_eq!(mesh.triangles, vec![[0, 1, 2], [0, 2, 3]]);
    }

    /// A vertex element carrying a list property is exotic but valid: the
    /// list is walked past and the scalars still land.
    #[test]
    fn a_vertex_list_property_is_walked_past() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
property list uchar float bins
end_header
0 0 0 3 1 2 3
1 1 1 0
";
        let mesh = load_ply(ply.as_bytes()).expect("a list-bearing vertex parses");
        assert_eq!(mesh.vertex_count(), 2);
        assert_eq!(mesh.bounds.max, [1.0; 3]);
    }

    /// The binary flavour of the same exotic shape.
    #[test]
    fn a_binary_vertex_list_property_is_walked_past() {
        let header = "\
ply
format binary_little_endian 1.0
element vertex 2
property float x
property float y
property float z
property list uchar float bins
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        for x in [0.0f32, 9.5] {
            for value in [x, 1.0, 2.0] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes.push(1); // one bin
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        let mesh = load_ply(&bytes).expect("a binary list-bearing vertex parses");
        assert_eq!(mesh.vertex_count(), 2);
        assert_eq!(mesh.bounds.max, [9.5, 1.0, 2.0]);
    }

    /// CRLF endings and stray blank lines between records still parse: the
    /// line walker treats them as separators, not records.
    #[test]
    fn ascii_ply_tolerates_crlf_and_blank_lines() {
        let ply = "ply\r\nformat ascii 1.0\r\nelement vertex 3\r\n\
                   property float x\r\nproperty float y\r\nproperty float z\r\n\
                   end_header\r\n0 0 0\r\n\r\n1 2 3\r\n-1 0 0.5\r\n";
        let mesh = load_ply(ply.as_bytes()).expect("a crlf cloud parses");
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.bounds.min, [-1.0, 0.0, 0.0]);
    }
}
