//! App-level recent-use records: recently picked colours and recently opened
//! libraries. Persisted as `history.json` next to the config file — separate
//! from `AppConfig` because these are high-frequency history writes with a
//! lifecycle of their own, not user preferences.
//!
//! Legacy migration: early builds stored both lists inside `config.json`.
//! On first load, when no history file exists yet, those fields are read
//! from the config file if present; the fields stay there harmlessly (the
//! config parser ignores unknown keys) but are never written again.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// How many picked colours to remember.
pub const COLOR_HISTORY_CAP: usize = 20;
/// How many recently-opened libraries to remember.
pub const RECENT_LIBRARY_CAP: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppHistory {
    /// Recently picked colours from the colour picker (newest first).
    #[serde(default)]
    colors: Vec<String>,
    /// Recently opened libraries, newest first.
    #[serde(default)]
    libraries: Vec<PathBuf>,
}

impl AppHistory {
    /// The history file, next to the config file.
    fn history_file() -> Option<PathBuf> {
        crate::config::AppConfig::config_dir().map(|d| d.join("history.json"))
    }

    /// Load the history, migrating the legacy config-stored lists once.
    pub fn load() -> Self {
        let Some(path) = Self::history_file() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => {
                // First run of the split: pull whatever the config still
                // carries, then persist so migration sticks.
                let history = Self::from_legacy_config();
                if history.colors.is_empty() && history.libraries.is_empty() {
                    return history;
                }
                let _ = history.save();
                history
            }
        }
    }

    /// Extract the legacy lists from `config.json`, if it still has them.
    fn from_legacy_config() -> Self {
        let Some(path) = crate::config::AppConfig::config_file() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        Self {
            colors: value
                .get("color_history")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
            libraries: value
                .get("recent_libraries")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
        }
    }

    /// Persist the history to disk.
    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::history_file() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        fs::write(&path, text)?;
        Ok(())
    }

    // -- colours ---------------------------------------------------------------

    /// Recently picked colours, newest first.
    pub fn colors(&self) -> &[String] {
        &self.colors
    }

    /// Record a picked colour: moved to the front, deduplicated, capped.
    /// Persists immediately.
    pub fn push_color(&mut self, hex: &str) -> Result<()> {
        self.colors.retain(|c| c != hex);
        self.colors.insert(0, hex.to_string());
        self.colors.truncate(COLOR_HISTORY_CAP);
        self.save()
    }

    /// Forget every picked colour and persist.
    pub fn clear_colors(&mut self) -> Result<()> {
        self.colors.clear();
        self.save()
    }

    // -- libraries ---------------------------------------------------------------

    /// Recently opened libraries, newest first.
    pub fn libraries(&self) -> &[PathBuf] {
        &self.libraries
    }

    /// Record a library as recently used: moved to the front, deduplicated,
    /// capped. Persists immediately.
    pub fn push_library(&mut self, path: &std::path::Path) -> Result<()> {
        self.libraries.retain(|p| p != path);
        self.libraries.insert(0, path.to_path_buf());
        self.libraries.truncate(RECENT_LIBRARY_CAP);
        self.save()
    }

    /// Drop one library from the recent list and persist.
    pub fn remove_library(&mut self, path: &std::path::Path) -> Result<()> {
        self.libraries.retain(|p| p != path);
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_history_dedupes_and_caps() {
        let mut h = AppHistory::default();
        for i in 0..(COLOR_HISTORY_CAP + 5) {
            h.push_color(&format!("#{i:06x}")).unwrap();
        }
        assert_eq!(h.colors().len(), COLOR_HISTORY_CAP);
        assert_eq!(h.colors()[0], format!("#{:06x}", COLOR_HISTORY_CAP + 4));

        // Re-picking an older colour moves it to the front.
        let old = h.colors().last().unwrap().clone();
        h.push_color(&old).unwrap();
        assert_eq!(h.colors()[0], old);
    }

    #[test]
    fn library_history_dedupes_and_caps() {
        let mut h = AppHistory::default();
        for i in 0..(RECENT_LIBRARY_CAP + 3) {
            h.push_library(std::path::Path::new(&format!("/lib/{i}")))
                .unwrap();
        }
        assert_eq!(h.libraries().len(), RECENT_LIBRARY_CAP);
        let kept = h.libraries()[0].clone();
        h.push_library(std::path::Path::new("/lib/1")).unwrap();
        assert_eq!(h.libraries()[0], std::path::Path::new("/lib/1"));
        h.remove_library(std::path::Path::new("/lib/1")).unwrap();
        assert!(!h.libraries().contains(&std::path::PathBuf::from("/lib/1")));
        assert_eq!(h.libraries()[0], kept);
    }
}
