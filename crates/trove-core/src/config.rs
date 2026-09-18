//! Application configuration: global preferences plus the registry of the
//! user's libraries.
//!
//! Two files, two scopes:
//!
//! - `config.json` in [`crate::paths::config_dir`] — preferences that are the
//!   same whichever library is open: appearance, language, keybindings, zoom,
//!   update checks. Plus the *registry* of libraries ([`LibraryEntry`]) and
//!   which one is open.
//! - `<library>/library.json` ([`LibraryConfig`]) — preferences that belong to
//!   one library: the folders it watches.
//!
//! Where those files live is [`crate::paths`]' business, not this module's.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paths;

/// Application configuration, persisted as JSON in [`paths::config_file`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// Every library the user has, in creation order. A fresh install starts
    /// with one ([`paths::DEFAULT_LIBRARY_SLUG`]).
    #[serde(default)]
    pub libraries: Vec<LibraryEntry>,
    /// Slug of the library currently open. `None` on a fresh install, which
    /// resolves to the default library.
    #[serde(default)]
    pub active_library: Option<String>,
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
    /// Local collect service (127.0.0.1 HTTP inbox). On by default.
    #[serde(default)]
    pub collect_enabled: Option<bool>,
    /// Collect service port. Defaults to [`crate::services::collect::DEFAULT_PORT`].
    #[serde(default)]
    pub collect_port: Option<u16>,
    /// Eye-dome lighting and gap filling on a point-cloud preview. On by
    /// default: without it a scan reads as dust rather than a surface. Turn it
    /// off for a flatter, marginally cheaper picture.
    #[serde(default)]
    pub point_enhance: Option<bool>,
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

/// One library in the registry. Its directory is [`paths::library_dir`] of
/// `slug`; `name` is what the user sees and may change at any time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Directory name under `data/libraries/`. Generated once at creation and
    /// never changed, so renaming a library never moves files.
    pub slug: String,
    /// Display name.
    pub name: String,
}

impl LibraryEntry {
    /// Where this library's data lives.
    pub fn dir(&self) -> PathBuf {
        paths::library_dir(&self.slug)
    }

    /// Where its thumbnails and full-text index live.
    pub fn cache_dir(&self) -> PathBuf {
        paths::library_cache_dir(&self.slug)
    }
}

/// Preferences belonging to a single library, persisted as `library.json` in
/// the library directory. Everything else is global (see [`AppConfig`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Folders watched for new files; anything that appears under them is
    /// imported automatically (unfiled). Empty = no watching.
    #[serde(default)]
    pub watched_folders: Vec<PathBuf>,
    /// Master switch for folder watching. Defaults to on once folders are
    /// configured; `false` pauses the watcher without losing the list.
    #[serde(default)]
    pub watch_folders_enabled: Option<bool>,
}

impl LibraryConfig {
    /// The config file inside `library_dir`.
    pub fn file(library_dir: &std::path::Path) -> PathBuf {
        library_dir.join("library.json")
    }

