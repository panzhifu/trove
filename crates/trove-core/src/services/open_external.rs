//! Open files with external applications.
//!
//! Wraps the platform's "open with" machinery so the app can hand a file to
//! the system default program for its type (`None`) or to a specific
//! application (`Some(app_path)`). The `plan` function is a pure, unit-testable
//! decision; `open` runs the resulting command.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;

/// What `open` should do with the resolved file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget<'a> {
    /// Open with the system default program for this file type.
    Default,
    /// Open with a specific application (absolute path to the .exe/.app, or
    /// a command found on `$PATH`).
    With(&'a Path),
}

/// The Python expression that imports a mesh file into a fresh Blender
/// session. Blender's command line only *opens* `.blend` files — handing it
/// an `.obj` or an `.fbx` as the file argument gets the user a "not a Blender
/// file" error instead of a model. Every other format has to go through an
/// import operator, and the operator names moved between Blender versions
/// (OBJ grew the C++ `wm.obj_import` in 3.1, STL followed with
/// `wm.stl_import` in 4.0), so each extension carries a candidate chain and
/// the first operator the running Blender knows wins. The path rides after
/// `--`, which Blender forwards verbatim on `sys.argv` — no quoting, no
/// matter what the file is named.
const BLENDER_IMPORT_SCRIPT: &str = r#"import bpy, functools, os, sys
path = sys.argv[-1]
ext = os.path.splitext(path)[1].lower().lstrip('.')
candidates = {
    'obj': ('wm.obj_import', 'import_scene.obj'),
    'stl': ('wm.stl_import', 'import_mesh.stl'),
    'ply': ('wm.ply_import', 'import_mesh.ply'),
    'gltf': ('import_scene.gltf',),
    'glb': ('import_scene.gltf',),
    'fbx': ('import_scene.fbx',),
    'dae': ('wm.collada_import',),
    'usd': ('wm.usd_import',),
    'usdz': ('wm.usd_import',),
    'abc': ('wm.alembic_import',),
    '3ds': ('import_scene.autodesk_3ds',),
}.get(ext, ())
for name in candidates:
    try:
        functools.reduce(getattr, name.split('.'), bpy.ops)(filepath=path)
        break
    except Exception:
        pass
else:
    print('trove: no import operator found for .' + ext)
"#;

/// The mesh formats Blender cannot open from its command line — the
/// "open with" model list minus `.blend` itself, which opens natively.
fn needs_blender_import(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "obj" | "fbx" | "gltf" | "glb" | "stl" | "ply" | "dae" | "3ds" | "usd" | "usdz" | "abc"
        )
    )
}

/// Does this application look like Blender? The stem is matched so
/// `blender.exe`, `blender-4.1` and `/Applications/Blender.app` all read as
/// the same program.
fn is_blender(app: &Path) -> bool {
    app.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.to_ascii_lowercase().contains("blender"))
}

/// The executable to hand Blender's own arguments to. A macOS `.app` bundle
/// cannot be exec'd directly; its binary sits one level inside.
fn blender_executable(app: &Path) -> PathBuf {
    if cfg!(target_os = "macos") && app.extension().and_then(|e| e.to_str()) == Some("app") {
        let stem = app.file_stem().unwrap_or_default();
        let inner = app.join("Contents/MacOS").join(stem);
        if inner.is_file() {
            return inner;
        }
    }
    app.to_path_buf()
}

/// Build the command that opens `path` using `target`.
///
/// - Windows: `ShellExecute` through `cmd /c start`, or runs the given app
///   with the file as the sole argument.
/// - macOS: `open` with either no flags (default) or `-a <app>`.
/// - Linux/other: `xdg-open` (default) or the given app with the file as the
///   sole argument.
pub fn plan<'a>(path: &Path, target: OpenTarget<'a>) -> Command {
    if cfg!(target_os = "windows") {
        match target {
            OpenTarget::Default => {
                // `start "" <path>` opens with the default association.
                let mut cmd = Command::new("cmd");
                cmd.arg("/c").arg("start").arg("\"\"").arg(path);
                cmd
            }
            OpenTarget::With(app) if is_blender(app) && needs_blender_import(path) => {
                let mut cmd = Command::new(blender_executable(app));
                cmd.arg("--python-expr")
                    .arg(BLENDER_IMPORT_SCRIPT)
                    .arg("--")
                    .arg(path);
                cmd
            }
            OpenTarget::With(app) => {
                let mut cmd = Command::new(app);
                cmd.arg(path);
                cmd
            }
        }
    } else if cfg!(target_os = "macos") {
        match target {
            OpenTarget::Default => {
                let mut cmd = Command::new("open");
                cmd.arg(path);
                cmd
            }
            OpenTarget::With(app) if is_blender(app) && needs_blender_import(path) => {
                let mut cmd = Command::new(blender_executable(app));
                cmd.arg("--python-expr")
                    .arg(BLENDER_IMPORT_SCRIPT)
                    .arg("--")
                    .arg(path);
                cmd
            }
            OpenTarget::With(app) => {
                let mut cmd = Command::new("open");
                cmd.arg("-a").arg(app).arg(path);
                cmd
            }
        }
    } else {
        // Linux / BSD / others.
        match target {
            OpenTarget::Default => {
                let mut cmd = Command::new("xdg-open");
                cmd.arg(path);
                cmd
            }
            OpenTarget::With(app) if is_blender(app) && needs_blender_import(path) => {
                let mut cmd = Command::new(blender_executable(app));
                cmd.arg("--python-expr")
                    .arg(BLENDER_IMPORT_SCRIPT)
                    .arg("--")
                    .arg(path);
                cmd
            }
            OpenTarget::With(app) => {
                let mut cmd = Command::new(app);
                cmd.arg(path);
                cmd
            }
        }
    }
}

