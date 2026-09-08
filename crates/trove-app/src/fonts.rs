//! System font installation (user-level, no admin rights).
//!
//! Install = copy the font file into the user's fonts directory and refresh
//! the font cache; uninstall = delete that file and refresh again. The copy
//! is named by content hash (`<sha256>.<ext>`), which doubles as the
//! installed-state check. Only Linux and macOS are wired up; on other
//! platforms install returns an error.

use std::path::{Path, PathBuf};

/// Where Trove installs fonts for the current user.
fn fonts_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(dirs::home_dir()?.join("Library/Fonts"))
    } else if cfg!(target_os = "linux") {
        Some(dirs::home_dir()?.join(".local/share/fonts/trove"))
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

/// Copy the font into the user fonts directory (named `<sha>.<ext>`) and
/// refresh the font cache. Returns the installed path.
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
    refresh_cache();
    Ok(dest)
}

/// Remove the installed font file and refresh the font cache.
pub(crate) fn uninstall(sha: &str) -> Result<(), String> {
    let path = installed_path(sha).ok_or_else(|| "font is not installed".to_string())?;
    std::fs::remove_file(path).map_err(|e| e.to_string())?;
    refresh_cache();
    Ok(())
}

/// Best-effort font-cache refresh so applications pick the change up.
fn refresh_cache() {
    let _ = std::process::Command::new("fc-cache").arg("-f").output();
}
