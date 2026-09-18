//! App-level recent-use records: the colours the user picked last.
//! Persisted as `history.json` beside the config file — separate from
//! `AppConfig` because this is high-frequency history with a lifecycle of its
//! own, not a preference.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// How many picked colours to remember.
pub const COLOR_HISTORY_CAP: usize = 20;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppHistory {
    /// Recently picked colours from the colour picker (newest first).
    #[serde(default)]
    colors: Vec<String>,
}

impl AppHistory {
    /// The history file, beside the config file.
    fn history_file() -> PathBuf {
        crate::paths::history_file()
    }

    /// Load the history. A missing or unreadable file is an empty history —
    /// this is a convenience record, not something to fail a startup over.
    pub fn load() -> Self {
        fs::read_to_string(Self::history_file())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist the history to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::history_file();
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
}
