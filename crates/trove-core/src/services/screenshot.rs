//! Screenshots: capture a target — the workspace, one screen, one window or
//! a rectangle — into a PNG that the app then imports like any other file.
//!
//! Capture runs in-process wherever a platform interface exists: KWin's
//! `org.kde.KWin.ScreenShot2` on Plasma (see [`crate::services::kwin`]),
//! `xcap` for a whole screen on X11, macOS and Windows. What is left over —
//! interactive pickers, and every session type without an in-process path —
//! goes through the platform's own tool ([`plan_for`]): `screencapture` on
//! macOS, a PowerShell one-liner on Windows, `grim`/`scrot` on Linux.
//!
//! Planning that chain is a pure function precisely so the whole matrix
//! stays unit-testable without a display:
//!
//! | target | in-process | external fallback |
//! | --- | --- | --- |
//! | [`CaptureTarget::Workspace`] | KWin, then `xcap` | `screencapture -x`, PowerShell, `grim`, `scrot` |
//! | [`CaptureTarget::Screen`] | KWin (`activeOutputName` when unnamed) | — |
//! | [`CaptureTarget::ActiveWindow`] | KWin | — |
//! | [`CaptureTarget::Window`] | KWin | — |
//! | [`CaptureTarget::Area`] | KWin | `screencapture -R`, `grim -g`, `scrot -a` |
//! | [`CaptureTarget::PickWindow`] | KWin's own picker | — |
//! | [`CaptureTarget::PickArea`] | — | `screencapture -i`, `grim -g "$(slurp)"`, `scrot -s` |

use std::path::{Path, PathBuf};
use std::process::Command;

/// Which part of the desktop to capture.
///
/// The variants mirror the platform interfaces one-to-one, because asking
/// for the target's own granularity is what keeps the result sharp: a
/// rectangle rendered by the compositor keeps its device pixels, while
/// cropping a whole-workspace frame throws them away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    /// Every screen, as the platform composes it.
    Workspace,
    /// One output by its platform name; `None` means "whichever has focus".
    Screen { name: Option<String> },
    /// The focused window.
    ActiveWindow,
    /// One window, named the way the platform names windows (KWin: the
    /// `Window::internalId()` UUID as a string).
    Window { handle: String },
    /// A rectangle in workspace coordinates.
    Area {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// Whatever window the user clicks, picked by the platform's own
    /// picker. No Trove UI is involved, so this is the honest fallback
    /// when the in-app picker has nothing to snap to.
    PickWindow,
    /// A rectangle the user drags, picked by the platform's own tool.
    PickArea,
}

impl CaptureTarget {
    /// Whether this target needs the user to click or drag something.
    pub fn is_interactive(&self) -> bool {
        matches!(self, CaptureTarget::PickWindow | CaptureTarget::PickArea)
    }
}

/// Where a frame came from — the reason to log it, and (later) the start of
/// the asset's provenance metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// The composed workspace.
    Workspace,
    /// One output, by name.
    Screen(String),
    /// The focused window.
    ActiveWindow,
    /// One window, by its platform handle.
    Window(String),
    /// A rectangle.
    Area,
    /// The platform's own picker chose.
    Picked,
    /// An external program wrote the file.
    Tool(String),
}

/// Why a capture did not produce a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The user dismissed the picker. Not a failure to report.
    Cancelled,
    /// No backend on this platform can capture this target.
    Unsupported(String),
    /// A backend tried and failed; the message carries the whole chain.
    Failed(String),
}

