//! Screenshots: capture the screen (or a region of it) into a PNG that the
//! app then imports like any other file.
//!
//! Full-screen capture runs in-process via [`xcap`] (XCB on X11,
//! ScreenCaptureKit on macOS, Windows Graphics Capture on Windows) — no
//! external tool has to be installed. On Wayland xcap only speaks
//! wlr-screencopy (libwayshot), so compositors that do not implement that
//! protocol (KWin) have no in-process path here and fall through to the
//! external toolchain ([`plan_for`]). Region picking stays with the
//! platform tools: an interactive selection overlay is out of scope here.
//! Planning the external chain is a pure function ([`plan_for`]) so it
//! stays unit-testable without a display.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Which part of the screen to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotMode {
    /// Every screen, as the platform composes it.
    Full,
    /// A rectangle the user picks interactively.
    Region,
}

/// An executable capture step: a program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePlan {
    pub program: String,
    pub args: Vec<String>,
}

/// Which session type we appear to be running under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Platform {
    pub wayland: bool,
    pub x11: bool,
}

impl Platform {
    /// Detect from the environment.
    pub fn detect() -> Self {
        Self {
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
        }
    }
}

/// Build the capture command for `mode`, writing PNG to `dest`: the
/// platform toolchain, or `None` when no strategy applies.
pub fn plan(mode: ScreenshotMode, dest: &Path) -> Option<CapturePlan> {
    plan_for(mode, dest, Platform::detect())
}

/// [`plan`] with an explicit session type — the unit-testable core.
#[allow(unused_variables)]
pub fn plan_for(mode: ScreenshotMode, dest: &Path, platform: Platform) -> Option<CapturePlan> {
    #[cfg(target_os = "macos")]
    {
        Some(CapturePlan {
            program: "screencapture".into(),
            args: vec![
                match mode {
                    ScreenshotMode::Full => "-x",
                    ScreenshotMode::Region => "-i",
                }
                .to_string(),
                quotable(dest),
            ],
        })
    }

    #[cfg(target_os = "windows")]
    {
        // Only full-screen: region picking needs a custom overlay.
        if mode == ScreenshotMode::Region {
            return None;
        }
        Some(CapturePlan {
            program: "powershell".into(),
            args: vec![
                "-NoProfile".into(),
                "-Command".into(),
                windows_capture_script(&quotable(dest)),
            ],
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if platform.wayland {
            return Some(match mode {
                ScreenshotMode::Full => CapturePlan {
                    program: "grim".into(),
                    args: vec![quotable(dest)],
                },
                // `slurp` prints the selection geometry for `grim -g`.
                ScreenshotMode::Region => {
                    shell(format!("grim -g \"$(slurp)\" {}", quotable(dest)), dest)
                }
            });
        }
        if platform.x11 {
            return Some(match mode {
                ScreenshotMode::Full => CapturePlan {
                    program: "scrot".into(),
                    args: vec![quotable(dest)],
                },
                ScreenshotMode::Region => CapturePlan {
                    program: "scrot".into(),
                    args: vec!["-s".into(), quotable(dest)],
                },
            });
        }
        None
    }
}

/// Run the capture: in-process (KWin on Plasma, else xcap) → the external
/// toolchain. The PNG must exist on disk when `Ok` comes back. Every
/// in-process failure reason rides along in the error that finally
/// surfaces, so the root cause stays diagnosable. Every step emits
/// `tracing` events (default level `info`; `RUST_LOG` adjusts).
pub fn capture(mode: ScreenshotMode, dest: &Path) -> Result<(), String> {
    tracing::info!(
        mode = ?mode,
        dest = %dest.display(),
        wayland = ?std::env::var_os("WAYLAND_DISPLAY"),
        x11 = ?std::env::var_os("DISPLAY"),
        "screenshot capture requested"
    );
    let mut in_process_error: Option<String> = None;
    if mode == ScreenshotMode::Full {
        // KWin first: Plasma implements neither wlr-screencopy nor
        // ext-image-copy-capture, so KWin's own D-Bus interface is the only
        // in-process capture there. On other sessions the call simply finds
        // no such service and we fall through.
        #[cfg(target_os = "linux")]
        {
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::services::kwin::capture_workspace(dest)
            }));
            match attempt.unwrap_or_else(|payload| Err(panic_message(payload))) {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    tracing::debug!(reason, "kwin capture unavailable; falling back");
                    in_process_error = Some(format!("kwin: {reason}"));
                }
            }
        }
        // xcap talks to the display server and can panic on hostile
        // environments; the unwind guard keeps such a failure a fallback
        // instead of taking the caller's task down. Its Wayland path is
        // libwayshot (wlr-screencopy), so it covers the wlroots compositors
        // and X11, not KWin.
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture_via_xcap(dest)));
        match attempt.unwrap_or_else(|payload| Err(panic_message(payload))) {
            Ok(()) => return Ok(()),
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "xcap capture failed; falling back to external tools"
                );
                in_process_error = Some(match in_process_error {
                    Some(previous) => format!("{previous}; xcap: {reason}"),
                    None => format!("xcap: {reason}"),
                });
            }
        }
    }
    let Some(plan) = plan_for(mode, dest, Platform::detect()) else {
        tracing::error!(
            in_process_error = ?in_process_error,
            "no external screenshot tool for this platform"
        );
        return Err(match in_process_error {
            Some(reason) => {
                format!("{reason}; no external screenshot tool for this platform")
            }
            None => "no screenshot tool for this platform".into(),
        });
    };
    tracing::debug!(program = %plan.program, args = ?plan.args, "external capture plan");
    run_plan(&plan, dest).map_err(|e| match in_process_error {
        Some(reason) => format!("{reason}; {e}"),
        None => e,
    })
}

