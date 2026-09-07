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
    /// Path to the single CLIP ONNX model file that exposes both the image
    /// encoder (`pixel_values` → `image_embeds`) and the text encoder
    /// (`input_ids` → `text_embeds`). Used only when `search_mode` is
    /// `semantic`. Defaults to `models/model.onnx` under the config dir.
    #[serde(default)]
    pub clip_model_path: Option<PathBuf>,
    /// Path to the ONNX Runtime dynamic library (`libonnxruntime.so` /
    /// `.dylib` / `.dll`). When `None`, the engine falls back to
    /// `ORT_DYLIB_PATH` env var then the executable / config directory.
    #[serde(default)]
    pub ort_lib_path: Option<PathBuf>,
    /// Minimum cosine similarity for a semantic (CLIP) search hit. Lower =
    /// more (noisier) results. Clamped to 0.0..1.0; defaults to 0.2.
    #[serde(default)]
    pub semantic_min_similarity: Option<f32>,
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
    /// Collect service port. Defaults to [`crate::collect::DEFAULT_PORT`].
    #[serde(default)]
    pub collect_port: Option<u16>,
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
        self.collect_port.unwrap_or(crate::collect::DEFAULT_PORT)
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

    /// Effective search mode ("visual" or "semantic"). Defaults to visual.
    pub fn search_mode(&self) -> String {
        self.search_mode.as_deref().unwrap_or("visual").to_string()
    }

    /// Effective semantic-search similarity threshold, clamped to 0.0..1.0.
    pub fn semantic_min_similarity(&self) -> f32 {
        self.semantic_min_similarity.unwrap_or(0.2).clamp(0.0, 1.0)
    }

    /// Directory that should hold the CLIP model file.
    pub fn clip_model_dir(&self) -> Option<PathBuf> {
        self.clip_model_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| Self::config_dir().map(|d| d.join("models")))
    }

    /// Full path to the single CLIP ONNX model file.
    pub fn clip_model_path(&self) -> Option<PathBuf> {
        self.clip_model_path
            .clone()
            .or_else(|| Self::config_dir().map(|d| d.join("models").join("model.onnx")))
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
