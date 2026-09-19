//! The sidecar-notes plugin: the reference implementation of every plugin
//! surface — a pipeline stage, its own settings page, and a keyboard command.
//!
//! For each imported file it looks for a sibling sidecar named after the
//! whole file name — `IMG_0001.jpg` reads `IMG_0001.jpg.trove.json` — and,
//! when that file carries a `title` string, applies it to the mined title.
//!
//! ```json
//! { "title": "The cover shot" }
//! ```
//!
//! Whether the sidecar *overrides* an existing (EXIF) title or only *fills*
//! the gap is the plugin's setting, on its own Settings page; the command
//! `sidecar-notes/toggle-mode` (bound to `ctrl-alt-s` by default) flips it
//! from the keyboard. The stage runs after the built-in miner — plugin
//! stages are appended, see `trove_core::plugins` — which is what makes the
//! override well-defined. Every stage failure — no sidecar, unreadable,
//! malformed — is silent and leaves the mined metadata alone: enrichment may
//! not fail an import.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::setting::SettingPage;
use gpui_kit::{App, IntoElement as _, ParentElement as _, SharedString, Styled as _, Window, div};

use gpui_kit::Keystroke;
use trove_core::config::AppConfig;

use crate::plugins::i18n::pt;

use trove_core::media::pipeline::{Cost, Stage, StageIo};
use trove_core::plugins::{Plugin, PluginCommand};

/// How a sidecar title meets the title the file itself carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// The sidecar wins: it is explicit user intent about this exact file.
    #[default]
    Override,
    /// The sidecar only fills a gap: a file with a title of its own keeps it.
    FillMissing,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Override => "override",
            Mode::FillMissing => "fill",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "fill" => Mode::FillMissing,
            _ => Mode::Override,
        }
    }

    /// Localized display name (settings page, toast) — the plugin's own
    /// catalogs, not the app's.
    fn label(self) -> String {
        match self {
            Mode::Override => pt!("plugins.sidecar_notes_mode_override"),
            Mode::FillMissing => pt!("plugins.sidecar_notes_mode_fill"),
        }
    }
}

/// The plugin: one pipeline stage, one command, one settings page — all
/// sharing the mode through `state`, so a settings change takes effect on
/// the next imported file, no relaunch.
pub struct SidecarNotes {
    state: Arc<RwLock<Mode>>,
}

impl SidecarNotes {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(Self::stored_mode())),
        }
    }

    /// The mode as the configuration left it (defaults on first run).
    fn stored_mode() -> Mode {
        AppConfig::load()
            .plugin_settings
            .get(Self::PLUGIN_NAME)
            .and_then(|settings| settings.get("mode"))
            .and_then(|value| value.as_str())
            .map(Mode::parse)
            .unwrap_or_default()
    }

    fn mode(&self) -> Mode {
        *self.state.read().expect("sidecar-notes mode lock")
    }

    /// Apply the mode to the live state and persist it for the next launch.
    fn set_mode(&self, mode: Mode) {
        *self.state.write().expect("sidecar-notes mode lock") = mode;
        let mut config = AppConfig::load();
        config
            .plugin_settings
            .entry(Self::PLUGIN_NAME.to_string())
            .or_default()
            .insert(
                "mode".to_string(),
                serde_json::Value::String(mode.as_str().to_string()),
            );
        let _ = config.save();
    }

    const PLUGIN_NAME: &'static str = "sidecar-notes";
}

impl Default for SidecarNotes {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SidecarNotes {
    fn name(&self) -> &'static str {
        Self::PLUGIN_NAME
    }

    fn pipeline_stages(&self) -> Vec<Arc<dyn Stage>> {
        vec![Arc::new(SidecarNotesStage {
            state: self.state.clone(),
        })]
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            action: "sidecar-notes/toggle-mode",
            key: "ctrl-alt-s",
            global: true,
        }]
    }
}

