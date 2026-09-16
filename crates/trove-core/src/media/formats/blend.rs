//! Blender `.blend` loader: the file is handed to Blender itself.
//!
//! A `.blend` is not a file format so much as a dump of Blender's memory:
//! version-tagged C structs whose pointers were written down as addresses.
//! Since 5.0 the mesh geometry is not even a field any more — it lives in an
//! attribute system (`attribute_storage.dna_attributes`) whose property names
//! are pointers, and the one crate that parses the container (`blend` 0.9) is
//! a structure browser rather than a loader: it refuses compressed files
//! (which is what Blender writes by default now), it panics on the very
//! pointer fields the geometry hangs off, and it cannot read a property name
//! at all. Reading `.blend` in-process therefore means writing a per-version
//! DNA interpreter, and getting it wrong looks like a blank model rather than
//! an error.
//!
//! So the file goes to the program that owns it. `blender -b` runs headless,
//! and the glTF exporter it ships flattens the scene for free — modifiers,
//! object transforms, multi-object scenes, instancing, materials, and
//! compressed files alike. The exported GLB rejoins the pipeline through
//! [`super::gltf`], which is the same path a `.glb` the user exported by hand
//! would take.
//!
//! That makes Blender a *runtime* dependency of `.blend` previews only: every
//! other format is still parsed in-process, and a machine without Blender is
//! told so plainly instead of being shown an empty viewport.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::types::Mesh;

/// Environment variable that says where Blender is, for a machine where it is
/// not on `PATH` under its usual name.
pub const BLENDER_ENV: &str = "TROVE_BLENDER";

/// How long one export may run before it is given up on.
///
/// A headless export of a small scene takes under a second; a scene with
/// hundreds of thousands of objects can take minutes. The limit exists so a
/// pathological file cannot hold a background worker for the rest of the
/// session, not to bound the normal case.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the child is checked for having finished.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes of Blender's own stderr quoted back when an export fails. The last
/// lines are the interesting ones, and the tail is what fits in a status line.
const STDERR_TAIL: usize = 400;

/// Executable names tried on `PATH`.
#[cfg(windows)]
const PROGRAM_NAMES: [&str; 2] = ["blender.exe", "blender"];
#[cfg(not(windows))]
const PROGRAM_NAMES: [&str; 1] = ["blender"];

/// Locations tried when `blender` is not on `PATH`.
///
/// A desktop install does not always put it there: the Flatpak and macOS
/// bundles in particular are launched by name from a menu, and the Snap and
/// distro-package paths are the ones a `.desktop` file uses.
const FALLBACK_PATHS: [&str; 6] = [
    "/usr/bin/blender",
    "/usr/local/bin/blender",
    "/snap/bin/blender",
    "/var/lib/flatpak/exports/bin/org.blender.Blender",
    "/Applications/Blender.app/Contents/MacOS/Blender",
    r"C:\Program Files\Blender Foundation\Blender\blender.exe",
];

/// Counter making scratch names unique within the process.
static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Load a `.blend` by exporting it to glTF with a headless Blender.
pub fn load_blend(path: &Path) -> Result<Mesh, String> {
    let blender = find_blender().ok_or_else(no_blender)?;
    let glb = scratch_path("glb");
    let log = scratch_path("log");
    // Both files are removed however this returns, including on the error
    // paths below — a failed export must not leave a GLB behind.
    let _glb = Scratch(glb.clone());
    let _log = Scratch(log.clone());

    export_glb(&blender, path, &glb, &log)?;
    super::gltf::load_gltf(&glb)
}

/// The Blender this process will convert with, if there is one.
///
/// `TROVE_BLENDER` wins over `PATH`, which wins over the well-known install
/// locations.
pub fn find_blender() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(BLENDER_ENV) {
        let path = PathBuf::from(configured);
        if is_executable(&path) {
            return Some(path);
        }
        // Worth saying: silently falling back would hide a typo in the
        // setting behind "it worked yesterday".
        tracing::warn!(
            path = %path.display(),
            "{BLENDER_ENV} does not point at an executable; looking on PATH instead"
        );
    }
    if let Some(found) = which_blender() {
        return Some(found);
    }
    FALLBACK_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|candidate| is_executable(candidate))
}

/// The reason a `.blend` cannot be opened here, in the user's own terms.
fn no_blender() -> String {
    format!(
        "opening a .blend needs Blender installed (looked on PATH and in the usual \
         install locations; set {BLENDER_ENV} to point at it)"
    )
}

