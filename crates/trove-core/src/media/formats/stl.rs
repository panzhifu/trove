//! STL (STereoLithography) parser.
//!
//! Hand-written: STL has only two layouts (ASCII and binary) and a fixed
//! structure, so a 70-line parser covers both. There are no normals, no
//! materials, and no shared-vertex optimisation — every triangle carries
//! its own three vertices.

use super::types::Mesh;

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
}