impl Error {
    /// A message suitable for a notification.
    pub fn message(&self) -> String {
        match self {
            Error::Cancelled => "cancelled".into(),
            Error::Unsupported(reason) | Error::Failed(reason) => reason.clone(),
        }
    }
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

/// Build the capture command for `target`, writing PNG to `dest`: the
/// platform toolchain, or `None` when the target has no external route
/// (a window or a screen, which only the compositor can render).
pub fn plan(target: &CaptureTarget, dest: &Path) -> Option<CapturePlan> {
    plan_for(target, dest, Platform::detect())
}

/// [`plan`] with an explicit session type — the unit-testable core.
#[allow(unused_variables)]
pub fn plan_for(target: &CaptureTarget, dest: &Path, platform: Platform) -> Option<CapturePlan> {
    #[cfg(target_os = "macos")]
    {
        // `screencapture` takes the destination as a positional argument
        // and `-x` silences the shutter sound, which every mode wants.
        return Some(match target {
            CaptureTarget::Workspace => CapturePlan {
                program: "screencapture".into(),
                args: vec!["-x".into(), quotable(dest)],
            },
            CaptureTarget::PickArea => CapturePlan {
                program: "screencapture".into(),
                args: vec!["-i".into(), quotable(dest)],
            },
            CaptureTarget::Area {
                x,
                y,
                width,
                height,
            } => CapturePlan {
                program: "screencapture".into(),
                args: vec![
                    "-x".into(),
                    "-R".into(),
                    format!("{x},{y},{width},{height}"),
                    quotable(dest),
                ],
            },
            // Screens, windows and the window picker are the compositor's
            // or the window server's business; no CLI route worth taking.
            _ => return None,
        });
    }

    #[cfg(target_os = "windows")]
    {
        // Only the whole screen: region picking needs a custom overlay, and
        // window capture is `xcap`'s job (Windows Graphics Capture).
        return match target {
            CaptureTarget::Workspace => Some(CapturePlan {
                program: "powershell".into(),
                args: vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    windows_capture_script(&quotable(dest)),
                ],
            }),
            _ => None,
        };
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if platform.wayland {
            return match target {
                CaptureTarget::Workspace => Some(CapturePlan {
                    program: "grim".into(),
                    args: vec![quotable(dest)],
                }),
                // `slurp` prints the selection geometry for `grim -g`.
                CaptureTarget::PickArea => Some(shell(
                    format!("grim -g \"$(slurp)\" {}", quotable(dest)),
                    dest,
                )),
                CaptureTarget::Area {
                    x,
                    y,
                    width,
                    height,
                } => Some(CapturePlan {
                    program: "grim".into(),
                    args: vec![
                        "-g".into(),
                        format!("{x},{y} {width}x{height}"),
                        quotable(dest),
                    ],
                }),
                _ => None,
            };
        }
        if platform.x11 {
            return match target {
                CaptureTarget::Workspace => Some(CapturePlan {
                    program: "scrot".into(),
                    args: vec![quotable(dest)],
                }),
                CaptureTarget::PickArea => Some(CapturePlan {
                    program: "scrot".into(),
                    args: vec!["-s".into(), quotable(dest)],
                }),
                CaptureTarget::Area {
                    x,
                    y,
                    width,
                    height,
                } => Some(CapturePlan {
                    program: "scrot".into(),
                    args: vec![
                        "-a".into(),
                        format!("{x},{y},{width},{height}"),
                        quotable(dest),
                    ],
                }),
                _ => None,
            };
        }
        None
    }
}

