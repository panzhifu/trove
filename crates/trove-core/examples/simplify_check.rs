//! Debug helper: run QEM simplification on a model and dump every level's
//! triangle count plus renders of each level.
use std::path::PathBuf;

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("a model path"));
    let mesh = trove_core::media::formats::load(&path).expect("parse");
    println!("original: {} triangles", mesh.triangles.len());
    let simplified = trove_core::media::formats::simplify::simplify_mesh(&mesh);
    println!("levels: {}", simplified.levels.len());
    for (index, level) in simplified.levels.iter().enumerate() {
        println!(
            "  level {index}: {} vertices, {} triangles, error={:.4}",
            level.vertices.len(),
            level.triangles.len(),
            level.error
        );
        let normals: Vec<[f32; 3]> = if simplified.has_normals {
            level.vertices.iter().map(|v| v.normal).collect()
        } else {
            Vec::new()
        };
        let level_mesh = trove_core::media::formats::types::Mesh::from_parts(
            level.vertices.iter().map(|v| v.position).collect(),
            normals,
            Vec::new(),
            level.triangles.clone(),
        );
        if let Some(level_mesh) = level_mesh {
            let camera = trove_core::media::render3d::Camera::default();
            let frame = trove_core::media::render3d::render(&level_mesh, &camera, 800, 600, 1, 1.0);
            write_png(&format!("/tmp/trove-lod-{index}.png"), &frame);
        }
    }
    println!("wrote /tmp/trove-lod-*.png");
    check_selection(&simplified);
}

fn write_png(path: &str, frame: &trove_core::media::render3d::Frame) {
    use std::io::Write;
    let mut raw = Vec::with_capacity((frame.width * frame.height * 4) as usize);
    for y in 0..frame.height {
        raw.push(0);
        for x in 0..frame.width {
            let [b, g, r, _] = frame.pixel(x, y);
            raw.extend_from_slice(&[r, g, b]);
        }
    }
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&frame.width.to_be_bytes());
    ihdr.extend_from_slice(&frame.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path)
        .unwrap()
        .write_all(&png)
        .unwrap();
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = 0xFFFF_FFFFu32;
    for byte in kind.iter().chain(data) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(65535).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(if chunks.peek().is_none() { 1 } else { 0 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

// Appended check: which level would the viewport pick at the default camera?
fn check_selection(simplified: &trove_core::media::formats::simplify::SimplifiedMesh) {
    use trove_core::media::render3d::Camera;
    let camera = Camera::default();
    let framing = camera.framing(simplified.bounds, 1.0);
    let radius = 1.0 / framing.inv_radius;
    let distance = (framing.distance * radius) as f64;
    let level = trove_core::media::formats::simplify::select_lod(
        simplified,
        distance,
        600.0,
        trove_core::media::render3d::FOV_DEG,
    );
    println!("default camera: distance={distance:.0} -> level {level:?}");
}
