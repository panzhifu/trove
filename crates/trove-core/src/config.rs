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
}

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

    /// Resolved library path: the configured one, or a sensible default
    /// (`~/.trove/library`) when none is set yet.
    pub fn resolved_library_path(&self) -> PathBuf {
        self.library_path.clone().unwrap_or_else(default_library_path)
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