    /// Load from `library_dir`, or defaults when the file is absent or
    /// unreadable. A missing file is the normal state of a fresh library; an
    /// unreadable one is not — the broken file is moved aside (kept, not
    /// deleted) so nothing silently rewrites over the only evidence of what
    /// the library's settings were.
    pub fn load(library_dir: &std::path::Path) -> Self {
        match fs::read_to_string(Self::file(library_dir)) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(config) => config,
                Err(error) => {
                    quarantine_corrupt(&Self::file(library_dir), &error);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Persist into `library_dir`.
    pub fn save(&self, library_dir: &std::path::Path) -> Result<()> {
        let path = Self::file(library_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        write_atomic(&path, &text)
    }

    /// Whether the folder watcher should run (on by default).
    pub fn watch_folders_enabled(&self) -> bool {
        self.watch_folders_enabled.unwrap_or(true)
    }

    /// Add a watched folder (deduplicated) and persist.
    pub fn add_watched_folder(
        &mut self,
        library_dir: &std::path::Path,
        path: PathBuf,
    ) -> Result<()> {
        if !self.watched_folders.contains(&path) {
            self.watched_folders.push(path);
        }
        self.save(library_dir)
    }

    /// Stop watching a folder and persist.
    pub fn remove_watched_folder(
        &mut self,
        library_dir: &std::path::Path,
        path: &PathBuf,
    ) -> Result<()> {
        self.watched_folders.retain(|p| p != path);
        self.save(library_dir)
    }
}

/// Default minimum preview zoom (0.25×).
pub const DEFAULT_MIN_PREVIEW_ZOOM: f32 = 0.25;
/// Default maximum preview zoom (32×).
pub const DEFAULT_MAX_PREVIEW_ZOOM: f32 = 32.0;
/// How long a completed release check keeps a fresh relaunch from probing
/// GitHub again.
pub const UPDATE_CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// Display name of the library a fresh install starts with. Renaming it is
/// the user's business, so the constant is only a starting point.
pub const DEFAULT_LIBRARY_NAME: &str = "Default";

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

/// Replace `path` with `text` without ever leaving a half-written file
/// behind: the data goes to a sibling temp file first and a rename — atomic
/// on every supported platform — moves it into place. A crash mid-save then
/// costs at most the previous state, never a truncated config.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let temp = path.with_extension(format!(
        "{}tmp-{}",
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&temp, text)?;
    // A failure here leaves the temp file behind (harmless litter next to
    // the config) and the previous config intact.
    fs::rename(&temp, path)?;
    Ok(())
}

/// Move a config file that no longer parses out of the way instead of
/// letting the next save overwrite the only copy of what it said. The
/// suffix carries the unix time so repeated corruption keeps every copy.
fn quarantine_corrupt(path: &Path, error: &serde_json::Error) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let quarantined = path.with_extension(format!("corrupt-{stamp}.json"));
    match fs::rename(path, &quarantined) {
        Ok(()) => tracing::error!(
            %error,
            quarantined = %quarantined.display(),
            "config file did not parse; it was moved aside and defaults apply"
        ),
        Err(error) => {
            tracing::error!(%error, "config file did not parse and could not be moved aside")
        }
    }
}

