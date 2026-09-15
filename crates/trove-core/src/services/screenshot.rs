//! Screenshots: capture the screen (or a region of it) into a PNG that the
//! app then imports like any other file.
//!
//! Full-screen capture runs in-process — no external tool has to be
//! installed: on Wayland via [`grim_rs`] (ext-image-copy-capture-v1 — the
//! protocol niri speaks — with a wlr-screencopy fallback), elsewhere via
//! [`xcap`] (XCB on X11, ScreenCaptureKit on macOS, Windows Graphics
//! Capture on Windows). Region picking stays with the platform tools: an
//! interactive selection overlay is out of scope here. Whatever the
//! in-process libraries cannot do falls back to the external toolchain
//! (grim/slurp, scrot, screencapture, PowerShell), and the user can
//! override the whole thing with a custom command in Settings ▸ General.
//! Planning that chain is a pure function ([`plan_for`]) so it stays
//! unit-testable without a display.

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

/// Build the capture command for `mode`, writing PNG to `dest`.
///
/// Priority: a non-empty `custom` command (it may contain `{file}`, which is
/// replaced by `dest`; otherwise `dest` is appended as `$1`), then the
/// platform toolchain. `None` when no strategy applies.
pub fn plan(mode: ScreenshotMode, custom: Option<&str>, dest: &Path) -> Option<CapturePlan> {
    plan_for(mode, custom, dest, Platform::detect())
}

/// [`plan`] with an explicit session type — the unit-testable core.
#[allow(unused_variables)]
pub fn plan_for(
    mode: ScreenshotMode,
    custom: Option<&str>,
    dest: &Path,
    platform: Platform,
) -> Option<CapturePlan> {
    if let Some(command) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        return Some(shell(command.replace("{file}", &quotable(dest)), dest));
    }

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

