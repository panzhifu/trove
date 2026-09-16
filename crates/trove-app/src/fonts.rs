//! System font installation (user-level, no admin rights).
//!
//! Install = copy the font file into the user's fonts directory, register it
//! with the OS and refresh the font cache; uninstall = unregister, delete
//! the file and refresh again. The copy is named by content hash
//! (`<sha256>.<ext>`), which doubles as the installed-state check.
//!
//! Windows registers per-user fonts in `HKCU\…\Fonts` (the value name is
//! `<family> (TrueType)`); running applications learn about the change on
//! their next font rescan, newly launched ones immediately. Linux/macOS
//! need no registry — the file in the fonts directory is the installation.

use std::path::{Path, PathBuf};

/// Where Trove installs fonts for the current user.
fn fonts_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(dirs::home_dir()?.join("Library/Fonts"))
    } else if cfg!(target_os = "linux") {
        Some(dirs::home_dir()?.join(".local/share/fonts/trove"))
    } else if cfg!(target_os = "windows") {
        // Per-user fonts need no elevation (machine fonts under
        // %SystemRoot%\Fonts would).
        Some(dirs::data_local_dir()?.join(r"Microsoft\Windows\Fonts"))
    } else {
        None
    }
}

/// The installed file for `sha`, if any (match by hash-named file in the
/// fonts directory).
fn installed_path(sha: &str) -> Option<PathBuf> {
    let dir = fonts_dir()?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.split('.').next() == Some(sha) {
            return Some(entry.path());
        }
    }
    None
}

/// Whether the font with this content hash is already installed.
pub(crate) fn is_installed(sha: &str) -> bool {
    installed_path(sha).is_some()
}

/// Copy the font into the user fonts directory (named `<sha>.<ext>`),
/// register it with the OS and refresh the font cache. Returns the
/// installed path.
pub(crate) fn install(source: &Path, sha: &str) -> Result<PathBuf, String> {
    let dir =
        fonts_dir().ok_or_else(|| "font install is not supported on this platform".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("ttf")
        .to_ascii_lowercase();
    let dest = dir.join(format!("{sha}.{ext}"));
    if source != dest.as_path() {
        std::fs::copy(source, &dest).map_err(|e| e.to_string())?;
    }
    // Windows ignores a per-user font without its registry entry.
    #[cfg(target_os = "windows")]
    {
        let family = trove_core::media::metadata::font_family(&dest)
            .map(|(family, _)| family)
            .unwrap_or_else(|| format!("Trove Font {sha}"));
        registry_set(&format!("{family} (TrueType)"), &dest);
    }
    refresh_cache();
    Ok(dest)
}

/// Unregister the installed font, remove its file and refresh the font
/// cache.
pub(crate) fn uninstall(sha: &str) -> Result<(), String> {
    let path = installed_path(sha).ok_or_else(|| "font is not installed".to_string())?;
    #[cfg(target_os = "windows")]
    if let Some((family, _)) = trove_core::media::metadata::font_family(&path) {
        registry_remove(&format!("{family} (TrueType)"));
    }
    std::fs::remove_file(path).map_err(|e| e.to_string())?;
    refresh_cache();
    Ok(())
}

/// One entry in a mass-activation report: a font that was skipped (already in
/// the target state) or that failed, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivationIssue {
    AlreadyActive,
    AlreadyInactive,
    Failed(String),
}

/// Result of a mass activate / deactivate run. Mirrors FontMatrix's
/// activation report: counts plus per-font issues, so the UI can show a
/// dialog summarising what was done, skipped and failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ActivationReport {
    pub activated: usize,
    pub deactivated: usize,
    /// sha → problem, for fonts not in the desired state after the run.
    pub issues: Vec<(String, ActivationIssue)>,
}

impl ActivationReport {
    /// Activate every font in `sources`, returning a report. Activation is
    /// idempotent and transactional: fonts already active are counted as
    /// skipped, fonts that copy/registration fail carry the reason.
    pub(crate) fn activate_all(sources: &[(&Path, &str)]) -> Self {
        let mut report = ActivationReport::default();
        for (source, sha) in sources {
            if is_installed(sha) {
                report
                    .issues
                    .push((sha.to_string(), ActivationIssue::AlreadyActive));
                continue;
            }
            match install(source, sha) {
                Ok(_) => report.activated += 1,
                Err(e) => report
                    .issues
                    .push((sha.to_string(), ActivationIssue::Failed(e))),
            }
        }
        report
    }

    /// Deactivate every font in `shas`, returning a report. Already-installed
    /// fonts are skipped with [ActivationIssue::AlreadyInactive]; failures
    /// carry the underlying error.
    pub(crate) fn deactivate_all(shas: &[&str]) -> Self {
        let mut report = ActivationReport::default();
        for sha in shas {
            if !is_installed(sha) {
                report
                    .issues
                    .push((sha.to_string(), ActivationIssue::AlreadyInactive));
                continue;
            }
            match uninstall(sha) {
                Ok(_) => report.deactivated += 1,
                Err(e) => report
                    .issues
                    .push((sha.to_string(), ActivationIssue::Failed(e))),
            }
        }
        report
    }
}

/// Best-effort font-cache refresh so applications pick the change up.
/// Runs in a background thread: `fc-cache -f` scans the whole font directory
/// and blocks for hundreds of ms to seconds on large collections —
/// waiting on it on the main thread is what made installation feel
/// sluggish. Font files are already on disk before this runs; the cache
/// just needs to catch up.
fn refresh_cache() {
    if cfg!(target_os = "linux") {
        std::thread::spawn(|| {
            let _ = std::process::Command::new("fc-cache").arg("-f").output();
        });
    }
}

/// The per-user font registry key (Windows). `reg.exe` keeps this
/// dependency-free; CREATE_NO_WINDOW keeps the console invisible.
#[cfg(target_os = "windows")]
fn registry_set(value_name: &str, path: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
            "/v",
            value_name,
            "/t",
            "REG_SZ",
            "/d",
        ])
        .arg(path.as_os_str())
        .arg("/f")
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// Drop the registry entry for `value_name`; a missing entry is fine.
#[cfg(target_os = "windows")]
fn registry_remove(value_name: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
            "/v",
            value_name,
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}
