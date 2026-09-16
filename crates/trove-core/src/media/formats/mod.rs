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

/// Extensions this module can parse (lowercase, without the dot).
pub const MODEL_EXTENSIONS: [&str; 6] = ["obj", "stl", "ply", "gltf", "glb", "blend"];

/// Whether `ext` (lowercase, without the dot) is a mesh format.
pub fn is_model_ext(ext: &str) -> bool {
    MODEL_EXTENSIONS.contains(&ext)
}

/// Load a mesh, dispatching on the file extension.
///
/// There is deliberately no file-size ceiling here. There used to be one — a
/// 2 GiB cap that refused a file before opening it — and it was the wrong
/// place to draw the line: the formats that actually grow past it have a
/// large-file reader of their own (a PLY goes to [`super::chunked`], which
/// bounds its *parsed* size rather than refusing the file), the caller routes
/// those by reading a header rather than the whole file, and a model a
/// machine *can* open should not be refused because a default chosen for a
/// smaller machine says so. What is left is the honest failure: a file too
/// large for the memory it is read into fails when it is read.
pub fn load(path: &Path) -> Result<Mesh, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !is_model_ext(&ext) {
        return Err(format!("unsupported model format: .{ext}"));
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

    /// The extension is the whole dispatch: an unknown one is refused by name
    /// rather than by attempting a parse that cannot work.
    #[test]
    fn an_unknown_extension_is_refused_with_its_name() {
        let error = load(Path::new("/nowhere/model.xyz")).expect_err("xyz is not a model");
        assert!(error.contains(".xyz"), "{error}");
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

    /// A known extension reaches its parser — there is no longer a size for
    /// it to be turned away by first.
    #[test]
    fn a_known_extension_reaches_its_parser() {
        let path = std::env::temp_dir().join("trove-model-dispatch-test.obj");
        std::fs::write(&path, b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();
        let mesh = load(&path).expect("a small OBJ parses");
        std::fs::remove_file(&path).ok();
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.triangle_count(), 1);
    }
}