impl AppConfig {
    /// Load the config from disk, or return a default config if none exists.
    ///
    /// A file that exists but does not parse is the dangerous case: this
    /// config holds the library registry, so silently replacing it with the
    /// default would present "no libraries" as a fresh install. The broken
    /// file is moved aside (`*.corrupt-*`, kept for inspection) and the
    /// default is returned — the data directories are still on disk either
    /// way.
    pub fn load() -> Self {
        match fs::read_to_string(paths::config_file()) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(config) => config,
                Err(error) => {
                    quarantine_corrupt(&paths::config_file(), &error);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Persist the config to disk.
    pub fn save(&self) -> Result<()> {
        let path = paths::config_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        write_atomic(&path, &text)
    }

    // -----------------------------------------------------------------------
    // Libraries
    // -----------------------------------------------------------------------

    /// Slug of the library to open: the recorded one when it is still in the
    /// registry, the default slug otherwise.
    pub fn active_slug(&self) -> String {
        self.active_library
            .as_ref()
            .filter(|slug| self.libraries.iter().any(|l| &l.slug == *slug))
            .cloned()
            .unwrap_or_else(|| paths::DEFAULT_LIBRARY_SLUG.to_string())
    }

    /// The library to open. Falls back to a default entry when the registry
    /// is empty, so callers always have something to open.
    pub fn active_entry(&self) -> LibraryEntry {
        match self.libraries.iter().find(|l| l.slug == self.active_slug()) {
            Some(entry) => entry.clone(),
            None => LibraryEntry {
                slug: paths::DEFAULT_LIBRARY_SLUG.to_string(),
                name: DEFAULT_LIBRARY_NAME.to_string(),
            },
        }
    }

    /// Make sure the recorded library exists on disk and return it.
    ///
    /// The registry itself is never filled in here: the welcome window makes
    /// the user create a library before the main window opens, so inventing
    /// one behind their back would only produce an empty library they did not
    /// ask for. A registry that is somehow empty still resolves to the default
    /// entry rather than panicking.
    pub fn ensure_active_library(&mut self) -> Result<LibraryEntry> {
        let entry = self.active_entry();
        if self.active_library.as_deref() != Some(entry.slug.as_str()) {
            self.active_library = Some(entry.slug.clone());
        }
        paths::ensure(&entry.dir())?;
        paths::ensure(&entry.cache_dir())?;
        self.save()?;
        Ok(entry)
    }

    /// Point the app at `slug` and persist.
    pub fn set_active_library(&mut self, slug: &str) -> Result<()> {
        self.active_library = Some(slug.to_string());
        self.save()
    }

    /// Register a new library named `name` and return its entry. An empty
    /// name falls back to a numbered default.
    pub fn add_library(&mut self, name: &str) -> Result<LibraryEntry> {
        let name = name.trim();
        let name = if name.is_empty() {
            format!("{} {}", DEFAULT_LIBRARY_NAME, self.libraries.len() + 1)
        } else {
            name.to_string()
        };
        let entry = LibraryEntry {
            slug: self.unique_slug(&name),
            name,
        };
        paths::ensure(&entry.dir())?;
        paths::ensure(&entry.cache_dir())?;
        self.libraries.push(entry.clone());
        self.save()?;
        Ok(entry)
    }

    /// Rename a library. The slug — and therefore every path on disk — stays
    /// put, so renaming never moves a file.
    pub fn rename_library(&mut self, slug: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if !name.is_empty()
            && let Some(entry) = self.libraries.iter_mut().find(|l| l.slug == slug)
        {
            entry.name = name.to_string();
        }
        self.save()
    }

    /// Drop a library from the registry. Nothing on disk is touched here: the
    /// caller decides whether the directories go too.
    pub fn forget_library(&mut self, slug: &str) -> Result<()> {
        self.libraries.retain(|l| l.slug != slug);
        if self.active_library.as_deref() == Some(slug) {
            self.active_library = None;
        }
        self.save()
    }

    /// A free slug derived from `name`: the ASCII-safe form of the name when
    /// it has one, a numbered `library-N` otherwise (a name in any non-Latin
    /// script has no usable ASCII form).
    fn unique_slug(&self, name: &str) -> String {
        let base: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = base.trim_matches('-').to_string();
        let base = if base.is_empty() {
            "library".to_string()
        } else {
            base
        };
        if !self.libraries.iter().any(|l| l.slug == base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|slug| !self.libraries.iter().any(|l| &l.slug == slug))
            .expect("an unused suffix always exists")
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

    /// Whether the local collect service should listen (on by default).
    pub fn collect_enabled(&self) -> bool {
        self.collect_enabled.unwrap_or(true)
    }

    /// Effective collect-service port.
    pub fn collect_port(&self) -> u16 {
        self.collect_port
            .unwrap_or(crate::services::collect::DEFAULT_PORT)
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

    /// Whether point-cloud previews get eye-dome lighting and gap filling.
    pub fn point_enhance(&self) -> bool {
        self.point_enhance.unwrap_or(true)
    }
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

    /// An empty registry resolves to the default library without any disk
    /// access — `active_entry` is what the UI asks on every frame.
    #[test]
    fn an_empty_registry_resolves_to_the_default_library() {
        let config = AppConfig::default();
        assert_eq!(config.active_slug(), paths::DEFAULT_LIBRARY_SLUG);
        let entry = config.active_entry();
        assert_eq!(entry.slug, paths::DEFAULT_LIBRARY_SLUG);
        assert_eq!(entry.name, DEFAULT_LIBRARY_NAME);
        assert_eq!(entry.dir(), paths::library_dir(paths::DEFAULT_LIBRARY_SLUG));
    }

    /// A recorded active library that has been forgotten falls back to the
    /// default rather than pointing at a directory that is not there.
    #[test]
    fn a_stale_active_slug_falls_back_to_the_default() {
        let config = AppConfig {
            active_library: Some("gone".into()),
            ..Default::default()
        };
        assert_eq!(config.active_slug(), paths::DEFAULT_LIBRARY_SLUG);
    }

    /// Slugs are ASCII-safe and unique; a name with no ASCII form still gets
    /// one, because the slug is a directory name.
    #[test]
    fn slugs_are_ascii_safe_and_unique() {
        let mut config = AppConfig::default();
        assert_eq!(config.unique_slug("Work 2026"), "work-2026");
        config.libraries.push(LibraryEntry {
            slug: "work".into(),
            name: "Work".into(),
        });
        assert_eq!(config.unique_slug("work"), "work-2");
        config.libraries.push(LibraryEntry {
            slug: "work-2".into(),
            name: "Work 2".into(),
        });
        assert_eq!(config.unique_slug("work"), "work-3");
        // No ASCII characters at all: a numbered fallback, never an empty slug.
        assert_eq!(config.unique_slug("素材库"), "library");
        config.libraries.push(LibraryEntry {
            slug: "library".into(),
            name: "素材库".into(),
        });
        assert_eq!(config.unique_slug("另一个"), "library-2");
    }

    /// A library's data and cache directories hang off the two roots, and
    /// they are different directories — deleting the cache must not touch the
    /// database.
    #[test]
    fn a_librarys_directories_are_separate_and_rooted() {
        let entry = LibraryEntry {
            slug: "work".into(),
            name: "Work".into(),
        };
        assert_eq!(entry.dir(), paths::data_dir().join("libraries/work"));
        assert_eq!(entry.cache_dir(), paths::cache_dir().join("libraries/work"));
        assert_ne!(entry.dir(), entry.cache_dir());
    }

    /// A crash mid-save must never leave a truncated config: the write lands
    /// on a temp file first and the rename into place is atomic, so the
    /// destination only ever holds a complete file.
    #[test]
    fn a_saved_config_never_replaces_itself_half_written() {
        let dir = std::env::temp_dir().join(format!("trove-cfg-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        write_atomic(&path, "{ \"first\": true }").unwrap();
        write_atomic(&path, "{ \"second\": true }").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"second\": true }"
        );
        // No temp litter survives a successful save.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A config that does not parse is moved aside, not overwritten: the file
    /// is the only record of the library registry, and the next save would
    /// otherwise destroy it.
    #[test]
    fn a_corrupt_config_is_quarantined_not_deleted() {
        let dir = std::env::temp_dir().join(format!("trove-cfg-quar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json").unwrap();

        let parsed: std::result::Result<AppConfig, serde_json::Error> =
            serde_json::from_str("{ not json");
        quarantine_corrupt(&path, &parsed.unwrap_err());

        assert!(!path.exists(), "the broken file no longer blocks loading");
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(kept.len(), 1, "exactly the quarantined copy remains");
        assert_eq!(
            std::fs::read_to_string(kept[0].path()).unwrap(),
            "{ not json"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The library's own preferences round-trip through `library.json`, and a
    /// library that has never been configured has an empty watch list.
    #[test]
    fn library_config_round_trips_and_defaults_to_watching() {
        let dir = std::env::temp_dir().join(format!("trove-libcfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let fresh = LibraryConfig::load(&dir);
        assert!(fresh.watched_folders.is_empty());
        assert!(fresh.watch_folders_enabled(), "watching defaults to on");

        let mut config = LibraryConfig::default();
        config
            .add_watched_folder(&dir, PathBuf::from("/tmp/trove-watched"))
            .unwrap();
        // Adding the same folder twice keeps one entry.
        config
            .add_watched_folder(&dir, PathBuf::from("/tmp/trove-watched"))
            .unwrap();
        assert_eq!(config.watched_folders.len(), 1);
        assert_eq!(
            LibraryConfig::load(&dir).watched_folders,
            config.watched_folders
        );

        config
            .remove_watched_folder(&dir, &PathBuf::from("/tmp/trove-watched"))
            .unwrap();
        assert!(LibraryConfig::load(&dir).watched_folders.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
