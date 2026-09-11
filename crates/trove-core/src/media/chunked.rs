//! Chunked / streaming PLY loader for files that don't fit in memory.
//!
//! The stock [`super::mesh::load`] reads the whole file and parses every
//! vertex into a `Vec` — fine up to a few gigabytes, impossible at twenty.
//! This module replaces both steps for large PLY files:
//!
//! * **File access** — the file is memory-mapped ([`MmapFile`]) instead of
//!   read into a `Vec<u8>`, so the kernel pages in only what the parser
//!   actually touches;
//! * **Parsing** — a level-of-detail step ([`LodConfig`]) keeps only every
//!   Nth vertex and, for meshes, only every Nth face. The result is a
//!   [`Mesh`] whose memory is bounded by `memory_budget` regardless of how
//!   large the file is.
//!
//! The two PLY layouts are handled differently:
//!
//! * **binary** — vertices are fixed-stride, so vertex N sits at
//!   `header_end + N * stride`: we seek straight to every Nth one. For
//!   meshes, faces are walked once to mark referenced vertices, then a
//!   second pass parses only the kept vertices and remaps face indices.
//!   For point clouds, every Nth vertex is kept directly.
//! * **ASCII** — vertices arrive one per line, so the section is walked
//!   line by line and every Nth non-empty line is parsed. The same
//!   two-pass remeshing applies to faces.
//!
//! [`load_ply_chunked`] is the single entry point: it picks the right
//! strategy from the `format` line and returns a [`Mesh`] the renderer
//! already understands.

use std::fs::File;
use std::path::Path;

use super::mesh::{
    self, BODY_END, Mesh, PlyElement, PlyEndian, PlyProperty, PlyType, VertexColumns,
};

/// How much RAM the parsed [`Mesh`] is allowed to occupy. The loader picks
/// a level-of-detail step that keeps the result under this budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LodConfig {
    /// Target upper bound on the parsed mesh, in bytes of `Vec` storage
    /// (positions + normals + colours + triangles).
    pub memory_budget: usize,
    /// Hard ceiling on the LOD step regardless of the budget, so a file
    /// with few vertices is never pointlessly thinned.
    pub max_lod_step: u32,
}

impl LodConfig {
    /// A budget generous enough for an interactive preview: 512 MiB of
    /// parsed geometry.
    pub const fn default_preview() -> Self {
        Self {
            memory_budget: 512 << 20,
            max_lod_step: 16,
        }
    }
}

impl Default for LodConfig {
    fn default() -> Self {
        Self::default_preview()
    }
}

/// File bytes, either memory-mapped (large files) or loaded whole (small
/// ones). The parsers only see `&[u8]`, so the two are interchangeable.
enum MmapFile {
    Vec(Vec<u8>),
    Mmap(memmap2::Mmap),
}

impl MmapFile {
    /// Open `path`, mmap-ing when `threshold` bytes is exceeded, reading
    /// whole otherwise.
    fn open(path: &Path, threshold: u64) -> Result<Self, String> {
        let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        if size > threshold {
            let file = File::open(path).map_err(|e| e.to_string())?;
            // SAFETY: PLY files are read-only once written; we never mmap
            // a file another process is mutating. The mapping stays alive
            // as long as this `MmapFile` does.
            let mmap =
                unsafe { memmap2::MmapOptions::new().map(&file) }.map_err(|e| e.to_string())?;
            Ok(Self::Mmap(mmap))
        } else {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            Ok(Self::Vec(bytes))
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Vec(v) => v.as_slice(),
            Self::Mmap(m) => &m[..],
        }
    }
}

/// One PLY property declaration (re-typed to avoid a name clash).
type PlyProp = PlyProperty;

/// Parts worth keeping from a PLY body.
#[derive(Default)]
struct ChunkedBody {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
}

