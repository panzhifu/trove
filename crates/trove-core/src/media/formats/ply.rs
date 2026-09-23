//! PLY (Polygon File Format) parser.
//!
//! Hand-written because no off-the-shelf crate handles every case trove needs:
//!
//! * **Point clouds** — a PLY with no `face` element is not an error; it is a
//!   scan or photogrammetry export drawn vertex-by-vertex.
//! * **Vertex colours** — `uchar` channels (0..=255) scale to 0..=1; `float`
//!   channels are taken as-is. `diffuse_*` aliases from MeshLab /
//!   CloudCompare are accepted.
//! * **Parallel ASCII** — large bodies are cut at newline boundaries and the
//!   chunks parsed on the rayon pool, then concatenated in file order.
//! * **Leniency** — unknown elements, extra properties and list-bearing
//!   vertex records are walked past; only a file with no usable vertex at
//!   all fails.

use super::types::Mesh;

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
    /// The scanner attributes, for the cloud that carries them. Filled vertex
    /// by vertex beside the positions, and handed to [`CloudFields`] by whoever
    /// builds the mesh.
    ///
    /// [`CloudFields`]: super::types::CloudFields
    intensities: Vec<f32>,
    classes: Vec<u8>,
}

impl PlyBody {
    /// Append one parsed chunk, every array in turn.
    ///
    /// A method rather than five `extend_from_slice` lines at each merge, because
    /// a merge that remembers the geometry and forgets a channel is invisible:
    /// the cloud renders, and only the colouring knows the intensity was dropped.
    fn absorb(&mut self, mut part: Self) {
        self.positions.append(&mut part.positions);
        self.normals.append(&mut part.normals);
        self.colors.append(&mut part.colors);
        self.triangles.append(&mut part.triangles);
        self.intensities.append(&mut part.intensities);
        self.classes.append(&mut part.classes);
    }
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
    /// Scanner attributes, each with the two spellings in the wild: the
    /// PLY/LAS convention, and what a CloudCompare export writes for the same
    /// number. A file with neither just cannot be painted by that field.
    pub(crate) intensity: Option<usize>,
    pub(crate) class: Option<usize>,
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
    let any = |names: [&str; 2]| -> Option<usize> { names.iter().find_map(|name| scalar(name)) };
    Some(VertexColumns {
        x: scalar("x")?,
        y: scalar("y")?,
        z: scalar("z")?,
        normals: triple(["nx", "ny", "nz"]),
        // `red green blue` is the spec spelling; MeshLab and CloudCompare
        // write `diffuse_*` often enough to take both.
        colors: triple(["red", "green", "blue"])
            .or_else(|| triple(["diffuse_red", "diffuse_green", "diffuse_blue"])),
        intensity: any(["intensity", "scalar_intensity"]),
        class: any(["classification", "class"]),
    })
}

/// Whether a face property holds the polygon index list.
pub(crate) fn is_indices(name: &str) -> bool {
    name == "vertex_indices" || name == "vertex_index"
}

/// Byte width of one record of a fixed-size element: the sum of its property
/// widths. `None` when any property is a list, which makes the rows
/// variable-width and has to be walked instead of strided.
pub(crate) fn fixed_row_stride(properties: &[PlyProperty]) -> Option<usize> {
    properties.iter().try_fold(0usize, |total, property| {
        property
            .count_ty
            .is_none()
            .then(|| total + property.ty.width())
    })
}

/// Byte offset of every property inside one fixed-size record; the entry for
/// a list property is meaningless and is only produced after
/// [`fixed_row_stride`] has agreed the element has none.
pub(crate) fn property_offsets(properties: &[PlyProperty]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(properties.len());
    let mut at = 0;
    for property in properties {
        offsets.push(at);
        at += property.ty.width();
    }
    offsets
}

/// Parse the textual part of a PLY header: the byte order and the element
/// blocks, each with its properties resolved to types and column indices.
///
/// Shared by the whole-file loader and the streaming point reader so the two
/// cannot disagree about what a record contains or where a channel sits.
/// A `face` element — the tag that separates a surface from a cloud — is
/// reported like any other; it is up to the caller to decide what to do
/// with it.
pub(crate) fn parse_ply_header(
    header: &str,
) -> Result<(Option<PlyEndian>, Vec<PlyElement>), String> {
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
    Ok((endian, elements))
}

