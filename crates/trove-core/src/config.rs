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
    /// Undo-history depth: how many invertible operations the library keeps
    /// for undo. See [`crate::history`]. Clamped to 1..=500 on read.
    #[serde(default)]
    pub undo_cap: Option<usize>,
    /// Which workspace filter tools are visible in the in-panel toolbar
    /// row (subset of [`FILTER_TOOLS`]). `None` = the default set.
    #[serde(default)]
    pub filter_tools: Option<Vec<String>>,
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
    /// Size from which a source is linked in place whatever `import_mode`
    /// says, in MiB. Defaults to
    /// [`crate::media::import::LINK_OVER_MB_DEFAULT`]: a multi-gigabyte
    /// model copied into the library would double the disk it needs for no
    /// benefit, since the preview reads it where it lies anyway. `0` turns
    /// the rule off.
    #[serde(default)]
    pub import_link_over_mb: Option<u64>,
    /// Eye-dome lighting and gap filling on a point-cloud preview. On by
    /// default: without it a scan reads as dust rather than a surface. Turn it
    /// off for a flatter, marginally cheaper picture.
    #[serde(default)]
    pub point_enhance: Option<bool>,
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
    /// Minimum preview zoom (image and 3D model). Clamped on read.
    #[serde(default)]
    pub min_preview_zoom: Option<f32>,
    /// Maximum preview zoom (image and 3D model). Clamped on read.
    #[serde(default)]
    pub max_preview_zoom: Option<f32>,
    /// Paint 3D previews by height — every [`crate::media::render3d::HEIGHT_BAND`]
    /// units gets its own hue — with the axis gizmo for reference. Off by
    /// default so a model looks the way the file intended.
    #[serde(default)]
    pub height_color: Option<bool>,
    /// Draw the scene's X/Y/Z axes on the model's bounding box. On by
    /// default: a model viewer whose axes cannot be told apart is measuring
    /// nothing. Independent of `height_color`, which used to be the only way
    /// to get them.
    #[serde(default)]
    pub scene_axes: Option<bool>,
    /// Draw the corner trihedron — a small X/Y/Z axis indicator pinned to the
    /// viewport's bottom-right corner, turning with the camera. On by default,
    /// the way every 3D viewer ships it.
    #[serde(default)]
    pub corner_axis: Option<bool>,
    /// Check GitHub for a newer release on launch. On by default. The check
    /// only reads the newest tag and offers a link; Trove never downloads or
    /// replaces its own binary (see [`crate::services::update`]).
    #[serde(default)]
    pub update_check: Option<bool>,
    /// Unix seconds when a check last finished, so relaunching does not probe
    /// GitHub again before [`UPDATE_CHECK_INTERVAL_SECS`] has passed.
    #[serde(default)]
    pub last_update_check: Option<i64>,
    /// A release the user asked not to be told about again, without the
    /// leading `v` (e.g. "0.5.0"). Only that exact version stays quiet — the
    /// next release after it is announced as usual.
    #[serde(default)]
    pub skipped_version: Option<String>,
}

/// Default minimum preview zoom (0.25×).
pub const DEFAULT_MIN_PREVIEW_ZOOM: f32 = 0.25;
/// Default maximum preview zoom (32×).
pub const DEFAULT_MAX_PREVIEW_ZOOM: f32 = 32.0;
/// How long a completed release check keeps a fresh relaunch from probing
/// GitHub again.
pub const UPDATE_CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

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

/// Undo-history depth used when no explicit cap is configured. See
/// [`crate::history::undo::DEFAULT_UNDO_CAP`].
pub use crate::history::undo::DEFAULT_UNDO_CAP;

/// Every workspace filter tool that the toolbar can show, in display order.
pub const FILTER_TOOLS: &[&str] = &["kind", "tag", "shape", "rating", "format"];