/// Open `path` using `target`. The command is spawned detached; a failure is
/// returned so the caller can surface it in the status bar.
pub fn open(path: &Path, target: OpenTarget<'_>) -> Result<(), Error> {
    plan(path, target)
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::External {
            program: "system opener".into(),
            message: e.to_string(),
        })
}

/// Build the command that opens `url` in the user's browser.
///
/// Same platform split as [`plan`] — `xdg-open`, `open` and `start` all take
/// a URL as happily as a path. Windows is the one that needs care: `start`
/// reads its first quoted argument as a window title, so the (empty) title
/// has to be passed explicitly or the URL is swallowed as one.
pub fn plan_url(url: &str) -> Command {
    if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.arg("/c").arg("start").arg("").arg(url);
        cmd
    } else if cfg!(target_os = "macos") {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    } else {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    }
}

/// Open `url` in the system browser (detached, like [`open`]).
///
/// Only `http(s)` is accepted. Everything reachable from here is a link Trove
/// itself produced, and this keeps a path — or a `file:`/`javascript:`
/// payload smuggled into one — from being handed to the OS opener, which
/// would happily run it.
pub fn open_url(url: &str) -> Result<(), Error> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::Validation(format!(
            "refusing to open a non-http(s) url: {url}"
        )));
    }
    plan_url(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::External {
            program: "system opener".into(),
            message: e.to_string(),
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_builds_a_command() {
        let _ = plan(Path::new("/tmp/file.png"), OpenTarget::Default);
        // Just verifying it doesn't panic; the actual binary differs per-OS.
    }

    #[test]
    fn with_app_plan_builds_a_command() {
        let _ = plan(
            Path::new("/tmp/file.png"),
            OpenTarget::With(Path::new("/usr/bin/gimp")),
        );
    }

    #[test]
    fn blender_gets_the_import_script_for_mesh_formats() {
        for name in [
            "model.obj",
            "scene.fbx",
            "mesh.stl",
            "asset.glb",
            "shape.ply",
        ] {
            let cmd = plan(
                &Path::new("/tmp").join(name),
                OpenTarget::With(Path::new("/usr/bin/blender")),
            );
            let args: Vec<String> = cmd
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(
                args.iter().any(|arg| arg == "--python-expr"),
                "{name}: the import script must ride along: {args:?}"
            );
            assert!(
                args.iter().any(|arg| arg == "--"),
                "{name}: the path must be fenced behind --: {args:?}"
            );
            assert!(
                args.last().is_some_and(|arg| arg.ends_with(name)),
                "{name}: the file must be the last argument: {args:?}"
            );
        }
    }

    #[test]
    fn blender_opens_blend_files_directly() {
        let cmd = plan(
            Path::new("/tmp/scene.blend"),
            OpenTarget::With(Path::new("/usr/bin/blender")),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/tmp/scene.blend"], "a .blend opens natively");
    }

    #[test]
    fn other_apps_keep_the_plain_file_argument() {
        let cmd = plan(
            Path::new("/tmp/model.obj"),
            OpenTarget::With(Path::new("/usr/bin/meshlab")),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/tmp/model.obj"]);
    }

    #[test]
    fn url_plan_carries_the_link_as_an_argument() {
        let url = "https://github.com/panzhifu/trove/releases/latest";
        let args: Vec<String> = plan_url(url)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|arg| arg == url),
            "the url must survive as its own argument: {args:?}"
        );
    }

    #[test]
    fn only_http_urls_reach_the_opener() {
        assert!(open_url("file:///etc/passwd").is_err());
        assert!(open_url("javascript:alert(1)").is_err());
        assert!(open_url("/etc/passwd").is_err());
    }
}
