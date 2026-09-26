//! Debug helper: parse a model file and print what came out.
use std::path::PathBuf;

fn main() {
    for arg in std::env::args().skip(1) {
        let path = PathBuf::from(&arg);
        println!("== {}", path.display());
        match trove_core::media::formats::load(&path) {
            Ok(mesh) => println!(
                "  OK: {} vertices, {} triangles, {} colors, normals={}",
                mesh.positions.len(),
                mesh.triangles.len(),
                mesh.colors.len(),
                mesh.has_vertex_normals()
            ),
            Err(e) => println!("  ERR: {e}"),
        }
    }
}