/// Pull a readable message out of a caught panic payload. The default
/// panic hook writes to stderr, which the file log never sees, so the
/// unwind guards turn panics into regular `Err` strings instead.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        "unknown panic payload".into()
    }
}

/// Capture the primary screen in-process with `xcap` and write a PNG.
///
/// `Monitor::all()` sorts by position; the first monitor is the primary.
/// The image comes back as an RGBA buffer, so saving is all that is left.
/// On Wayland this only succeeds where wlr-screencopy exists (wlroots
/// compositors); KWin does not implement it, so KDE falls through to the
/// external toolchain.
fn capture_via_xcap(dest: &Path) -> Result<(), String> {
    use xcap::Monitor;

    let monitors = Monitor::all().map_err(|e| {
        tracing::error!(error = %e, "xcap: could not enumerate monitors");
        e.to_string()
    })?;
    for monitor in &monitors {
        tracing::debug!(
            name = monitor.name().unwrap_or_else(|_| "?".into()),
            width = monitor.width().ok(),
            height = monitor.height().ok(),
            "xcap: monitor found"
        );
    }
    let monitor = monitors.into_iter().next().ok_or_else(|| {
        tracing::error!("xcap: no monitor found");
        "no monitor found".to_string()
    })?;
    tracing::info!(
        name = monitor.name().unwrap_or_else(|_| "?".into()),
        width = monitor.width().ok(),
        height = monitor.height().ok(),
        "xcap: capturing primary monitor"
    );
    let started = std::time::Instant::now();
    let image = monitor.capture_image().map_err(|e| {
        tracing::error!(
            error = %e,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "xcap: capture_image failed"
        );
        e.to_string()
    })?;
    tracing::info!(
        width = image.width(),
        height = image.height(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "xcap: frame captured"
    );
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            tracing::error!(
                dir = %parent.display(),
                error = %e,
                "xcap: could not create output dir"
            );
            e.to_string()
        })?;
    }
    image.save(dest).map_err(|e| {
        tracing::error!(dest = %dest.display(), error = %e, "xcap: could not save png");
        format!("{}: {e}", dest.display())
    })?;
    tracing::info!(dest = %dest.display(), "xcap: png written");
    Ok(())
}