/// Run the capture: in-process first (KWin on Plasma, then `xcap`), then the
/// external toolchain. The PNG exists on disk when `Ok` comes back, and the
/// returned source says which path produced it.
///
/// Every in-process failure reason rides along in the error that finally
/// surfaces, so the root cause stays diagnosable. Cancellation is the one
/// exception: it is the user's answer, not a failure, so it is returned
/// straight away instead of being retried through another backend.
/// Every step emits `tracing` events (default level `info`; `RUST_LOG`
/// adjusts).
pub fn capture(target: &CaptureTarget, dest: &Path) -> Result<CaptureSource, Error> {
    tracing::info!(
        target = ?target,
        dest = %dest.display(),
        wayland = ?std::env::var_os("WAYLAND_DISPLAY"),
        x11 = ?std::env::var_os("DISPLAY"),
        "screenshot capture requested"
    );

    // KWin first on Linux: Plasma implements neither wlr-screencopy nor
    // ext-image-copy-capture, so KWin's own D-Bus interface is the only
    // in-process capture there. On other sessions the call simply finds no
    // such service and we fall through. The call reaches into the display
    // server, and a bug there should not take the caller's task down, hence
    // the unwind guard on both in-process backends.
    #[cfg(target_os = "linux")]
    let mut in_process_error: Option<String> = {
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture_in_process(target)));
        match attempt.unwrap_or_else(|payload| Err(kwin::Failure::Failed(panic_message(payload)))) {
            Ok((source, image)) => {
                save_png(&image, dest)?;
                return Ok(source);
            }
            Err(kwin::Failure::Cancelled) => {
                tracing::info!(target = ?target, "kwin: capture cancelled by the user");
                return Err(Error::Cancelled);
            }
            Err(failure) => {
                tracing::debug!(reason = %failure.message(), "kwin capture unavailable; falling back");
                Some(failure.labelled())
            }
        }
    };
    #[cfg(not(target_os = "linux"))]
    let mut in_process_error: Option<String> = None;

    if *target == CaptureTarget::Workspace {
        // xcap talks to the display server and can panic on hostile
        // environments; the unwind guard keeps such a failure a fallback
        // instead of taking the caller's task down. Its Wayland path is
        // libwayshot (wlr-screencopy), so it covers the wlroots compositors
        // and X11, not KWin.
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture_via_xcap(dest)));
        match attempt.unwrap_or_else(|payload| Err(panic_message(payload))) {
            Ok(()) => return Ok(CaptureSource::Workspace),
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

    let Some(plan) = plan_for(target, dest, Platform::detect()) else {
        tracing::error!(
            target = ?target,
            in_process_error = ?in_process_error,
            "no capture backend for this target"
        );
        let reason = match in_process_error {
            Some(reason) => format!("{reason}; no external capture tool for this target"),
            None => "no capture backend for this target on this platform".into(),
        };
        return Err(Error::Unsupported(reason));
    };
    tracing::debug!(program = %plan.program, args = ?plan.args, "external capture plan");
    run_plan(&plan, dest)
        .map(|()| CaptureSource::Tool(plan.program.clone()))
        .map_err(|e| match in_process_error {
            Some(reason) => Error::Failed(format!("{reason}; {e}")),
            None => Error::Failed(e),
        })
}

/// The in-process path for one target: KWin on Linux, nothing elsewhere
/// (macOS and Windows reach the workspace through `xcap`, and the rest of
/// their targets are the toolchain's).
#[cfg(target_os = "linux")]
fn capture_in_process(
    target: &CaptureTarget,
) -> Result<(CaptureSource, image::RgbaImage), kwin::Failure> {
    use kwin::Options;

    // A native-resolution crop is the point of capturing an exact
    // rectangle: the caller already decided where the edges are, so
    // slicing a downscaled frame would only lose detail.
    let area_options = Options {
        native: true,
        ..Options::defaults()
    };
    // A window is asked for with its decoration: the geometry the picker
    // highlighted is the frame geometry, decorations included.
    let window_options = Options {
        decoration: Some(true),
        shadow: Some(false),
        ..Options::defaults()
    };

    Ok(match target {
        CaptureTarget::Workspace => (CaptureSource::Workspace, kwin::capture_workspace_image()?),
        CaptureTarget::Screen { name } => {
            let name = match name {
                Some(name) => name.clone(),
                None => kwin::active_output_name()?,
            };
            (
                CaptureSource::Screen(name.clone()),
                kwin::capture_screen_image(&name, Options::defaults())?,
            )
        }
        CaptureTarget::ActiveWindow => (
            CaptureSource::ActiveWindow,
            kwin::capture_active_window_image(Options::defaults())?,
        ),
        CaptureTarget::Window { handle } => (
            CaptureSource::Window(handle.clone()),
            kwin::capture_window_image(handle, window_options)?,
        ),
        CaptureTarget::Area {
            x,
            y,
            width,
            height,
        } => (
            CaptureSource::Area,
            kwin::capture_area_image(*x, *y, *width, *height, area_options)?,
        ),
        CaptureTarget::PickWindow => (
            CaptureSource::Picked,
            kwin::capture_interactive_window_image(window_options)?,
        ),
        // The platform's own region picker is a CLI tool, not a KWin
        // method, so this target never reaches the compositor.
        CaptureTarget::PickArea => {
            return Err(kwin::Failure::Unavailable(
                "a picked region goes through the platform toolchain".into(),
            ));
        }
    })
}

