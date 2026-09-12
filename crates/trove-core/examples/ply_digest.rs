//! Temporary A/B harness: dump the arrays `load_ply` produces for a set of
//! PLY files, so the loader rewrite can be checked bit-for-bit against the
//! previous implementation. Delete once the comparison has run.
//!
//! ```text
//! cargo run --release -p trove-core --example ply_digest -- OUT_DIR FILE...
//! ```

use std::path::PathBuf;

use trove_core::media::mesh::load_ply;

fn main() {
    let mut args = std::env::args().skip(1);
    let out_dir = PathBuf::from(args.next().expect("output directory"));
    std::fs::create_dir_all(&out_dir).expect("create output directory");
    for path in args {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path));
        let mesh = load_ply(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path));
        let name = PathBuf::from(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let dump_f32 = |tag: &str, data: &[[f32; 3]]| {
            let mut out = Vec::with_capacity(data.len() * 12);
            for value in data {
                for channel in value {
                    out.extend_from_slice(&channel.to_le_bytes());
                }
            }
            std::fs::write(out_dir.join(format!("{name}.{tag}.f32")), out).unwrap();
        };
        let dump_u32 = |tag: &str, data: &[[u32; 3]]| {
            let mut out = Vec::with_capacity(data.len() * 12);
            for value in data {
                for index in value {
                    out.extend_from_slice(&index.to_le_bytes());
                }
            }
            std::fs::write(out_dir.join(format!("{name}.{tag}.u32")), out).unwrap();
        };
        dump_f32("positions", &mesh.positions);
        dump_f32("normals", &mesh.normals);
        dump_f32("colors", &mesh.colors);
        dump_u32("triangles", &mesh.triangles);
        println!(
            "{name}: {} vertices, {} normals, {} colors, {} triangles",
            mesh.positions.len(),
            mesh.normals.len(),
            mesh.colors.len(),
            mesh.triangles.len()
        );
    }
}
