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
    /// Which "search by image" backend to use: "visual" (pHash + colour
    /// histogram, no model needed) or "semantic" (CLIP embeddings).
    #[serde(default)]
    pub search_mode: Option<String>,
    /// Directory holding the CLIP ONNX models (`clip-image.onnx` +
    /// `clip-text.onnx`). Used only when `search_mode` is `semantic`.
    #[serde(default)]
    pub clip_model_dir: Option<PathBuf>,
    /// Path to the ONNX Runtime dynamic library (`libonnxruntime.so` /
    /// `.dylib` / `.dll`). When `None`, the engine falls back to
    /// `ORT_DYLIB_PATH` env var then the executable directory.
    #[serde(default)]
    pub ort_lib_path: Option<PathBuf>,
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

    /// Set the UI language (`None` = follow system) and persist.
    pub fn set_language(&mut self, language: Option<String>) -> Result<()> {
        self.language = language;
        self.save()
    }

    /// Effective search mode ("visual" or "semantic"). Defaults to visual.
    pub fn search_mode(&self) -> String {
        self.search_mode.as_deref().unwrap_or("visual").to_string()
    }

    /// Directory that should hold the CLIP image/text ONNX models.
    pub fn clip_model_dir(&self) -> Option<PathBuf> {
        self.clip_model_dir
            .clone()
            .or_else(|| Self::config_dir().map(|d| d.join("models")))
    }

    /// Effective path to the ONNX Runtime library: explicit config wins,
    /// then `ORT_DYLIB_PATH` env, then `None` (engine searches cwd/exe dir).
    pub fn ort_lib_path(&self) -> Option<PathBuf> {
        self.ort_lib_path
            .clone()
            .or_else(|| std::env::var("ORT_DYLIB_PATH").ok().map(PathBuf::from))
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