/// Run one headless export, and return once the GLB is on disk.
fn export_glb(blender: &Path, source: &Path, glb: &Path, log: &Path) -> Result<(), String> {
    // Blender's own output goes to a file rather than a pipe: it is chatty
    // enough on a damaged file to fill a pipe buffer, and a full pipe would
    // block the child while this thread sits in `try_wait` waiting for it.
    let log_file =
        std::fs::File::create(log).map_err(|e| format!("could not write a scratch log: {e}"))?;
    let mut child = Command::new(blender)
        .arg("-b")
        .arg(source)
        // The user's add-ons, start-up file and preferences must not run: a
        // start-up script that opens a window would hang a headless export,
        // and an exporter add-on could change what "export" means.
        .arg("--factory-startup")
        .arg("--python-expr")
        .arg(export_script(glb))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", blender.display()))?;

    let deadline = Instant::now() + EXPORT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // The child is killed rather than abandoned: an export
                    // still running would keep writing to a scratch file this
                    // call is about to delete.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Blender did not finish the export within {}s",
                        EXPORT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(format!("could not wait for Blender: {error}")),
        }
    };

    // The exit status is not enough on its own: a scene with no geometry at
    // all exports "successfully" into nothing, and a missing file is the
    // clearer thing to report.
    if !status.success() {
        return Err(format!(
            "Blender could not export this file ({}): {}",
            describe_status(&status),
            stderr_tail(log)
        ));
    }
    if !glb.is_file() {
        return Err(format!(
            "Blender exported no geometry ({}): the file has no mesh objects",
            stderr_tail(log)
        ));
    }
    Ok(())
}

/// The Python Blender runs to write the GLB.
///
/// `export_apply` bakes modifiers into the export, which is what the user
/// sees in Blender's own viewport; an exporter old enough not to take the
/// option is retried without it rather than failing the load.
fn export_script(glb: &Path) -> String {
    let target = python_string(glb);
    format!(
        "import bpy\n\
         options = {{'filepath': {target}, 'export_format': 'GLB', 'export_apply': True}}\n\
         try:\n\
         \x20   bpy.ops.export_scene.gltf(**options)\n\
         except TypeError:\n\
         \x20   options.pop('export_apply')\n\
         \x20   bpy.ops.export_scene.gltf(**options)\n"
    )
}

/// A path as a Python string literal.
///
/// The path is embedded in a `--python-expr` argument, so a backslash or a
/// quote in it — a Windows user directory is both — would otherwise end the
/// literal early and hand Blender a syntax error instead of a file.
fn python_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(character),
        }
    }
    out.push('\'');
    out
}

/// How the child ended, in a short form fit for an error message.
fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The end of Blender's own output, for a failure message.
fn stderr_tail(log: &Path) -> String {
    let mut text = String::new();
    let Ok(mut file) = std::fs::File::open(log) else {
        return "no output captured".to_string();
    };
    if file.read_to_string(&mut text).is_err() {
        return "no output captured".to_string();
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "no output".to_string();
    }
    // Char boundaries, not byte offsets: Blender quotes file names, and a
    // path with an accent in it must not be split mid-character.
    if trimmed.len() <= STDERR_TAIL {
        return trimmed.to_string();
    }
    let start = trimmed
        .char_indices()
        .map(|(at, _)| at)
        .find(|at| trimmed.len() - at <= STDERR_TAIL)
        .unwrap_or(0);
    format!("…{}", &trimmed[start..])
}

/// `blender` from `PATH`, if it is there.
fn which_blender() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|directory| PROGRAM_NAMES.map(|name| directory.join(name)))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a file this platform would run.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A scratch file path that no other load in this process can collide with.
fn scratch_path(extension: &str) -> PathBuf {
    let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "trove-blend-{}-{nanos}-{sequence}.{extension}",
        std::process::id()
    ))
}

