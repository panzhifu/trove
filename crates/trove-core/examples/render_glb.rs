//! Debug helper: render a model to a PNG to see what the viewport would show.
use std::path::PathBuf;

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("a model path"));
    let mesh = trove_core::media::formats::load(&path).expect("parse");
    println!(
        "{}: {} vertices, {} triangles, winding={:?}, colors={}",
        path.display(),
        mesh.positions.len(),
        mesh.triangles.len(),
        mesh.winding(),
        mesh.colors.len()
    );
    let camera = match std::env::var("TROVE_DEBUG_CAMERA") {
        Ok(spec) => {
            let parts: Vec<f32> = spec.split(',').filter_map(|v| v.parse().ok()).collect();
            trove_core::media::render3d::Camera {
                yaw: parts.first().copied().unwrap_or(0.0),
                pitch: parts.get(1).copied().unwrap_or(0.0),
                zoom: parts.get(2).copied().unwrap_or(1.0),
                ..trove_core::media::render3d::Camera::default()
            }
        }
        Err(_) => trove_core::media::render3d::Camera::default(),
    };
    let frame = trove_core::media::render3d::render(&mesh, &camera, 800, 600, 2, 1.0);
    write_png("/tmp/trove-render.png", &frame);
    println!("wrote /tmp/trove-render.png");
}

/// Minimal PNG writer: BGRA frame to an 8-bit RGB PNG via zlib.
fn write_png(path: &str, frame: &trove_core::media::render3d::Frame) {
    use std::io::Write;
    let mut raw = Vec::with_capacity((frame.width * frame.height * 4) as usize);
    for y in 0..frame.height {
        raw.push(0); // filter: none
        for x in 0..frame.width {
            let [b, g, r, _a] = frame.pixel(x, y);
            raw.extend_from_slice(&[r, g, b]);
        }
    }
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    push_chunk(&mut png, b"IHDR", {
        let mut c = Vec::new();
        c.extend_from_slice(&frame.width.to_be_bytes());
        c.extend_from_slice(&frame.height.to_be_bytes());
        c.extend_from_slice(&[8, 2, 0, 0, 0]);
        c
    });
    push_chunk(&mut png, b"IDAT", zlib_compress(&raw));
    push_chunk(&mut png, b"IEND", Vec::new());
    std::fs::File::create(path)
        .unwrap()
        .write_all(&png)
        .unwrap();
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: Vec<u8>) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(&data);
    out.extend_from_slice(&crc32_parts(kind, &data).to_be_bytes());
}

/// The CRC over two byte runs, as a PNG chunk needs (its kind, then its body).
fn crc32_parts(first: &[u8], second: &[u8]) -> u32 {
    crc32_update(crc32_update(0xFFFF_FFFF, first), second) ^ 0xFFFF_FFFF
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    // Stored deflate blocks (no compression) wrapped in zlib — fine for a
    // debug helper, no dependency needed.
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(65535).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(if last { 1 } else { 0 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