/// Execute a capture step and verify the PNG showed up.
fn run_plan(plan: &CapturePlan, dest: &Path) -> Result<(), String> {
    tracing::info!(program = %plan.program, args = ?plan.args, "running capture tool");
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            tracing::error!(
                dir = %parent.display(),
                error = %e,
                "could not create output dir"
            );
            e.to_string()
        })?;
    }
    // Interactive tools inherit the desktop: keep stdio open so region
    // pickers can draw, but never block on a hung child forever — the
    // caller runs this on a background executor.
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .map_err(|e| {
            tracing::error!(program = %plan.program, error = %e, "could not spawn capture tool");
            format!("{}: {e}", plan.program)
        })?;
    tracing::debug!(program = %plan.program, status = %status, "capture tool exited");
    if !status.success() {
        tracing::error!(program = %plan.program, status = %status, "capture tool failed");
        return Err(format!("{} exited with {status}", plan.program));
    }
    if !dest.is_file() {
        tracing::error!(dest = %dest.display(), "capture tool wrote no file");
        return Err("the screenshot tool wrote no file".into());
    }
    match std::fs::metadata(dest) {
        Ok(meta) => {
            tracing::info!(dest = %dest.display(), bytes = meta.len(), "screenshot file verified")
        }
        Err(_) => {
            tracing::warn!(dest = %dest.display(), "screenshot file exists but metadata unavailable")
        }
    }
    Ok(())
}

/// A `sh -c` step: `$1` is the destination, so commands can ignore
/// `{file}` and still find it.
fn shell(command: String, dest: &Path) -> CapturePlan {
    CapturePlan {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            command,
            "trove-screenshot".into(),
            quotable(dest),
        ],
    }
}

/// Quote a path for both direct arguments and shell interpolation.
fn quotable(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.contains(' ') && !raw.contains('\'') {
        format!("'{raw}'")
    } else {
        raw.into_owned()
    }
}

/// Where a capture is written: the caller passes the incoming directory (see
/// [`crate::paths::incoming_dir`]). It also stays there — the import links the
/// file instead of copying it in, so this is its permanent home.
pub fn destination(dir: &Path) -> PathBuf {
    let name = chrono::Local::now().format("screenshot-%Y%m%d-%H%M%S.png");
    dir.join(name.to_string())
}

/// .NET one-liner capturing the primary screen (Windows fallback).
#[cfg(target_os = "windows")]
fn windows_capture_script(dest: &str) -> String {
    format!(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \
         $img = New-Object System.Drawing.Bitmap $b.Width, $b.Height; \
         $g = [System.Drawing.Graphics]::FromImage($img); \
         $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size); \
         $img.Save({dest}, [System.Drawing.Imaging.ImageFormat]::Png);"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest() -> PathBuf {
        PathBuf::from("/tmp/shot.png")
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn wayland_uses_grim_and_slurp() {
        let wayland = Platform {
            wayland: true,
            x11: false,
        };
        let full = plan_for(ScreenshotMode::Full, &dest(), wayland).unwrap();
        assert_eq!(full.program, "grim");
        assert_eq!(full.args, vec!["/tmp/shot.png".to_string()]);

        let region = plan_for(ScreenshotMode::Region, &dest(), wayland).unwrap();
        assert_eq!(region.program, "sh");
        assert!(region.args[1].contains("slurp"));
        assert!(region.args[1].contains("/tmp/shot.png"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn x11_uses_scrot() {
        let x11 = Platform {
            wayland: false,
            x11: true,
        };
        let full = plan_for(ScreenshotMode::Full, &dest(), x11).unwrap();
        assert_eq!((full.program.as_str(), full.args.len()), ("scrot", 1));

        let region = plan_for(ScreenshotMode::Region, &dest(), x11).unwrap();
        assert_eq!(
            region.args,
            vec!["-s".to_string(), "/tmp/shot.png".to_string()]
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn no_session_has_no_default_plan() {
        assert!(plan_for(ScreenshotMode::Full, &dest(), Platform::default()).is_none());
    }

    #[test]
    fn destination_lives_directly_in_the_given_folder() {
        let out = destination(Path::new("/tmp/trove-incoming"));
        assert!(out.starts_with("/tmp/trove-incoming"));
        assert_eq!(out.parent().unwrap(), Path::new("/tmp/trove-incoming"));
        assert_eq!(out.extension().unwrap(), "png");
    }
}