impl super::AppPlugin for SidecarNotes {
    fn name(&self) -> &'static str {
        Self::PLUGIN_NAME
    }

    /// The plugin's own language files, living beside this source file and
    /// shaped like the app catalogs. They carry every string the settings
    /// page and the shortcuts page render, in every supported language.
    fn translations(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("en", include_str!("locales/sidecar-notes/en.toml")),
            ("zh-CN", include_str!("locales/sidecar-notes/zh-CN.toml")),
        ]
    }

    /// The plugin's own settings page: its behaviour, and where its command's
    /// shortcut stands (rebinding happens in Settings ▸ Shortcuts, where the
    /// command sits beside the built-in actions).
    fn settings_pages(&self) -> Vec<SettingPage> {
        let mode = self.mode();
        let default_key = Self::TOGGLE_COMMAND.1;
        let effective_key = AppConfig::load()
            .keybindings
            .get(Self::TOGGLE_COMMAND.0)
            .cloned()
            .unwrap_or_else(|| default_key.to_string());

        let getter_state = self.state.clone();
        let setter_state = self.state.clone();
        let page = SettingPage::new(pt!("plugins.sidecar_notes_name"))
            .description(pt!("plugins.sidecar_notes_description"))
            .group(
                gpui_kit::component::setting::SettingGroup::new()
                    .title(pt!("plugins.sidecar_notes_group"))
                    .item(
                        gpui_kit::component::setting::SettingItem::new(
                            pt!("plugins.sidecar_notes_mode"),
                            gpui_kit::component::setting::SettingField::dropdown(
                                vec![
                                    (
                                        SharedString::from(Mode::Override.as_str()),
                                        Mode::Override.label().into(),
                                    ),
                                    (
                                        SharedString::from(Mode::FillMissing.as_str()),
                                        Mode::FillMissing.label().into(),
                                    ),
                                ],
                                move |_cx| {
                                    SharedString::from(
                                        Self::mode_of(&getter_state).as_str().to_string(),
                                    )
                                },
                                move |value: SharedString, cx| {
                                    self_set_mode(&setter_state, Mode::parse(&value));
                                    cx.refresh_windows();
                                },
                            ),
                        )
                        .description(pt!("plugins.sidecar_notes_mode_desc")),
                    )
                    .item(gpui_kit::component::setting::SettingItem::render(
                        move |_, _, cx| shortcut_row(cx, &effective_key, Mode::label(mode)),
                    )),
            );
        vec![page]
    }

    fn run_command(&self, command: &str, window: &mut Window, cx: &mut App) {
        if command != Self::TOGGLE_COMMAND.0 {
            return;
        }
        let next = match self.mode() {
            Mode::Override => Mode::FillMissing,
            Mode::FillMissing => Mode::Override,
        };
        self.set_mode(next);
        // The chord works anywhere, so say what it did: the only surface
        // showing the mode is this plugin's settings page.
        window.push_notification(
            Notification::info(pt!("plugins.sidecar_notes_mode_now", mode = next.label())),
            cx,
        );
        cx.refresh_windows();
    }
}

impl SidecarNotes {
    /// (action id, default key) of the toggle command — read by the settings
    /// page and the command handler so the id is written once.
    const TOGGLE_COMMAND: (&'static str, &'static str) =
        ("sidecar-notes/toggle-mode", "ctrl-alt-s");

    fn mode_of(state: &Arc<RwLock<Mode>>) -> Mode {
        *state.read().expect("sidecar-notes mode lock")
    }
}

/// Free-standing `set_mode` for the settings closures, which own a state
/// clone rather than the plugin itself.
fn self_set_mode(state: &Arc<RwLock<Mode>>, mode: Mode) {
    *state.write().expect("sidecar-notes mode lock") = mode;
    let mut config = AppConfig::load();
    config
        .plugin_settings
        .entry("sidecar-notes".to_string())
        .or_default()
        .insert(
            "mode".to_string(),
            serde_json::Value::String(mode.as_str().to_string()),
        );
    let _ = config.save();
}

/// The command's shortcut line on the settings page: the chord it answers to
/// right now, and where to change it.
fn shortcut_row(cx: &App, key: &str, mode_label: String) -> gpui_kit::Div {
    use gpui_kit::component::ActiveTheme as _;

    let chord = match Keystroke::parse(key) {
        Ok(stroke) => Kbd::new(stroke).into_any_element(),
        Err(_) => div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(key.to_string())
            .into_any_element(),
    };
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child(pt!("plugins.sidecar_notes_toggle")))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(pt!(
                            "plugins.sidecar_notes_shortcut_hint",
                            mode = mode_label
                        )),
                ),
        )
        .child(chord)
}

/// The stage doing the work: `<file>.trove.json` → `mined.title`, under the
/// mode the plugin's settings page (or its chord) last set.
struct SidecarNotesStage {
    state: Arc<RwLock<Mode>>,
}