/// A scratch file that cleans itself up, on every return path.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path with the characters that would end a Python string literal
    /// early comes back escaped, so Blender parses it as a path.
    #[test]
    fn paths_are_escaped_for_the_python_expression() {
        let quoted = python_string(Path::new(r"C:\Users\o'brien\modèle.blend"));
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        assert!(quoted.contains(r"\\Users"), "{quoted}");
        assert!(quoted.contains(r"o\'brien"), "{quoted}");
        // The escape must be readable back: the literal between the quotes,
        // unescaped, is the path again.
        let inner = &quoted[1..quoted.len() - 1];
        let unescaped = inner.replace(r"\\", "\\").replace(r"\'", "'");
        assert_eq!(unescaped, r"C:\Users\o'brien\modèle.blend");
    }

    /// The wrong path is not run: an override pointing at nothing is dropped
    /// rather than attempted, and a file that is not executable is not a
    /// Blender.
    #[test]
    fn only_an_executable_counts_as_blender() {
        // A name of its own: two tests sharing a temp directory prefix would
        // delete each other's while running in parallel.
        let directory =
            std::env::temp_dir().join(format!("trove-blendperm-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let plain = directory.join("plain");
        std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
        assert!(!is_executable(&plain), "a data file is not executable");
        assert!(!is_executable(&directory), "a directory is not a program");
        assert!(!is_executable(&directory.join("absent")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(is_executable(&plain), "a 0755 file is executable");
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Scratch names are unique, so two loads at once cannot collide.
    #[test]
    fn scratch_names_do_not_repeat() {
        let first = scratch_path("glb");
        let second = scratch_path("glb");
        assert_ne!(first, second);
        assert_eq!(first.extension().and_then(|e| e.to_str()), Some("glb"));
    }

    /// A scratch file that is never created is dropped without complaint.
    #[test]
    fn a_scratch_file_is_removed_and_a_missing_one_is_harmless() {
        let path = scratch_path("tmp");
        std::fs::write(&path, b"x").unwrap();
        {
            let _guard = Scratch(path.clone());
        }
        assert!(!path.exists());
        // Dropping again, for a file that is already gone, must not panic.
        drop(Scratch(path));
    }

    /// Round-trips a real `.blend` through a real Blender.
    ///
    /// Named `real_blender` so a machine without one can skip it: the
    /// fixture is generated by Blender itself, which is also the only way to
    /// keep a binary `.blend` out of the repository.
    #[test]
    fn real_blender_converts_a_generated_scene() {
        let Some(blender) = find_blender() else {
            eprintln!("skipping: no Blender on this machine");
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("trove-blendtest-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("scene.blend");
        // Saved compressed, which is what Blender does by default and what the
        // in-process parsers of the format cannot read at all.
        let build = format!(
            "import bpy\n\
             bpy.ops.wm.read_factory_settings(use_empty=True)\n\
             bpy.ops.mesh.primitive_cube_add(size=2.0)\n\
             bpy.ops.mesh.primitive_uv_sphere_add(segments=16, ring_count=8, radius=1.0, location=(3.0, 0.0, 1.0))\n\
             bpy.ops.wm.save_as_mainfile(filepath={}, compress=True)\n",
            python_string(&source)
        );
        let built = Command::new(&blender)
            .args(["-b", "--factory-startup", "--python-expr", &build])
            .output()
            .expect("Blender runs");
        assert!(
            built.status.success() && source.is_file(),
            "the fixture was generated ({}) — dir {} exists: {}\nstdout: {}\nstderr: {}\nexpr: {build}",
            describe_status(&built.status),
            directory.display(),
            directory.is_dir(),
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr),
        );

        let mesh = load_blend(&source).expect("the scene converts");
        // The vertex count is deliberately not pinned: glTF splits a vertex
        // wherever the normal or the UV differs across a face, so a cube with
        // its six hard-edged sides exports 24 of them. That is the format
        // working as intended, not something to assert on. Triangles are
        // stable, and they are what says both objects came through.
        assert!(mesh.vertex_count() >= 8 + 114, "{}", mesh.vertex_count());
        // 12 cube triangles, 16 per sphere cap, and 16 * 2 per band between
        // the six rings that are left once the caps are counted.
        assert_eq!(
            mesh.triangle_count(),
            12 + 16 * 2 + 16 * 2 * 6,
            "cube faces plus both sphere caps and the bands between them"
        );
        // Both objects arrive in the right places — and in the right
        // orientation: Blender is Z-up and glTF is Y-up, so the sphere Blender
        // placed at (3, 0, 1) comes out at (3, 1, 0). Asserting the whole box
        // is what proves the exporter converted the axes for both objects
        // rather than only for the first.
        let close = |actual: [f32; 3], expected: [f32; 3]| {
            actual
                .iter()
                .zip(expected)
                .all(|(a, e)| (a - e).abs() < 1e-3)
        };
        assert!(
            close(mesh.bounds.min, [-1.0, -1.0, -1.0]),
            "{:?}",
            mesh.bounds
        );
        assert!(close(mesh.bounds.max, [4.0, 2.0, 1.0]), "{:?}", mesh.bounds);

        // Nothing is left behind: the GLB and the log were both scratch.
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("trove-blend-"))
            .collect();
        assert!(leftovers.is_empty(), "scratch files left: {leftovers:?}");
        std::fs::remove_dir_all(&directory).ok();
    }

    /// A file that is not a `.blend` fails with Blender's own words rather
    /// than an empty mesh.
    #[test]
    fn real_blender_reports_a_broken_file() {
        if find_blender().is_none() {
            eprintln!("skipping: no Blender on this machine");
            return;
        }
        let path = scratch_path("blend");
        std::fs::write(&path, b"this is not a blend file, not even close").unwrap();
        let result = load_blend(&path);
        std::fs::remove_file(&path).ok();
        let error = result.expect_err("a text file is not a .blend");
        assert!(
            error.contains("could not export") || error.contains("no geometry"),
            "unhelpful message: {error}"
        );
    }
}
