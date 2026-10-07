//! Debug helper: parse a model file and print what came out.
use std::path::PathBuf;

use trove_core::media::formats::types::NO_TEXTURE;

fn main() {
    for arg in std::env::args().skip(1) {
        let path = PathBuf::from(&arg);
        println!("== {}", path.display());
        match trove_core::media::formats::load(&path) {
            Ok(mesh) => {
                println!(
                    "  OK: {} vertices, {} triangles, {} colors, normals={}, textures={} ({} maps)",
                    mesh.positions.len(),
                    mesh.triangles.len(),
                    mesh.colors.len(),
                    mesh.has_vertex_normals(),
                    mesh.has_textures(),
                    mesh.texture.as_ref().map_or(0, |t| t.maps.len()),
                );
                if let Some(t) = &mesh.texture {
                    // Which maps the materials actually sample. A slot is a
                    // per-primitive index into `maps`, and each of a material's
                    // five channels names one — `NO_TEXTURE` on a channel means
                    // that channel has no map, not a map that is missing.
                    let mut slots: Vec<u16> = t
                        .materials
                        .iter()
                        .flat_map(|m| {
                            [m.slot, m.mr_slot, m.normal_slot, m.ao_slot, m.emissive_slot]
                        })
                        .filter(|&slot| slot != NO_TEXTURE)
                        .collect();
                    slots.sort();
                    slots.dedup();
                    println!(
                        "  uv: {} pairs, materials: {}, slots in use: {slots:?}",
                        t.uv.len(),
                        t.materials.len()
                    );
                    for (index, map) in t.maps.iter().enumerate() {
                        let n = (map.rgba.len() / 4).max(1) as u32;
                        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                        for px in map.rgba.as_chunks::<4>().0 {
                            r += px[0] as u32;
                            g += px[1] as u32;
                            b += px[2] as u32;
                        }
                        println!(
                            "  map {index}: {}x{} avg rgb ({}, {}, {})",
                            map.width,
                            map.height,
                            r / n,
                            g / n,
                            b / n
                        );
                    }
                }
            }
            Err(e) => println!("  ERR: {e}"),
        }
    }
}
