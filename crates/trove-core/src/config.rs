//! Application configuration: persisted user preferences such as the
//! library location. Stored as JSON in the platform's standard config
//! directory (`~/.config/trove` on Linux, `~/Library/Application Support/trove`
//! on macOS, `%APPDATA%/trove` on Windows).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Application configuration, persisted as JSON in the platform config dir.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// The path of the currently-open library. `None` until the user picks one.
    pub library_path: Option<PathBuf>,
    /// UI language (`None` = follow the system preference). See
    /// `trove-app/src/i18n.rs` for how the code resolves to a catalog.
    #[serde(default)]
    pub language: Option<String>,
    /// Custom keybindings. Maps action name to key string (e.g. "enter" -> "ctrl-p").
    #[serde(default)]
    pub keybindings: std::collections::HashMap<String, String>,
    /// Grid zoom: multiplier on the ideal thumbnail-row height (the
    /// title-bar slider). 1.0 = default; clamped on read.
    #[serde(default)]
    pub grid_zoom: Option<f32>,
    /// Recently opened libraries, newest first (settings ▸ general lists
    /// these for one-click hot switching). Capped at [`RECENT_LIBRARY_CAP`].
    #[serde(default)]
    pub recent_libraries: Vec<PathBuf>,
    /// Folders watched for new files; anything that appears under them is
    /// imported automatically (unfiled). Empty = no watching.
    #[serde(default)]
    pub watched_folders: Vec<PathBuf>,
    /// Master switch for folder watching. Defaults to on once folders are
    /// configured; `false` pauses the watcher without losing the list.
    #[serde(default)]
    pub watch_folders_enabled: Option<bool>,
    /// Local collect service (127.0.0.1 HTTP inbox). On by default.
    #[serde(default)]
    pub collect_enabled: Option<bool>,
    /// Collect service port. Defaults to [`crate::services::collect::DEFAULT_PORT`].
    #[serde(default)]
    pub collect_port: Option<u16>,
    /// How manual imports treat source files: "copy" (default) stores a copy
    /// of the file inside the library; "link" keeps the file where it is and
    /// records the original location instead.
    #[serde(default)]
    pub import_mode: Option<String>,
    /// Sample text rendered on font-specimen thumbnails (font cards).
    /// Characters missing from a given font are skipped while rendering.
    #[serde(default)]
    pub font_sample: Option<String>,
    /// Custom screenshot command. It receives the output PNG path: either as
    /// `{file}` inside the command, or as `$1`. Empty = auto-detect.
    #[serde(default)]
    pub screenshot_command: Option<String>,
    /// Light/dark appearance. `System` follows the OS and is the default.
    #[serde(default)]
    pub appearance: Appearance,
    /// Named theme (from the UI framework's theme registry) used when the
    /// appearance resolves to light. `None` = the framework default.
    #[serde(default)]
    pub theme_light: Option<String>,
    /// Named theme used when the appearance resolves to dark.
    #[serde(default)]
    pub theme_dark: Option<String>,
}

/// Which light/dark appearance the UI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl Appearance {
    /// The value stored in JSON and offered by the settings picker.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Inverse of [`Self::as_str`]; unknown values fall back to `System`.
    pub fn parse(value: &str) -> Self {
        match value {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
}

/// How many recent-library entries to remember.
pub const RECENT_LIBRARY_CAP: usize = 8;

impl AppConfig {
    /// The directory where the config file lives.
    ///
    /// Follows the platform convention:
    /// - Linux: `~/.config/trove`
    /// - macOS: `~/Library/Application Support/trove`
    /// - Windows: `%APPDATA%/trove`
    pub fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("trove"))
    }

    /// Full path to the config file.
    pub fn config_file() -> Option<PathBuf> {
        Self::config_dir().map(|d| d.join("config.json"))
    }

    /// Load the config from disk, or return a default config if none exists.
    pub fn load() -> Self {
        let Some(path) = Self::config_file() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the config to disk.
    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::config_file() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        fs::write(&path, text)?;
        Ok(())
    }

    /// Set the library path and persist.
    pub fn set_library_path(&mut self, path: PathBuf) -> Result<()> {
        self.library_path = Some(path);
        self.save()
    }

    /// Record a library as recently used: moved to the front, deduplicated,
    /// capped. Persists immediately.
    pub fn push_recent_library(&mut self, path: PathBuf) -> Result<()> {
        self.recent_libraries.retain(|p| p != &path);
        self.recent_libraries.insert(0, path);
        self.recent_libraries.truncate(RECENT_LIBRARY_CAP);
        self.save()
    }

    /// Drop one library from the recent list and persist.
    pub fn remove_recent_library(&mut self, path: &PathBuf) -> Result<()> {
        self.recent_libraries.retain(|p| p != path);
        self.save()
    }

    /// Whether the folder watcher should run (on by default).
    pub fn watch_folders_enabled(&self) -> bool {
        self.watch_folders_enabled.unwrap_or(true)
    }

    /// Whether the local collect service should listen (on by default).
    pub fn collect_enabled(&self) -> bool {
        self.collect_enabled.unwrap_or(true)
    }

    /// Effective collect-service port.
    pub fn collect_port(&self) -> u16 {
        self.collect_port
            .unwrap_or(crate::services::collect::DEFAULT_PORT)
    }

    /// Add a watched folder (deduplicated) and persist.
    pub fn add_watched_folder(&mut self, path: PathBuf) -> Result<()> {
        if !self.watched_folders.contains(&path) {
            self.watched_folders.push(path);
        }
        self.save()
    }

    /// Stop watching a folder and persist.
    pub fn remove_watched_folder(&mut self, path: &PathBuf) -> Result<()> {
        self.watched_folders.retain(|p| p != path);
        self.save()
    }

    /// Set the UI language (`None` = follow system) and persist.
    pub fn set_language(&mut self, language: Option<String>) -> Result<()> {
        self.language = language;
        self.save()
    }

    /// Effective grid zoom, clamped to 0.6..1.8 (1.0 = default size).
    pub fn grid_zoom(&self) -> f32 {
        self.grid_zoom.unwrap_or(1.0).clamp(0.6, 1.8)
    }

    /// How manual imports treat source files: `true` = link to the original
    /// location (no copy), `false` = copy into the library (default).
    pub fn import_linked(&self) -> bool {
        self.import_mode.as_deref() == Some("link")
    }

    /// Sample text for font-specimen thumbnails (`font_sample` or the
    /// built-in default).
    pub fn font_sample_text(&self) -> String {
        self.font_sample
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Aa 允 123".into())
    }

    /// The screenshot command as typed by the user (never a fallback).
    pub fn screenshot_command_text(&self) -> String {
        self.screenshot_command.clone().unwrap_or_default()
    }

    /// Resolved library path: the `TROVE_LIBRARY_DIR` override when set,
    /// then the configured one, then a sensible default (`~/.trove/library`)
    /// when none is set yet.
    pub fn resolved_library_path(&self) -> PathBuf {
        if let Ok(dir) = std::env::var("TROVE_LIBRARY_DIR") {
            return PathBuf::from(dir);
        }
        self.library_path
            .clone()
            .unwrap_or_else(default_library_path)
    }
}

/// Sensible default library location when no config exists yet.
pub fn default_library_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".trove")
        .join("library")
}

/// Ensure the config directory exists and return its path.
pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = AppConfig::config_dir().unwrap_or_else(|| PathBuf::from(".trove"));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
