//! System font enumeration and user-level uninstall.
//!
//! Scans the platform font directories (machine + per-user), reads each
//! file's family from its name table, and classifies which entries the
//! current user may remove: only per-user directories are writable — the
//! machine font directories (`C:\Windows\Fonts`, `/usr/share/fonts`,
//! `/System/Library/Fonts`) need administrator rights everywhere, and trove
//! never elevates.
//!
//! Installation lives in the app layer ([`crate::fonts`] mirrors) because
//! Windows setup needs the registry; removing a file the user (or trove)
//! put there is safe from core.

use std::path::{Path, PathBuf};

/// One font file found in a system font directory.
#[derive(Debug, Clone)]
pub struct SystemFont {
    /// Family name from the font's name table (typographic family preferred).
    pub family: String,
    /// Subfamily / style ("Regular", "Bold", …) when the file carries one.
    pub style: Option<String>,
    pub path: PathBuf,
    /// `true` when the file sits in a per-user directory and can be removed
    /// without elevation.
    pub writable: bool,
}

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

/// Collect every font in the system font directories, family-sorted.
/// Files whose name table cannot be parsed (corrupt, or a format
/// ttf-parser does not cover) are skipped.
pub fn scan_system_fonts() -> Vec<SystemFont> {
    let mut out = Vec::new();
    for (dir, writable) in system_font_dirs() {
        for path in font_files(&dir) {
            // Skip absurd files up front; the read below is only for the
            // name table, and a real font is rarely this large.
            match std::fs::metadata(&path) {
                Ok(meta) if meta.len() <= 32 * 1024 * 1024 => {}
                _ => continue,
            }
            let Some((family, style)) = crate::media::metadata::font_family(&path) else {
                continue;
            };
            out.push(SystemFont {
                family,
                style,
                path,
                writable,
            });
        }
    }
    out.sort_by(|a, b| {
        a.family
            .to_lowercase()
            .cmp(&b.family.to_lowercase())
            .then_with(|| a.style.cmp(&b.style))
    });
    out
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

/// Every font file under `dir`, recursively (Linux system trees nest by
/// foundry / license; the Windows and macOS dirs are flat but walking is
/// uniform).
fn font_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(font_files(&path));
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc")
        ) {
            out.push(path);
        }
    }
    out
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
