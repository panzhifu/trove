//! System font directory enumeration and user-level uninstall.
//!
//! Classifies which entries the current user may remove: only per-user
//! directories are writable — the machine font directories
//! (`C:\Windows\Fonts`, `/usr/share/fonts`, `/System/Library/Fonts`) need
//! administrator rights everywhere, and trove never elevates.
//!
//! Installation lives in the app layer ([`crate::fonts`] mirrors) because
//! Windows setup needs the registry; removing a file the user (or trove)
//! put there is safe from core.

use std::path::{Path, PathBuf};

/// `(directory, user_writable)` pairs scanned for fonts, user dirs first.
pub fn system_font_dirs() -> Vec<(PathBuf, bool)> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(d) = dirs::data_local_dir() {
            dirs.push((d.join(r"Microsoft\Windows\Fonts"), true));
        }
        // Machine fonts live under the Windows directory; resolve through
        // SystemRoot so relocated installs still resolve.
        if let Ok(root) = std::env::var("SystemRoot") {
            dirs.push((PathBuf::from(root).join("Fonts"), false));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = dirs::home_dir() {
            dirs.push((home.join("Library/Fonts"), true));
        }
        dirs.push((PathBuf::from("/Library/Fonts"), false));
        dirs.push((PathBuf::from("/System/Library/Fonts"), false));
    } else {
        if let Some(home) = dirs::home_dir() {
            dirs.push((home.join(".local/share/fonts"), true));
            dirs.push((home.join(".fonts"), true));
        }
        dirs.push((PathBuf::from("/usr/local/share/fonts"), false));
        dirs.push((PathBuf::from("/usr/share/fonts"), false));
    }
    dirs
}

/// `true` when `path` sits inside one of the per-user font directories.
pub fn is_user_writable(path: &Path) -> bool {
    system_font_dirs()
        .iter()
        .any(|(dir, writable)| *writable && path.starts_with(dir))
}

/// Remove a user-installed font file and refresh the font cache. Refuses
/// anything outside the per-user directories.
pub fn uninstall_system_font(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("font file not found".to_string());
    }
    if !is_user_writable(path) {
        return Err("this font belongs to the system and cannot be removed here".to_string());
    }
    std::fs::remove_file(path).map_err(|e| e.to_string())?;
    refresh_cache();
    Ok(())
}

/// Best-effort font-cache refresh so other applications pick the change up.
fn refresh_cache() {
    if cfg!(target_os = "linux") {
        let _ = std::process::Command::new("fc-cache").arg("-f").output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_dirs_are_absolute_and_deduped() {
        let dirs = system_font_dirs();
        assert!(!dirs.is_empty());
        let mut seen = std::collections::HashSet::new();
        for (dir, _) in &dirs {
            assert!(dir.is_absolute(), "{dir:?} not absolute");
            assert!(seen.insert(dir.clone()), "{dir:?} duplicated");
        }
    }

    #[test]
    fn uninstall_refuses_paths_outside_user_dirs() {
        assert!(uninstall_system_font(Path::new("/usr/share/fonts/x.ttf")).is_err());
        assert!(uninstall_system_font(Path::new("/nonexistent/font.otf")).is_err());
    }
}
