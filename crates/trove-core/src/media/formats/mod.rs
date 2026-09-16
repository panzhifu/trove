//! Per-format 3D model parsers.
//!
//! Each sub-module normalises its format into the crate-wide [`Mesh`] type
//! defined in [`types`](crate::media::formats::types). The [`load`] function
//! dispatches to them by file extension.
//!
//! One of them is not a parser: [`blend`] hands a `.blend` to a headless
//! Blender and reads back the glTF it exports, because the file is a dump of
//! Blender's memory rather than a documented format. See that module for why.

use std::path::Path;

pub mod blend;
pub mod gltf;
pub mod meshlet;
pub mod obj;
pub mod ply;
pub mod point_cloud;
pub mod simplify;
pub mod stl;
pub mod streaming;
pub mod streaming_point_cloud;
pub mod types;
pub mod virtual_memory;

pub use streaming_point_cloud::StreamingPointCloud;

pub use types::{Bounds, Mesh};

/// Ceiling on a model file read whole into memory: the parsed mesh costs a
/// multiple of the file, so a stray multi-gigabyte export must fail fast
/// instead of dragging the machine into swap.
const MAX_MODEL_FILE_BYTES: u64 = 2 << 30;

/// Extensions this module can parse (lowercase, without the dot).
pub const MODEL_EXTENSIONS: [&str; 6] = ["obj", "stl", "ply", "gltf", "glb", "blend"];

/// Whether `ext` (lowercase, without the dot) is a mesh format.
pub fn is_model_ext(ext: &str) -> bool {
    MODEL_EXTENSIONS.contains(&ext)
}

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
    match ext.as_str() {
        "obj" => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            obj::load_obj(&text)
        }
        "stl" => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            stl::load_stl(&bytes)
        }
        "ply" => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            ply::load_ply(&bytes)
        }
        "gltf" | "glb" => gltf::load_gltf(path),
        "blend" => blend::load_blend(path),
        _ => Err(format!("unsupported model format: .{ext}")),
    }
}

// Re-export the format-specific loaders for direct use.
pub use obj::load_obj;
pub use ply::load_ply;
pub use stl::load_stl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_extensions_are_recognised() {
        assert!(is_model_ext("obj") && is_model_ext("stl") && is_model_ext("ply"));
        assert!(is_model_ext("gltf") && is_model_ext("glb") && is_model_ext("blend"));
        assert!(!is_model_ext("png"));
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

    /// `.blend` reaches the Blender-backed loader rather than the
    /// "unsupported" arm.
    ///
    /// No fixture and no Blender needed: what is asserted is that the error
    /// comes from the loader — Blender missing, or the file not being there —
    /// and not from the dispatcher refusing the extension.
    #[test]
    fn a_blend_extension_reaches_the_blender_loader() {
        let error = load(Path::new("/nowhere/absent.blend")).expect_err("there is no such file");
        assert!(!error.contains("unsupported model format"), "{error}");
    }
}