/// Run the capture: custom command → in-process (grim-rs on Wayland, then
/// xcap) → the external toolchain. The PNG must exist on disk when `Ok`
/// comes back. Every in-process failure reason rides along in the error
/// that finally surfaces, so the root cause stays diagnosable. Every step
/// emits `tracing` events (default level `info`; `RUST_LOG` adjusts).
pub fn capture(mode: ScreenshotMode, custom: Option<&str>, dest: &Path) -> Result<(), String> {
    tracing::info!(
        mode = ?mode,
        dest = %dest.display(),
        custom = ?custom,
        wayland = ?std::env::var_os("WAYLAND_DISPLAY"),
        x11 = ?std::env::var_os("DISPLAY"),
        "screenshot capture requested"
    );
    if let Some(command) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        return run_custom(command, dest);
    }
    // In-process reasons accumulate and ride along in whatever error finally
    // surfaces, so a failed niri capture reads `grim-rs: …; xcap: …; grim: …`
    // in one line.
    let mut in_process_error: Option<String> = None;
    if mode == ScreenshotMode::Full {
        // Wayland first: grim-rs speaks ext-image-copy-capture-v1, which is
        // what niri implements — xcap's libwayshot only offers
        // wlr-screencopy there.
        #[cfg(target_os = "linux")]
        {
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture_via_grim_rs(dest)
            }));
            match attempt.unwrap_or_else(|_| Err("the capture library panicked".into())) {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    tracing::warn!(reason, "grim-rs capture failed; falling back");
                    in_process_error = Some(format!("grim-rs: {reason}"));
                }
            }
            // Portal probe: the `screenshots` crate asks the
            // xdg-desktop-portal Screenshot interface instead of a capture
            // protocol — success means the session has a portal backend.
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture_via_screenshots(dest)
            }));
            match attempt.unwrap_or_else(|_| Err("the capture library panicked".into())) {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    tracing::warn!(reason, "screenshots (portal) capture failed; falling back");
                    in_process_error = match in_process_error {
                        Some(previous) => Some(format!("{previous}; screenshots: {reason}")),
                        None => Some(format!("screenshots: {reason}")),
                    };
                }
            }
        }
        // xcap talks to the display server and can panic on hostile
        // environments; the unwind guard keeps such a failure a fallback
        // instead of taking the caller's task down.
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture_via_xcap(dest)));
        match attempt.unwrap_or_else(|_| Err("the capture library panicked".into())) {
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
    let Some(plan) = plan_for(mode, custom, dest, Platform::detect()) else {
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

/// Capture the whole desktop in-process with `grim-rs` and write a PNG.
///
/// Wayland-only (Wayland-only backends, so callers on X11 fall through to
/// xcap's XCB path). Unlike xcap's primary-monitor capture this composites
/// every connected output at its physical position. Backends are tried in
/// order — ext-image-copy-capture-v1 (niri, sway ≥ 2025, hyprland, COSMIC)
/// then wlr-screencopy (wlroots) — and the chosen one is logged.
#[cfg(target_os = "linux")]
fn capture_via_grim_rs(dest: &Path) -> Result<(), String> {
    use grim_rs::Grim;

    let mut grim = match Grim::new_ext() {
        Ok(grim) => {
            tracing::info!("grim-rs: backend ext-image-copy-capture-v1");
            grim
        }
        Err(ext_err) => {
            tracing::debug!(
                error = %ext_err,
                "grim-rs: ext-image-copy-capture-v1 unavailable"
            );
            let grim = Grim::new_wlr()
                .map_err(|e| format!("ext-image-copy-capture: {ext_err}; wlr-screencopy: {e}"))?;
            tracing::info!("grim-rs: backend wlr-screencopy");
            grim
        }
    };
    let started = std::time::Instant::now();
    let result = grim.capture_all().map_err(|e| {
        tracing::error!(
            error = %e,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "grim-rs: capture_all failed"
        );
        e.to_string()
    })?;
    tracing::info!(
        width = result.width(),
        height = result.height(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "grim-rs: frame captured"
    );
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
    grim.save_png(result.data(), result.width(), result.height(), dest)
        .map_err(|e| {
            tracing::error!(dest = %dest.display(), error = %e, "grim-rs: could not save png");
            format!("{}: {e}", dest.display())
        })?;
    tracing::info!(dest = %dest.display(), "grim-rs: png written");
    Ok(())
}

/// Capture the primary screen in-process with the `screenshots` crate and
/// write a PNG.
///
/// Probe layer: on Wayland this asks the xdg-desktop-portal `Screenshot`
/// D-Bus interface (not a capture protocol), so it succeeds only when the
/// session runs a portal backend that implements it. The crate is
/// deprecated upstream and returns an `image` 0.24 buffer, which is
/// re-wrapped into this workspace's `image` 0.25 type for saving.
#[cfg(target_os = "linux")]
fn capture_via_screenshots(dest: &Path) -> Result<(), String> {
    use screenshots::Screen;

    let started = std::time::Instant::now();
    let screens = Screen::all().map_err(|e| {
        tracing::error!(error = %e, "screenshots: could not enumerate screens");
        e.to_string()
    })?;
    let screen = screens.into_iter().next().ok_or_else(|| {
        tracing::error!("screenshots: no screen found");
        "no screen found".to_string()
    })?;
    tracing::info!(
        width = screen.display_info.width,
        height = screen.display_info.height,
        "screenshots: capturing primary screen"
    );
    let image = screen.capture().map_err(|e| {
        tracing::error!(
            error = %e,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "screenshots: capture failed"
        );
        e.to_string()
    })?;
    tracing::info!(
        width = image.width(),
        height = image.height(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "screenshots: frame captured"
    );
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
    let rgba = image::RgbaImage::from_raw(image.width(), image.height(), image.into_raw())
        .ok_or_else(|| {
            tracing::error!("screenshots: captured buffer does not match its dimensions");
            "invalid captured buffer".to_string()
        })?;
    rgba.save(dest).map_err(|e| {
        tracing::error!(dest = %dest.display(), error = %e, "screenshots: could not save png");
        format!("{}: {e}", dest.display())
    })?;
    tracing::info!(dest = %dest.display(), "screenshots: png written");
    Ok(())
}

/// Capture the primary screen in-process with `xcap` and write a PNG.
///
/// `Monitor::all()` sorts by position; the first monitor is the primary.
/// The image comes back as an RGBA buffer, so saving is all that is left.
/// On Wayland this sits behind grim-rs (its libwayshot backend only speaks
/// wlr-screencopy, which niri rejects); on X11 it is the primary path.
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

/// Run a custom capture command: `{file}` is substituted with the
/// destination, which is also available as `$1`.
fn run_custom(command: &str, dest: &Path) -> Result<(), String> {
    let plan = shell(command.replace("{file}", &quotable(dest)), dest);
    run_plan(&plan, dest)
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
        return Err(format!(
            "{} exited with {status} (set a custom command in Settings ▸ General)",
            plan.program
        ));
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

/// Where a capture is written before import: `<config>/trove/screenshots`.
pub fn destination(dir: &Path) -> PathBuf {
    let name = chrono::Local::now().format("screenshot-%Y%m%d-%H%M%S.png");
    dir.join("screenshots").join(name.to_string())
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

    #[test]
    fn custom_command_wins_and_substitutes_the_file() {
        let plan = plan_for(
            ScreenshotMode::Full,
            Some("grim {file}"),
            &dest(),
            Platform::default(),
        )
        .expect("custom command always plans");
        assert_eq!(plan.program, "sh");
        assert!(plan.args.contains(&"grim /tmp/shot.png".to_string()));
        // The destination is also available as `$1`.
        assert_eq!(plan.args.last().unwrap(), "/tmp/shot.png");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn blank_custom_command_falls_through() {
        let plan = plan_for(
            ScreenshotMode::Full,
            Some("   "),
            &dest(),
            Platform {
                wayland: true,
                x11: false,
            },
        )
        .expect("wayland plans");
        assert_eq!(plan.program, "grim");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn wayland_uses_grim_and_slurp() {
        let wayland = Platform {
            wayland: true,
            x11: false,
        };
        let full = plan_for(ScreenshotMode::Full, None, &dest(), wayland).unwrap();
        assert_eq!(full.program, "grim");
        assert_eq!(full.args, vec!["/tmp/shot.png".to_string()]);

        let region = plan_for(ScreenshotMode::Region, None, &dest(), wayland).unwrap();
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
        let full = plan_for(ScreenshotMode::Full, None, &dest(), x11).unwrap();
        assert_eq!((full.program.as_str(), full.args.len()), ("scrot", 1));

        let region = plan_for(ScreenshotMode::Region, None, &dest(), x11).unwrap();
        assert_eq!(
            region.args,
            vec!["-s".to_string(), "/tmp/shot.png".to_string()]
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn no_session_has_no_default_plan() {
        assert!(plan_for(ScreenshotMode::Full, None, &dest(), Platform::default()).is_none());
    }

    #[test]
    fn destination_lives_under_the_given_folder() {
        let out = destination(Path::new("/home/noke/.config/trove"));
        assert!(out.starts_with("/home/noke/.config/trove/screenshots"));
        assert_eq!(out.extension().unwrap(), "png");
    }
}
