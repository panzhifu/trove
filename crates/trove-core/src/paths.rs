//! Where Trove keeps its files.
//!
//! Four roots, one per XDG category, and nothing of ours outside them:
//!
//! ```text
//! config   ~/.config/trove          settings, themes, history
//! data     ~/.local/share/trove     library databases, backups, incoming
//! cache    ~/.cache/trove           thumbnails, full-text index — rebuildable
//! state    ~/.local/state/trove     logs
//! ```
//!
//! A library is never a folder the user picks: each one owns a *slug*
//! directory under `data/libraries/`, and the parts that can be regenerated
//! (thumbnails, the full-text index) sit under the matching `cache/libraries/
//! <slug>/`. Removing a library therefore removes two directories and touches
//! no user file — assets are linked where they already live.
//!
//! `config`, `data` and `cache` each have an environment override
//! (`TROVE_CONFIG_DIR`, `TROVE_DATA_DIR`, `TROVE_CACHE_DIR`) so a test run, a
//! benchmark or a second profile can live entirely elsewhere. `state` has
//! none — the only thing under it is a log file.

use std::path::{Path, PathBuf};

/// Application directory name under each platform root.
pub const APP_DIR: &str = "trove";

/// Slug of the library a fresh install starts with.
pub const DEFAULT_LIBRARY_SLUG: &str = "default";

/// Environment variable relocating the data root.
pub const DATA_DIR_ENV: &str = "TROVE_DATA_DIR";

/// Environment variable relocating the config root. A second profile, or a
/// test run that must not read or write the user's real settings.
pub const CONFIG_DIR_ENV: &str = "TROVE_CONFIG_DIR";

/// Environment variable relocating the cache root. Same purpose as
/// [`CONFIG_DIR_ENV`]: a test run must not fill the user's real cache.
pub const CACHE_DIR_ENV: &str = "TROVE_CACHE_DIR";

/// `<platform root>/trove`, falling back to a dot-directory in `$HOME` when
/// the platform reports no root at all (a stripped container, a broken
/// environment — the app should still start and say where it put things).
fn platform_root(pick: fn() -> Option<PathBuf>, kind: &str) -> PathBuf {
    match pick() {
        Some(dir) => dir.join(APP_DIR),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(format!(".{APP_DIR}-{kind}")),
    }
}

/// The root for a given XDG category: the override when present, the platform
/// root otherwise. Split out of the callers so the rule is testable without
/// mutating the process environment (racy under a parallel test run).
fn resolve_root(
    override_dir: Option<PathBuf>,
    pick: fn() -> Option<PathBuf>,
    kind: &str,
) -> PathBuf {
    override_dir.unwrap_or_else(|| platform_root(pick, kind))
}

/// The value of a root's override variable, if it is set.
fn env_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

/// `~/.config/trove` — settings, themes, history. Relocatable with
/// [`CONFIG_DIR_ENV`].
pub fn config_dir() -> PathBuf {
    resolve_root(env_dir(CONFIG_DIR_ENV), dirs::config_dir, "config")
}

/// `~/.local/share/trove` — library databases, backups, incoming files.
/// Relocatable with [`DATA_DIR_ENV`].
pub fn data_dir() -> PathBuf {
    resolve_root(env_dir(DATA_DIR_ENV), dirs::data_dir, "data")
}

/// `~/.cache/trove` — thumbnails and the full-text index. Safe to delete:
/// everything here is derived from the database and the linked files.
/// Relocatable with [`CACHE_DIR_ENV`].
pub fn cache_dir() -> PathBuf {
    resolve_root(env_dir(CACHE_DIR_ENV), dirs::cache_dir, "cache")
}

/// `~/.local/state/trove` — logs. XDG_STATE_HOME is a Linux convention;
/// elsewhere the app's state lives beside its data.
pub fn state_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Some(dir) = dirs::state_dir() {
        return dir.join(APP_DIR);
    }
    data_dir().join("state")
}

/// The config file.
pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// High-frequency history (recent colours, recent libraries).
pub fn history_file() -> PathBuf {
    config_dir().join("history.json")
}

/// User-supplied theme JSON.
pub fn themes_dir() -> PathBuf {
    config_dir().join("themes")
}

/// The parent of every library directory.
pub fn libraries_dir() -> PathBuf {
    data_dir().join("libraries")
}

/// One library's data root: its database, `library.json`, backups, and — when
/// the library holds copies Trove made for itself — a `media/` blob store.
pub fn library_dir(slug: &str) -> PathBuf {
    libraries_dir().join(slug)
}

/// One library's rebuildable derivatives: `thumbs/` and `search_index/`.
pub fn library_cache_dir(slug: &str) -> PathBuf {
    cache_dir().join("libraries").join(slug)
}

/// Files Trove produced itself and then imported — screenshots, extension
/// uploads. They are kept here rather than in a temporary directory because
/// the library *links* them: the asset must survive a reboot.
pub fn incoming_dir() -> PathBuf {
    data_dir().join("incoming")
}

/// Where the log file is written.
pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Create `dir` (and its parents) and return it.
pub fn ensure(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    Ok(dir.to_path_buf())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every derived path hangs off exactly one root, so relocating a root
    /// relocates its whole subtree.
    #[test]
    fn derived_directories_follow_their_root() {
        let libs = libraries_dir();
        assert!(libs.starts_with(data_dir()));
        assert_eq!(library_dir("work"), libs.join("work"));
        assert!(library_cache_dir("work").starts_with(cache_dir()));
        assert!(incoming_dir().starts_with(data_dir()));
        assert!(logs_dir().starts_with(state_dir()));
        assert!(config_file().starts_with(config_dir()));
        assert!(themes_dir().starts_with(config_dir()));
    }

    /// The roots that can be moved by the environment resolve to their
    /// override, and to the platform root otherwise.
    #[test]
    fn the_roots_honour_their_overrides() {
        let override_dir = PathBuf::from("/nonexistent/trove-data-override");
        assert_eq!(
            resolve_root(Some(override_dir.clone()), dirs::data_dir, "data"),
            override_dir
        );
        for (pick, kind) in [
            (dirs::data_dir as fn() -> Option<PathBuf>, "data"),
            (dirs::config_dir, "config"),
            (dirs::cache_dir, "cache"),
        ] {
            assert!(resolve_root(None, pick, kind).ends_with(APP_DIR));
        }
    }
}