/// Parse a PLY file (ASCII or binary, either endianness): reads the header,
/// resolves the interesting properties to column indices once, then pulls
/// the vertex element and the face index lists straight into typed arrays —
/// binary vertex rows through their fixed stride, ASCII lines in parallel
/// chunks. A file with no `face` element becomes a point cloud.
pub fn load_ply(bytes: &[u8]) -> Result<Mesh, String> {
    let header_end = find_ply_header_end(bytes).ok_or("not a PLY file")?;
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let (endian, elements) = parse_ply_header(&header)?;

    let body = &bytes[header_end..];
    let parsed = match endian {
        Some(order) => load_ply_binary(body, &elements, order)?,
        None => load_ply_ascii(body, &elements)?,
    };

    // A `face` element separates a surface from a cloud — but only when it
    // declares records. `element face 0` is a point cloud an exporter tagged
    // with a vestigial face element, which is common enough to matter, and
    // rejecting it would lose a perfectly readable scan. A header that
    // declares face records and still yields no usable triangle is a broken
    // export, and that stays an error.
    let declared_faces = elements
        .iter()
        .any(|element| element.name == "face" && element.count > 0);
    if declared_faces {
        Mesh::finish(
            parsed.positions,
            parsed.normals,
            parsed.colors,
            parsed.triangles,
        )
        .ok_or_else(|| "the PLY file contains no triangles".to_string())
    } else {
        Mesh::finish_points_with(
            parsed.positions,
            parsed.normals,
            parsed.colors,
            super::types::CloudFields {
                intensities: parsed.intensities,
                classes: parsed.classes,
            },
        )
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
    let intensity = cols.intensity.map(|c| (offset(c), ty(c)));
    let class = cols.class.map(|c| (offset(c), ty(c)));

    out.positions.reserve(element.count);
    if cols.normals.is_some() {
        out.normals.reserve(element.count);
    }
    if cols.colors.is_some() {
        out.colors.reserve(element.count);
    }
    if cols.intensity.is_some() {
        out.intensities.reserve(element.count);
    }
    if cols.class.is_some() {
        out.classes.reserve(element.count);
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
        if let Some(s) = intensity {
            out.intensities
                .push(scalar_row(row, s.0, s.1, order) as f32);
        }
        if let Some(s) = class {
            out.classes
                .push(class_byte(scalar_row(row, s.0, s.1, order)));
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
    out.absorb(parsed.body);
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
        parsed.body.absorb(part.body);
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
    // colour channel, 10-11 the two scanner attributes, 0 nothing worth a parse.
    let last_col = [cols.x, cols.y, cols.z]
        .into_iter()
        .chain(cols.normals.into_iter().flatten())
        .chain(cols.colors.into_iter().flatten())
        .chain(cols.intensity.into_iter().chain(cols.class))
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
    if let Some(column) = cols.intensity {
        plan[column] = 10;
    }
    if let Some(column) = cols.class {
        plan[column] = 11;
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
        let mut record = [0.0f64; 11];
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
        if cols.intensity.is_some() {
            part.intensities.push(record[9] as f32);
        }
        if cols.class.is_some() {
            part.classes.push(class_byte(record[10]));
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

/// The classification column as the byte it indexes.
///
/// Clamped rather than wrapped: a file that declared `int` and wrote something
/// outside 0..=255 gets the nearest class the palette has, rather than one a
/// two's-complement cast happened to land on.
pub(crate) fn class_byte(value: impl Into<f64>) -> u8 {
    value.into().clamp(0.0, 255.0) as u8
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
    // The ASCII path reads a record's properties into `scalars` already, so a
    // channel is one more column to pick up. Both channels keep the file's own
    // number: the colouring normalises against the range, and a classification
    // is an index rather than a value to scale.
    if let Some(column) = cols.intensity {
        out.intensities.push(scalars[column] as f32);
    }
    if let Some(column) = cols.class {
        out.classes.push(class_byte(scalars[column]));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Two vertices, extra colour properties, and a face element that
        // declares nothing: the colours are read and the file is a cloud.
        let mesh = load_ply(ply.as_bytes()).expect("a tagged cloud parses");
        assert!(mesh.is_point_cloud());
        assert_eq!(mesh.vertex_count(), 2);
        assert_eq!(mesh.colors, vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
    }

    /// The two scanner attributes, under the spelling CloudCompare's own
    /// exporter uses rather than the LAS one.
    #[test]
    fn a_scan_reads_its_channels_under_either_spelling() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
property uchar scalar_intensity
property uchar class
end_header
0 0 0 255 6
1 1 1 12 2
";
        let mesh = load_ply(ply.as_bytes()).expect("a CloudCompare export parses");
        let fields = mesh.fields.as_deref().expect("both channels read");
        assert_eq!(fields.intensities, vec![255.0, 12.0]);
        assert_eq!(fields.classes, vec![6, 2]);
        assert_eq!(mesh.intensity_range(), Some((12.0, 255.0)));
        assert_eq!(mesh.class_count(), Some(7));
    }

    /// A classification column declared wider than the byte it indexes: the
    /// nearest class the palette has wins, rather than whatever a wrapped cast
    /// happened to land on.
    #[test]
    fn an_out_of_range_class_is_clamped_not_wrapped() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
property int classification
end_header
0 0 0 300
1 1 1 -5
";
        let mesh = load_ply(ply.as_bytes()).expect("the odd header parses");
        let fields = mesh.fields.as_deref().expect("the class channel reads");
        assert_eq!(fields.classes, vec![255, 0]);
    }

    /// A file whose records are not all usable: the vertex with a NaN
    /// coordinate goes, and its two channel entries must go with it, or every
    /// point after it is painted with the neighbour's values.
    #[test]
    fn a_dropped_vertex_takes_its_channels_with_it() {
        let ply = "\
ply
format ascii 1.0
element vertex 3
property float x
property float y
property float z
property float intensity
property uchar classification
end_header
0 0 0 10 1
nan 0 0 20 2
1 1 1 30 3
";
        let mesh = load_ply(ply.as_bytes()).expect("the rest of the cloud parses");
        assert_eq!(mesh.vertex_count(), 2);
        let fields = mesh.fields.as_deref().expect("channels survive the drop");
        assert_eq!(fields.intensities, vec![10.0, 30.0]);
        assert_eq!(fields.classes, vec![1, 3]);
    }

    /// A surface keeps its triangles and loses its per-vertex attributes: the
    /// renderers index a cloud's channels by vertex, and a mesh drawn from an
    /// LOD level would index a subset that no longer lines up.
    #[test]
    fn a_triangle_mesh_carries_no_channels() {
        let ply = "\
ply
format ascii 1.0
element vertex 3
property float x
property float y
property float z
property float intensity
element face 1
property list uchar int vertex_indices
end_header
0 0 0 10
1 0 0 20
0 1 0 30
3 0 1 2
";
        let mesh = load_ply(ply.as_bytes()).expect("the surface parses");
        assert!(!mesh.is_point_cloud());
        assert!(mesh.fields.is_none());
        assert_eq!(mesh.intensity_range(), None);
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

    /// The same two attributes on the other side of the format, with the
    /// channels sitting between the position and the colour so the byte offsets
    /// are checked rather than assumed.
    #[test]
    fn a_binary_scan_reads_its_channels() {
        let header = "\
ply
format binary_little_endian 1.0
element vertex 2
property float x
property float y
property float z
property uchar intensity
property uchar classification
property uchar red
property uchar green
property uchar blue
end_header
";
        let mut bytes = header.as_bytes().to_vec();
        for (point, intensity, class, color) in [
            ([0f32, 0.0, 0.0], 250u8, 6u8, [255u8, 0, 0]),
            ([2.0, 1.0, 0.0], 12, 2, [0, 255, 0]),
        ] {
            for value in point {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes.extend_from_slice(&[intensity, class]);
            bytes.extend_from_slice(&color);
        }
        let mesh = load_ply(&bytes).expect("binary scan parses");
        let fields = mesh.fields.as_deref().expect("both channels read");
        assert_eq!(fields.intensities, vec![250.0, 12.0]);
        assert_eq!(fields.classes, vec![6, 2]);
        // And the colour after them still lands in the right slot.
        assert_eq!(mesh.colors, vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
    }

    /// A `face` element that declares records but yields no usable triangle
    /// is a broken export, and stays an error.
    #[test]
    fn a_face_element_that_yields_no_triangle_still_errors() {
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
3 0 0 0
";
        assert!(load_ply(ply.as_bytes()).is_err());
    }

    /// `element face 0` declares no surface at all: the file is a point cloud
    /// an exporter tagged with an empty face element, and it has to load as a
    /// cloud rather than fail with "no triangles".
    #[test]
    fn a_face_element_with_zero_records_is_a_point_cloud() {
        let ply = "\
ply
format ascii 1.0
element vertex 2
property float x
property float y
property float z
element face 0
property list uchar int vertex_indices
end_header
0 0 0
1 2 3
";
        let mesh = load_ply(ply.as_bytes()).expect("a tagged cloud parses");
        assert!(mesh.is_point_cloud());
        assert_eq!(mesh.vertex_count(), 2);
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
        assert!(mesh.has_vertex_colors());
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