#[cfg(target_os = "linux")]
use crate::services::kwin;

/// Write a captured frame to `dest`, creating the directory first.
fn save_png(image: &image::RgbaImage, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            tracing::error!(dir = %parent.display(), error = %e, "could not create output dir");
            Error::Failed(e.to_string())
        })?;
    }
    image.save(dest).map_err(|e| {
        tracing::error!(dest = %dest.display(), error = %e, "could not save png");
        Error::Failed(format!("{}: {e}", dest.display()))
    })?;
    tracing::info!(dest = %dest.display(), "screenshot png written");
    Ok(())
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

    fn area() -> CaptureTarget {
        CaptureTarget::Area {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn wayland_uses_grim_for_the_workspace_and_slurp_for_a_picked_area() {
        let wayland = Platform {
            wayland: true,
            x11: false,
        };
        let full = plan_for(&CaptureTarget::Workspace, &dest(), wayland).unwrap();
        assert_eq!(full.program, "grim");
        assert_eq!(full.args, vec!["/tmp/shot.png".to_string()]);

        let region = plan_for(&CaptureTarget::PickArea, &dest(), wayland).unwrap();
        assert_eq!(region.program, "sh");
        assert!(region.args[1].contains("slurp"));
        assert!(region.args[1].contains("/tmp/shot.png"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn an_exact_area_goes_straight_to_the_tool_geometry() {
        let wayland = Platform {
            wayland: true,
            x11: false,
        };
        let plan = plan_for(&area(), &dest(), wayland).unwrap();
        assert_eq!(plan.program, "grim");
        assert_eq!(plan.args[0], "-g");
        assert_eq!(plan.args[1], "10,20 30x40");

        let x11 = Platform {
            wayland: false,
            x11: true,
        };
        let plan = plan_for(&area(), &dest(), x11).unwrap();
        assert_eq!(plan.program, "scrot");
        assert_eq!(plan.args[0], "-a");
        assert_eq!(plan.args[1], "10,20,30,40");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn x11_uses_scrot() {
        let x11 = Platform {
            wayland: false,
            x11: true,
        };
        let full = plan_for(&CaptureTarget::Workspace, &dest(), x11).unwrap();
        assert_eq!((full.program.as_str(), full.args.len()), ("scrot", 1));

        let region = plan_for(&CaptureTarget::PickArea, &dest(), x11).unwrap();
        assert_eq!(
            region.args,
            vec!["-s".to_string(), "/tmp/shot.png".to_string()]
        );
    }

    /// Windows, screens and a compositor-side picker cannot be named by a
    /// CLI tool, so a target without a plan must say so rather than guess.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn window_and_screen_targets_have_no_external_plan() {
        let wayland = Platform {
            wayland: true,
            x11: false,
        };
        for target in [
            CaptureTarget::ActiveWindow,
            CaptureTarget::Window {
                handle: "uuid".into(),
            },
            CaptureTarget::PickWindow,
            CaptureTarget::Screen { name: None },
        ] {
            assert!(
                plan_for(&target, &dest(), wayland).is_none(),
                "{target:?} must not claim an external tool"
            );
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn no_session_has_no_default_plan() {
        assert!(plan_for(&CaptureTarget::Workspace, &dest(), Platform::default()).is_none());
    }

    #[test]
    fn destination_lives_directly_in_the_given_folder() {
        let out = destination(Path::new("/tmp/trove-incoming"));
        assert!(out.starts_with("/tmp/trove-incoming"));
        assert_eq!(out.parent().unwrap(), Path::new("/tmp/trove-incoming"));
        assert_eq!(out.extension().unwrap(), "png");
    }

    #[test]
    fn only_the_platform_pickers_are_interactive() {
        assert!(CaptureTarget::PickWindow.is_interactive());
        assert!(CaptureTarget::PickArea.is_interactive());
        assert!(!CaptureTarget::Workspace.is_interactive());
        assert!(!area().is_interactive());
    }
}
