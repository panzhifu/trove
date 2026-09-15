//! Open files with external applications.
//!
//! Wraps the platform's "open with" machinery so the app can hand a file to
//! the system default program for its type (`None`) or to a specific
//! application (`Some(app_path)`). The `plan` function is a pure, unit-testable
//! decision; `open` runs the resulting command.

use std::path::Path;
use std::process::Command;

/// What `open` should do with the resolved file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget<'a> {
    /// Open with the system default program for this file type.
    Default,
    /// Open with a specific application (absolute path to the .exe/.app, or
    /// a command found on `$PATH`).
    With(&'a Path),
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
            OpenTarget::With(app) => {
                let mut cmd = Command::new(app);
                cmd.arg(path);
                cmd
            }
        }
    }
}

/// Open `path` using `target`. The command is spawned detached; failures are
/// returned as a string so the caller can surface them in the status bar.
pub fn open(path: &Path, target: OpenTarget<'_>) -> Result<(), String> {
    plan(path, target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{e}"))
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

/// Open `url` in the system browser (detached, like [`open`]; failures come
/// back as a string).
///
/// Only `http(s)` is accepted. Everything reachable from here is a link Trove
/// itself produced, and this keeps a path — or a `file:`/`javascript:`
/// payload smuggled into one — from being handed to the OS opener, which
/// would happily run it.
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("refusing to open a non-http(s) url: {url}"));
    }
    plan_url(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{e}"))
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
