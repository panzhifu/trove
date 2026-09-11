//! Screenshots: capture the screen (or a region of it) into a PNG that the
//! app then imports like any other file.
//!
//! Trove never talks to a display server directly: it shells out to whatever
//! the platform offers, and lets the user override the whole thing with a
//! custom command in Settings ▸ General. Planning is a pure function
//! ([`plan`]) so the strategy chain is unit-testable without a display.

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

/// Run the capture: plan it, execute it, and verify the PNG showed up.
pub fn capture(mode: ScreenshotMode, custom: Option<&str>, dest: &Path) -> Result<(), String> {
    let Some(plan) = plan(mode, custom, dest) else {
        return Err("no screenshot tool for this platform".into());
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Interactive tools inherit the desktop: keep stdio open so region
    // pickers can draw, but never block on a hung child forever — the
    // caller runs this on a background executor.
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .map_err(|e| format!("{}: {e}", plan.program))?;
    if !status.success() {
        return Err(format!(
            "{} exited with {status} (set a custom command in Settings ▸ General)",
            plan.program
        ));
    }
    if !dest.is_file() {
        return Err("the screenshot tool wrote no file".into());
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