impl Stage for SidecarNotesStage {
    fn name(&self) -> &'static str {
        "sidecar-notes"
    }

    fn cost(&self) -> Cost {
        // One `stat` per imported file; the read only happens when the
        // sidecar exists.
        Cost::Io
    }

    fn run(&self, io: &mut StageIo) -> trove_core::Result<()> {
        let Some(sidecar) = sidecar_path(&io.src) else {
            return Ok(());
        };
        let Ok(text) = std::fs::read_to_string(&sidecar) else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(());
        };
        if let Some(title) = value.get("title").and_then(|v| v.as_str()) {
            let title = title.trim();
            if title.is_empty() {
                return Ok(());
            }
            let fill_only = self.state.read().map(|m| *m == Mode::FillMissing);
            match fill_only {
                Ok(true) if io.mined.title.is_some() => return Ok(()),
                _ => {}
            }
            io.mined.title = Some(title.to_string());
        }
        Ok(())
    }
}

/// `IMG_0001.jpg` → its sibling `IMG_0001.jpg.trove.json`. `None` when the
/// source has no representable file name.
fn sidecar_path(src: &std::path::Path) -> Option<PathBuf> {
    let name = src.file_name()?.to_str()?;
    Some(src.with_file_name(format!("{name}.trove.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn stage_with(mode: Mode) -> SidecarNotesStage {
        SidecarNotesStage {
            state: Arc::new(RwLock::new(mode)),
        }
    }

    #[test]
    fn the_sidecar_sits_beside_the_whole_file_name() {
        let sidecar = sidecar_path(Path::new("/photos/IMG_0001.jpg"));
        assert_eq!(
            sidecar,
            Some(PathBuf::from("/photos/IMG_0001.jpg.trove.json"))
        );
    }

    #[test]
    fn a_nameless_source_has_no_sidecar() {
        assert_eq!(sidecar_path(Path::new("/")), None);
    }

    #[test]
    fn a_title_sidecar_overrides_the_mined_title() {
        let dir = std::env::temp_dir().join(format!("trove-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("IMG_1.jpg");
        std::fs::write(&photo, b"not really a jpeg").unwrap();
        std::fs::write(
            dir.join("IMG_1.jpg.trove.json"),
            br#"{"title": "  From the sidecar  "}"#,
        )
        .unwrap();

        let mut io = StageIo::new(
            &photo,
            &dir,
            &dir,
            trove_core::media::import::ImportStorage::Link,
        )
        .unwrap();
        io.mined.title = Some("from exif".to_string());
        stage_with(Mode::Override).run(&mut io).unwrap();
        assert_eq!(io.mined.title.as_deref(), Some("From the sidecar"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fill mode is the other half of the contract: a file that already has a
    /// title keeps it, a file without one still gets the sidecar's.
    #[test]
    fn fill_mode_defers_to_an_existing_title() {
        let dir = std::env::temp_dir().join(format!("trove-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let titled = dir.join("titled.jpg");
        std::fs::write(&titled, b"jpg").unwrap();
        std::fs::write(
            dir.join("titled.jpg.trove.json"),
            br#"{"title": "sidecar"}"#,
        )
        .unwrap();
        let untitled = dir.join("untitled.jpg");
        std::fs::write(&untitled, b"jpg").unwrap();
        std::fs::write(
            dir.join("untitled.jpg.trove.json"),
            br#"{"title": "sidecar"}"#,
        )
        .unwrap();

        let stage = stage_with(Mode::FillMissing);

        let mut io = StageIo::new(
            &titled,
            &dir,
            &dir,
            trove_core::media::import::ImportStorage::Link,
        )
        .unwrap();
        io.mined.title = Some("from exif".to_string());
        stage.run(&mut io).unwrap();
        assert_eq!(io.mined.title.as_deref(), Some("from exif"));

        let mut io = StageIo::new(
            &untitled,
            &dir,
            &dir,
            trove_core::media::import::ImportStorage::Link,
        )
        .unwrap();
        stage.run(&mut io).unwrap();
        assert_eq!(io.mined.title.as_deref(), Some("sidecar"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_or_missing_title_leaves_the_mined_one() {
        let dir = std::env::temp_dir().join(format!("trove-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("IMG_2.jpg");
        std::fs::write(&photo, b"not really a jpeg").unwrap();
        std::fs::write(dir.join("IMG_2.jpg.trove.json"), br#"{"title": "   "}"#).unwrap();

        let mut io = StageIo::new(
            &photo,
            &dir,
            &dir,
            trove_core::media::import::ImportStorage::Link,
        )
        .unwrap();
        io.mined.title = Some("from exif".to_string());
        stage_with(Mode::Override).run(&mut io).unwrap();
        assert_eq!(io.mined.title.as_deref(), Some("from exif"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mode_round_trips_through_its_config_string() {
        assert_eq!(Mode::parse("override"), Mode::Override);
        assert_eq!(Mode::parse("fill"), Mode::FillMissing);
        // Anything else — an old value, a typo — lands on the default.
        assert_eq!(Mode::parse("nonsense"), Mode::Override);
    }
}