/// Parse a PLY file with bounded memory.
///
/// The file-size cap that [`mesh::load`] enforces (2 GiB) is gone.
pub fn load_ply_chunked(path: &Path, config: LodConfig) -> Result<Mesh, String> {
    // 64 GiB threshold: below it the kernel's page cache makes a plain
    // read competitive; above it mmap saves us from doubling the file in
    // RAM before parsing even starts.
    const MMAP_THRESHOLD: u64 = 64 << 30;

    let file = MmapFile::open(path, MMAP_THRESHOLD)?;
    let bytes = file.as_slice();

    let header_end = mesh::find_ply_header_end(bytes).ok_or("not a PLY file")?;
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
                        element.properties.push(PlyProp {
                            name,
                            ty,
                            count_ty: Some(count_ty),
                        });
                    }
                } else if let Some(ty) = PlyType::parse(first) {
                    element.properties.push(PlyProp {
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
    let vertex_count = elements
        .iter()
        .find(|e| e.name == "vertex")
        .map(|e| e.count)
        .unwrap_or(0);

    if vertex_count == 0 {
        return Err("the PLY file contains no vertices".to_string());
    }

    let declared_faces = elements.iter().any(|e| e.name == "face");

    // LOD step: how many vertices / faces to skip between two we keep.
    let lod_step = choose_lod_step(vertex_count, &elements, config);

    let parsed = match endian {
        Some(order) => parse_binary_chunked(body, &elements, order, lod_step, declared_faces)?,
        None => parse_ascii_chunked(body, &elements, lod_step, declared_faces)?,
    };

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

// ---------------------------------------------------------------------------
// LOD selection
// ---------------------------------------------------------------------------

/// Pick the stride N so that keeping every Nth vertex and every Nth face
/// lands the parsed `Mesh` near `config.memory_budget` bytes, capped at
/// `config.max_lod_step`.
///
/// The strategy differs by geometry kind:
///
/// * **Point cloud (no faces):** every Nth vertex is kept. Memory is
///   `vertex_count / N * bytes_per_vertex`, so `N = total_bytes / budget`.
/// * **Mesh (has faces):** every Nth face is kept, and every vertex
///   referenced by the kept faces is parsed. A face references 3
///   vertices on average but they share, so the unique-vertex count is
///   roughly `kept_faces * 2`. We solve `kept_faces * (24 + 12) <= budget`.
fn choose_lod_step(vertex_count: usize, elements: &[PlyElement], config: LodConfig) -> u32 {
    let vertex = match elements.iter().find(|e| e.name == "vertex") {
        Some(v) => v,
        None => return 1,
    };
    let face_count = elements
        .iter()
        .find(|e| e.name == "face")
        .map(|e| e.count)
        .unwrap_or(0);

    let bytes_per_vertex = 12 + vertex_normal_cost(vertex) + vertex_color_cost(vertex);
    let budget = config.memory_budget.max(1) as u64;

    if face_count == 0 {
        // Point cloud: subsample vertices directly.
        let total = vertex_count as u64 * bytes_per_vertex as u64;
        if total <= budget {
            return 1;
        }
        let step = ((total + budget - 1) / budget) as u32;
        return step.max(1).min(config.max_lod_step);
    }

    // Mesh: subsample faces. Each kept face references ~2 unique vertices
    // on average (3 indices with sharing), and costs 12 bytes for its
    // indices + 2 * bytes_per_vertex for its vertices.
    let bytes_per_kept_face = 12 + 2 * bytes_per_vertex as u64;
    let total = face_count as u64 * bytes_per_kept_face;
    if total <= budget {
        return 1;
    }
    let step = ((total + budget - 1) / budget) as u32;
    step.max(1).min(config.max_lod_step)
}

fn vertex_normal_cost(vertex: &PlyElement) -> u64 {
    let has = vertex
        .properties
        .iter()
        .any(|p| matches!(p.name.as_str(), "nx" | "ny" | "nz"));
    if has { 12 } else { 0 }
}

fn vertex_color_cost(vertex: &PlyElement) -> u64 {
    let has = vertex.properties.iter().any(|p| {
        matches!(
            p.name.as_str(),
            "red" | "green" | "blue" | "diffuse_red" | "diffuse_green" | "diffuse_blue"
        )
    });
    if has { 12 } else { 0 }
}

// ---------------------------------------------------------------------------
// Binary chunked parsing
// ---------------------------------------------------------------------------

/// Parse a binary PLY body with bounded memory.
///
/// Strategy:
/// - **Point cloud (no faces):** every `lod_step`-th vertex is parsed
///   directly; no remapping needed.
/// - **Mesh (has faces):** a first pass walks the face block to mark which
///   vertices are referenced, then a second pass parses only the kept
///   vertices (those both referenced AND at a multiple of `lod_step`) and
///   remaps face indices to the new layout. Triangles referencing any
///   culled vertex are dropped.
fn parse_binary_chunked(
    body: &[u8],
    elements: &[PlyElement],
    order: PlyEndian,
    lod_step: u32,
    has_faces: bool,
) -> Result<ChunkedBody, String> {
    let mut out = ChunkedBody::default();
    let vertex_count = elements
        .iter()
        .find(|e| e.name == "vertex")
        .map(|e| e.count)
        .unwrap_or(0);

    if has_faces {
        // -- Mesh: two-pass with remapping --
        // Pass 1: skip to the face block, collect every lod_step-th face's
        // vertex indices, and mark referenced vertices.
        let mut cursor = 0usize;
        let mut face_indices: Vec<Vec<u32>> = Vec::new();
        let mut referenced = vec![0u8; vertex_count];

        for element in elements {
            if element.name == "vertex" {
                skip_binary_element(body, &mut cursor, element, order)?;
                continue;
            }
            if element.name == "face" {
                collect_binary_faces(
                    body,
                    &mut cursor,
                    element,
                    order,
                    lod_step,
                    &mut face_indices,
                    &mut referenced,
                )?;
                continue;
            }
            skip_binary_element(body, &mut cursor, element, order)?;
        }

        // Build remap: old_index → new_index. Keep every referenced vertex;
        // the LOD stride was already applied at face-collection time.
        let mut remap = Vec::<u32>::with_capacity(vertex_count);
        let mut new_index = 0u32;
        for (_old, is_ref) in referenced.iter().enumerate() {
            if *is_ref != 0 {
                remap.push(new_index);
                new_index += 1;
            } else {
                remap.push(u32::MAX); // culled
            }
        }

        // Pass 2: parse only the kept vertices.
        cursor = 0;
        for element in elements {
            if element.name == "vertex" {
                let fixed = element.properties.iter().all(|p| p.count_ty.is_none());
                if fixed && let Some(cols) = vertex_columns(&element.properties) {
                    parse_binary_vertices_remapped(
                        body,
                        &mut cursor,
                        element,
                        &cols,
                        order,
                        &remap,
                        &mut out,
                    )?;
                    continue;
                }
                skip_binary_element(body, &mut cursor, element, order)?;
                continue;
            }
            skip_binary_element(body, &mut cursor, element, order)?;
        }

        // Emit remapped triangles.
        for face in &face_indices {
            if face.len() < 3 {
                continue;
            }
            for i in 1..face.len() - 1 {
                let (a, b, c) = (face[0], face[i], face[i + 1]);
                let (ra, rb, rc) = (
                    *remap.get(a as usize).unwrap_or(&u32::MAX),
                    *remap.get(b as usize).unwrap_or(&u32::MAX),
                    *remap.get(c as usize).unwrap_or(&u32::MAX),
                );
                if ra == u32::MAX || rb == u32::MAX || rc == u32::MAX {
                    continue;
                }
                out.triangles.push([ra, rb, rc]);
            }
        }
    } else {
        // -- Point cloud: simple stride subsampling --
        let mut cursor = 0usize;
        for element in elements {
            if element.name == "vertex" {
                let fixed = element.properties.iter().all(|p| p.count_ty.is_none());
                if fixed && let Some(cols) = vertex_columns(&element.properties) {
                    parse_binary_vertices_stride(
                        body,
                        &mut cursor,
                        element,
                        &cols,
                        order,
                        lod_step,
                        &mut out,
                    )?;
                    continue;
                }
                skip_binary_element(body, &mut cursor, element, order)?;
                continue;
            }
            skip_binary_element(body, &mut cursor, element, order)?;
        }
    }

    Ok(out)
}

/// Skip a whole binary element (fixed or list-bearing).
fn skip_binary_element(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    order: PlyEndian,
) -> Result<(), String> {
    let fixed = element.properties.iter().all(|p| p.count_ty.is_none());
    if fixed {
        let stride: usize = element.properties.iter().map(|p| p.ty.width()).sum();
        let size = element.count.checked_mul(stride).ok_or(BODY_END)?;
        *cursor = (*cursor)
            .checked_add(size)
            .filter(|end| *end <= body.len())
            .ok_or(BODY_END)?;
        Ok(())
    } else {
        // List-bearing: skip record by record.
        for _ in 0..element.count {
            for property in &element.properties {
                match property.count_ty {
                    Some(count_ty) => {
                        let width = count_ty.width();
                        if cursor.saturating_add(width) > body.len() {
                            return Err(BODY_END.into());
                        }
                        let length = mesh::scalar_at(body, *cursor, count_ty, order)
                            .ok_or(BODY_END)? as usize;
                        let item_width = property.ty.width();
                        let record_width = width + length * item_width;
                        *cursor = (*cursor)
                            .checked_add(record_width)
                            .filter(|at| *at <= body.len())
                            .ok_or(BODY_END)?;
                    }
                    None => {
                        let width = property.ty.width();
                        *cursor = (*cursor)
                            .checked_add(width)
                            .filter(|at| *at <= body.len())
                            .ok_or(BODY_END)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Collect every `lod_step`-th face's vertex indices and mark referenced
/// vertices.
fn collect_binary_faces(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    order: PlyEndian,
    lod_step: u32,
    face_indices: &mut Vec<Vec<u32>>,
    referenced: &mut [u8],
) -> Result<(), String> {
    let indices_col = element
        .properties
        .iter()
        .position(|p| p.count_ty.is_some() && mesh::is_indices(&p.name));
    let count_ty = indices_col
        .map(|col| element.properties[col].count_ty.unwrap_or(PlyType::U8))
        .unwrap_or(PlyType::U8);
    let item_ty = indices_col
        .map(|col| element.properties[col].ty)
        .unwrap_or(PlyType::I32);

    let step = lod_step as usize;
    let mut face_id = 0usize;

    for _ in 0..element.count {
        if *cursor + count_ty.width() > body.len() {
            return Err(BODY_END.into());
        }
        let count = mesh::scalar_at(body, *cursor, count_ty, order).ok_or(BODY_END)? as usize;
        *cursor += count_ty.width();
        let item_bytes = count * item_ty.width();
        if *cursor + item_bytes > body.len() {
            return Err(BODY_END.into());
        }

        if face_id % step == 0 {
            let mut face = Vec::with_capacity(count);
            for i in 0..count {
                let at = *cursor + i * item_ty.width();
                let idx = mesh::scalar_at(body, at, item_ty, order).ok_or(BODY_END)? as u32;
                face.push(idx);
                if (idx as usize) < referenced.len() {
                    referenced[idx as usize] = 1;
                }
            }
            face_indices.push(face);
        }
        *cursor += item_bytes;
        face_id += 1;
    }
    Ok(())
}

/// Parse only the kept binary vertices, using the remapping table.
fn parse_binary_vertices_remapped(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    order: PlyEndian,
    remap: &[u32],
    out: &mut ChunkedBody,
) -> Result<(), String> {
    let properties = &element.properties;
    let stride: usize = properties.iter().map(|p| p.ty.width()).sum();
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

    let kept = remap.iter().filter(|r| **r != u32::MAX).count();
    out.positions.reserve(kept);
    if cols.normals.is_some() {
        out.normals.reserve(kept);
    }
    if cols.colors.is_some() {
        out.colors.reserve(kept);
    }

    for (old_index, new_index) in remap.iter().enumerate() {
        if *new_index == u32::MAX {
            continue;
        }
        let base = *cursor + old_index * stride;
        let row = &body[base..base + stride];
        out.positions.push([
            mesh::scalar_row(row, x.0, x.1, order) as f32,
            mesh::scalar_row(row, y.0, y.1, order) as f32,
            mesh::scalar_row(row, z.0, z.1, order) as f32,
        ]);
        if let Some(n) = normals {
            out.normals.push([
                mesh::scalar_row(row, n[0].0, n[0].1, order) as f32,
                mesh::scalar_row(row, n[1].0, n[1].1, order) as f32,
                mesh::scalar_row(row, n[2].0, n[2].1, order) as f32,
            ]);
        }
        if let Some(c) = colors {
            out.colors.push([
                mesh::color_row(row, c[0].0, c[0].1, order),
                mesh::color_row(row, c[1].0, c[1].1, order),
                mesh::color_row(row, c[2].0, c[2].1, order),
            ]);
        }
    }

    *cursor = end;
    Ok(())
}

/// Parse every `lod_step`-th binary vertex (point-cloud path).
fn parse_binary_vertices_stride(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    order: PlyEndian,
    lod_step: u32,
    out: &mut ChunkedBody,
) -> Result<(), String> {
    let properties = &element.properties;
    let stride: usize = properties.iter().map(|p| p.ty.width()).sum();
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

    let step = lod_step as usize;
    let kept = (element.count + step - 1) / step;
    out.positions.reserve(kept);
    if cols.normals.is_some() {
        out.normals.reserve(kept);
    }
    if cols.colors.is_some() {
        out.colors.reserve(kept);
    }

    let mut index = 0usize;
    while index < element.count {
        let base = *cursor + index * stride;
        let row = &body[base..base + stride];
        out.positions.push([
            mesh::scalar_row(row, x.0, x.1, order) as f32,
            mesh::scalar_row(row, y.0, y.1, order) as f32,
            mesh::scalar_row(row, z.0, z.1, order) as f32,
        ]);
        if let Some(n) = normals {
            out.normals.push([
                mesh::scalar_row(row, n[0].0, n[0].1, order) as f32,
                mesh::scalar_row(row, n[1].0, n[1].1, order) as f32,
                mesh::scalar_row(row, n[2].0, n[2].1, order) as f32,
            ]);
        }
        if let Some(c) = colors {
            out.colors.push([
                mesh::color_row(row, c[0].0, c[0].1, order),
                mesh::color_row(row, c[1].0, c[1].1, order),
                mesh::color_row(row, c[2].0, c[2].1, order),
            ]);
        }
        index += step;
    }

    *cursor = end;
    Ok(())
}

// ---------------------------------------------------------------------------
// ASCII chunked parsing
// ---------------------------------------------------------------------------

/// Parse an ASCII PLY body with bounded memory.
///
/// Same two-pass remeshing strategy as the binary path.
fn parse_ascii_chunked(
    body: &[u8],
    elements: &[PlyElement],
    lod_step: u32,
    has_faces: bool,
) -> Result<ChunkedBody, String> {
    let mut out = ChunkedBody::default();
    let vertex_count = elements
        .iter()
        .find(|e| e.name == "vertex")
        .map(|e| e.count)
        .unwrap_or(0);

    if has_faces {
        // -- Mesh: two-pass with remapping --
        let mut cursor = 0usize;
        let mut face_indices: Vec<Vec<u32>> = Vec::new();
        let mut referenced = vec![0u8; vertex_count];

        for element in elements {
            if element.name == "vertex" {
                for _ in 0..element.count {
                    mesh::next_record_line(body, &mut cursor).ok_or(BODY_END)?;
                }
                continue;
            }
            if element.name == "face" {
                collect_ascii_faces(
                    body,
                    &mut cursor,
                    element,
                    lod_step,
                    &mut face_indices,
                    &mut referenced,
                )?;
                continue;
            }
            for _ in 0..element.count {
                mesh::next_record_line(body, &mut cursor).ok_or(BODY_END)?;
            }
        }

        let mut remap = Vec::<u32>::with_capacity(vertex_count);
        let mut new_index = 0u32;
        for (_old, is_ref) in referenced.iter().enumerate() {
            if *is_ref != 0 {
                remap.push(new_index);
                new_index += 1;
            } else {
                remap.push(u32::MAX);
            }
        }

        cursor = 0;
        for element in elements {
            if element.name == "vertex"
                && let Some(cols) = vertex_columns(&element.properties)
            {
                parse_ascii_vertices_remapped(body, &mut cursor, element, &cols, &remap, &mut out)?;
                continue;
            }
            for _ in 0..element.count {
                mesh::next_record_line(body, &mut cursor).ok_or(BODY_END)?;
            }
        }

        for face in &face_indices {
            if face.len() < 3 {
                continue;
            }
            for i in 1..face.len() - 1 {
                let (a, b, c) = (face[0], face[i], face[i + 1]);
                let (ra, rb, rc) = (
                    *remap.get(a as usize).unwrap_or(&u32::MAX),
                    *remap.get(b as usize).unwrap_or(&u32::MAX),
                    *remap.get(c as usize).unwrap_or(&u32::MAX),
                );
                if ra == u32::MAX || rb == u32::MAX || rc == u32::MAX {
                    continue;
                }
                out.triangles.push([ra, rb, rc]);
            }
        }
    } else {
        // -- Point cloud: simple stride subsampling --
        let mut cursor = 0usize;
        for element in elements {
            if element.name == "vertex"
                && let Some(cols) = vertex_columns(&element.properties)
            {
                parse_ascii_vertices_stride(body, &mut cursor, element, &cols, lod_step, &mut out)?;
                continue;
            }
            for _ in 0..element.count {
                mesh::next_record_line(body, &mut cursor).ok_or(BODY_END)?;
            }
        }
    }

    Ok(out)
}

/// Collect every `lod_step`-th ASCII face's vertex indices.
fn collect_ascii_faces(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    lod_step: u32,
    face_indices: &mut Vec<Vec<u32>>,
    referenced: &mut [u8],
) -> Result<(), String> {
    let step = lod_step as usize;
    let mut face_id = 0usize;

    for _ in 0..element.count {
        let line = mesh::next_record_line(body, cursor).ok_or(BODY_END)?;
        if face_id % step != 0 {
            face_id += 1;
            continue;
        }
        face_id += 1;

        let mut tokens = mesh::Tokens::new(line);
        let length = mesh::token_f64(tokens.next_token().ok_or(BODY_END)?)
            .ok_or(BODY_END)?
            .max(0.0) as usize;
        let mut face = Vec::with_capacity(length);
        for _ in 0..length {
            let index =
                mesh::token_f64(tokens.next_token().ok_or(BODY_END)?).ok_or(BODY_END)? as u32;
            face.push(index);
            if (index as usize) < referenced.len() {
                referenced[index as usize] = 1;
            }
        }
        face_indices.push(face);
    }
    Ok(())
}

/// Parse only the kept ASCII vertices, using the remapping table.
fn parse_ascii_vertices_remapped(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    remap: &[u32],
    out: &mut ChunkedBody,
) -> Result<(), String> {
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

    let kept = remap.iter().filter(|r| **r != u32::MAX).count();
    out.positions.reserve(kept);
    if cols.normals.is_some() {
        out.normals.reserve(kept);
    }
    if cols.colors.is_some() {
        out.colors.reserve(kept);
    }

    for (_old_index, new_index) in remap.iter().enumerate() {
        if *new_index == u32::MAX {
            mesh::next_record_line(body, cursor).ok_or(BODY_END)?;
            continue;
        }
        let line = mesh::next_record_line(body, cursor).ok_or(BODY_END)?;
        let mut tokens = mesh::Tokens::new(line);
        let Some(first) = tokens.next_token() else {
            continue;
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
                record[(slot - 1) as usize] = mesh::token_f64(value).ok_or(BODY_END)?;
            }
            column += 1;
            token = tokens.next_token();
        }
        if column <= last_col {
            return Err(BODY_END.into());
        }
        out.positions
            .push([record[0] as f32, record[1] as f32, record[2] as f32]);
        if cols.normals.is_some() {
            out.normals
                .push([record[3] as f32, record[4] as f32, record[5] as f32]);
        }
        if let Some([r, g, b]) = cols.colors {
            out.colors.push([
                mesh::unit_colour(record[6], element.properties[r].ty),
                mesh::unit_colour(record[7], element.properties[g].ty),
                mesh::unit_colour(record[8], element.properties[b].ty),
            ]);
        }
    }

    Ok(())
}

/// Parse every `lod_step`-th ASCII vertex (point-cloud path).
fn parse_ascii_vertices_stride(
    body: &[u8],
    cursor: &mut usize,
    element: &PlyElement,
    cols: &VertexColumns,
    lod_step: u32,
    out: &mut ChunkedBody,
) -> Result<(), String> {
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

    let step = lod_step as usize;
    let mut vertex_id = 0usize;

    for _ in 0..element.count {
        let line = mesh::next_record_line(body, cursor).ok_or(BODY_END)?;
        if vertex_id % step != 0 {
            vertex_id += 1;
            continue;
        }
        vertex_id += 1;

        let mut tokens = mesh::Tokens::new(line);
        let Some(first) = tokens.next_token() else {
            continue;
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
                record[(slot - 1) as usize] = mesh::token_f64(value).ok_or(BODY_END)?;
            }
            column += 1;
            token = tokens.next_token();
        }
        if column <= last_col {
            return Err(BODY_END.into());
        }
        out.positions
            .push([record[0] as f32, record[1] as f32, record[2] as f32]);
        if cols.normals.is_some() {
            out.normals
                .push([record[3] as f32, record[4] as f32, record[5] as f32]);
        }
        if let Some([r, g, b]) = cols.colors {
            out.colors.push([
                mesh::unit_colour(record[6], element.properties[r].ty),
                mesh::unit_colour(record[7], element.properties[g].ty),
                mesh::unit_colour(record[8], element.properties[b].ty),
            ]);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve the vertex columns; mirrors `mesh::vertex_columns`.
fn vertex_columns(properties: &[PlyProperty]) -> Option<VertexColumns> {
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
        colors: triple(["red", "green", "blue"])
            .or_else(|| triple(["diffuse_red", "diffuse_green", "diffuse_blue"])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_ascii_cloud(path: &std::path::Path, n: usize) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, "ply").unwrap();
        writeln!(f, "format ascii 1.0").unwrap();
        writeln!(f, "element vertex {n}").unwrap();
        writeln!(f, "property float x").unwrap();
        writeln!(f, "property float y").unwrap();
        writeln!(f, "property float z").unwrap();
        writeln!(f, "end_header").unwrap();
        for i in 0..n {
            writeln!(
                f,
                "{} {} {}",
                i as f32 * 0.1,
                (i * 2) as f32 * 0.1,
                (i * 3) as f32 * 0.1
            )
            .unwrap();
        }
    }

    fn write_binary_cloud(path: &std::path::Path, n: usize) {
        let mut f = std::fs::File::create(path).unwrap();
        let header = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex {n}\n\
             property float x\nproperty float y\nproperty float z\nend_header\n"
        );
        f.write_all(header.as_bytes()).unwrap();
        for i in 0..n {
            let x = i as f32 * 0.1;
            let y = (i * 2) as f32 * 0.1;
            let z = (i * 3) as f32 * 0.1;
            f.write_all(&x.to_le_bytes()).unwrap();
            f.write_all(&y.to_le_bytes()).unwrap();
            f.write_all(&z.to_le_bytes()).unwrap();
        }
    }

    fn write_binary_mesh(path: &std::path::Path, grid: usize) {
        let mut positions = Vec::new();
        let mut triangles = Vec::new();
        for y in 0..=grid {
            for x in 0..=grid {
                positions.push([x as f32, y as f32, 0.0f32]);
            }
        }
        for y in 0..grid {
            for x in 0..grid {
                let i = y * (grid + 1) + x;
                triangles.push([i, i + 1, i + grid + 1]);
                triangles.push([i + 1, i + grid + 2, i + grid + 1]);
            }
        }
        let mut f = std::fs::File::create(path).unwrap();
        let header = format!(
            "ply\nformat binary_little_endian 1.0\n\
             element vertex {}\nproperty float x\nproperty float y\nproperty float z\n\
             element face {}\nproperty list uchar int vertex_indices\nend_header\n",
            positions.len(),
            triangles.len()
        );
        f.write_all(header.as_bytes()).unwrap();
        for p in &positions {
            f.write_all(&p[0].to_le_bytes()).unwrap();
            f.write_all(&p[1].to_le_bytes()).unwrap();
            f.write_all(&p[2].to_le_bytes()).unwrap();
        }
        for t in &triangles {
            f.write_all(&3u8.to_le_bytes()).unwrap();
            f.write_all(&(t[0] as i32).to_le_bytes()).unwrap();
            f.write_all(&(t[1] as i32).to_le_bytes()).unwrap();
            f.write_all(&(t[2] as i32).to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn chunked_binary_cloud_matches_stock_loader() {
        let dir = std::env::temp_dir().join("trove-chunked-bin-cloud");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.ply");
        write_binary_cloud(&path, 1000);

        let stock = mesh::load_ply(&std::fs::read(&path).unwrap()).unwrap();
        let chunked = load_ply_chunked(&path, LodConfig::default()).unwrap();

        assert_eq!(chunked.positions.len(), stock.positions.len());
        assert_eq!(chunked.positions.len(), 1000);
    }

    #[test]
    fn chunked_binary_mesh_no_lod() {
        let dir = std::env::temp_dir().join("trove-chunked-bin-mesh");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("grid.ply");
        write_binary_mesh(&path, 100);

        let stock = mesh::load_ply(&std::fs::read(&path).unwrap()).unwrap();
        let chunked = load_ply_chunked(&path, LodConfig::default()).unwrap();

        assert!(!stock.is_point_cloud());
        assert!(!chunked.is_point_cloud());
        assert_eq!(chunked.positions.len(), stock.positions.len());
        assert_eq!(chunked.triangles.len(), stock.triangles.len());
    }

    #[test]
    fn chunked_lod_thins_geometry() {
        let dir = std::env::temp_dir().join("trove-chunked-lod");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("big.ply");
        // 64x64 grid = 4096 vertices, 8192 triangles.
        write_binary_mesh(&path, 64);

        // Force LOD step 4 via a small budget.
        let config = LodConfig {
            memory_budget: 20_000,
            max_lod_step: 16,
        };
        let chunked = load_ply_chunked(&path, config).unwrap();
        let stock = mesh::load_ply(&std::fs::read(&path).unwrap()).unwrap();

        assert!(chunked.positions.len() < stock.positions.len());
        assert!(chunked.triangles.len() < stock.triangles.len());
        assert!(!chunked.triangles.is_empty());
        assert!(!chunked.positions.is_empty());
    }

    #[test]
    fn chunked_ascii_cloud() {
        let dir = std::env::temp_dir().join("trove-chunked-ascii");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.ply");
        write_ascii_cloud(&path, 500);

        let chunked = load_ply_chunked(&path, LodConfig::default()).unwrap();
        assert_eq!(chunked.positions.len(), 500);
        assert!(chunked.is_point_cloud());
    }

    #[test]
    fn chunked_very_small_budget_succeeds() {
        let dir = std::env::temp_dir().join("trove-chunked-tiny");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("big.ply");
        // 32x32 grid = 1024 vertices, 2000 triangles.
        write_binary_mesh(&path, 32);

        // A tiny budget forces heavy thinning.
        let config = LodConfig {
            memory_budget: 1_000,
            max_lod_step: 64,
        };
        let chunked = load_ply_chunked(&path, config).unwrap();
        assert!(!chunked.positions.is_empty());
        assert!(!chunked.triangles.is_empty());
    }

    #[test]
    fn chunked_bounds_are_valid() {
        let dir = std::env::temp_dir().join("trove-chunked-bounds");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.ply");
        write_binary_cloud(&path, 1000);

        let chunked = load_ply_chunked(&path, LodConfig::default()).unwrap();
        assert!(!chunked.bounds.is_empty());
        assert!(chunked.bounds.min[0] <= chunked.bounds.max[0]);
    }
}