/// The filter tools shown when the user has not customized the set.
pub const DEFAULT_FILTER_TOOLS: &[&str] = &["kind"];

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

    /// Undo-history depth: how many invertible operations stay undoable.
    pub fn undo_cap(&self) -> usize {
        self.undo_cap.unwrap_or(DEFAULT_UNDO_CAP).clamp(1, 500)
    }

    /// The workspace filter tools currently shown, defaulting to the
    /// favourites + kind pair.
    pub fn filter_tools(&self) -> Vec<String> {
        match &self.filter_tools {
            Some(tools) => FILTER_TOOLS
                .iter()
                .filter(|t| tools.contains(&t.to_string()))
                .map(|t| t.to_string())
                .collect(),
            None => DEFAULT_FILTER_TOOLS.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Toggle one filter tool on/off and persist.
    pub fn toggle_filter_tool(&mut self, tool: &str) -> Result<()> {
        let mut tools = self.filter_tools();
        if tools.iter().any(|t| t == tool) {
            tools.retain(|t| t != tool);
        } else {
            tools.push(tool.to_string());
        }
        self.filter_tools = Some(tools);
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

    /// Minimum preview zoom, clamped to a sane range.
    pub fn min_preview_zoom(&self) -> f32 {
        self.min_preview_zoom
            .unwrap_or(DEFAULT_MIN_PREVIEW_ZOOM)
            .clamp(0.1, 1.0)
    }

    /// Maximum preview zoom, clamped to a sane range.
    pub fn max_preview_zoom(&self) -> f32 {
        self.max_preview_zoom
            .unwrap_or(DEFAULT_MAX_PREVIEW_ZOOM)
            .clamp(2.0, 100.0)
    }

    /// Whether 3D previews are painted by height rather than the material.
    pub fn height_color(&self) -> bool {
        self.height_color.unwrap_or(false)
    }

    /// Whether the 3D preview draws the scene's X/Y/Z axes (on by default).
    pub fn scene_axes(&self) -> bool {
        self.scene_axes.unwrap_or(true)
    }

    /// Whether the 3D preview draws the corner trihedron (on by default).
    pub fn corner_axis(&self) -> bool {
        self.corner_axis.unwrap_or(true)
    }

    /// Whether to look for a newer release on launch (on by default).
    pub fn update_check(&self) -> bool {
        self.update_check.unwrap_or(true)
    }

    /// Whether a launch should check for updates: on when no check has ever
    /// run, or when the last one is at least
    /// [`UPDATE_CHECK_INTERVAL_SECS`] old. `now` is Unix seconds
    /// ([`crate::services::update::now_unix`]) so this stays testable.
    pub fn update_check_due(&self, now: i64) -> bool {
        match self.last_update_check {
            // A clock that jumped backwards must not latch the check off, so
            // the difference is taken as an absolute value.
            Some(last) => (now - last).abs() >= UPDATE_CHECK_INTERVAL_SECS,
            None => true,
        }
    }

    /// Record that a check just finished (successful or not) and persist.
    ///
    /// Failed checks count too: a machine that is offline should not probe
    /// GitHub again on every single launch.
    pub fn record_update_check(&mut self, now: i64) -> Result<()> {
        self.last_update_check = Some(now);
        self.save()
    }

    /// The release the user asked not to hear about again.
    pub fn skipped_version(&self) -> Option<&str> {
        self.skipped_version.as_deref()
    }

    /// Stop announcing `version` and persist.
    pub fn skip_version(&mut self, version: &str) -> Result<()> {
        self.skipped_version = Some(version.to_string());
        self.save()
    }

    /// How manual imports treat source files: `true` = link to the original
    /// location (no copy), `false` = copy into the library (default).
    pub fn import_linked(&self) -> bool {
        self.import_mode.as_deref() == Some("link")
    }

    /// Whether point-cloud previews get eye-dome lighting and gap filling.
    pub fn point_enhance(&self) -> bool {
        self.point_enhance.unwrap_or(true)
    }

    /// The import policy this configuration describes: the user's all-or-
    /// nothing preference plus the size at which a file is linked anyway.
    pub fn import_policy(&self) -> crate::media::import::ImportPolicy {
        crate::media::import::ImportPolicy {
            link_all: self.import_linked(),
            link_over: self
                .import_link_over_mb
                .unwrap_or(crate::media::import::LINK_OVER_MB_DEFAULT)
                .saturating_mul(1 << 20),
        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_check_waits_a_day_between_probes() {
        let mut config = AppConfig::default();
        assert!(config.update_check_due(1_000), "no timestamp yet → due");
        config.last_update_check = Some(1_000);
        assert!(!config.update_check_due(1_000 + UPDATE_CHECK_INTERVAL_SECS - 1));
        assert!(config.update_check_due(1_000 + UPDATE_CHECK_INTERVAL_SECS));
        // A clock that jumped backwards must not latch the check off.
        assert!(config.update_check_due(1_000 - UPDATE_CHECK_INTERVAL_SECS));
    }

    #[test]
    fn the_launch_check_is_on_until_switched_off() {
        let mut config = AppConfig::default();
        assert!(config.update_check());
        config.update_check = Some(false);
        assert!(!config.update_check());
    }

    #[test]
    fn a_skipped_release_is_remembered() {
        let mut config = AppConfig::default();
        assert_eq!(config.skipped_version(), None);
        config.skipped_version = Some("0.5.0".into());
        assert_eq!(config.skipped_version(), Some("0.5.0"));
    }
}
