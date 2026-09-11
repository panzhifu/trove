//! Appearance: the theme catalogue, the light/dark switch, and applying the
//! user's stored preference.
//!
//! The model is gpui-kit's: a `Theme` global holds one named config per mode
//! (light and dark) plus the active `ThemeMode`, while a `ThemeRegistry`
//! global holds every known theme by name. Settings ▸ Appearance picks the
//! mode (follow system / light / dark) and which named theme fills each slot;
//! every framework widget reads `cx.theme()`, so re-skinning is just swapping
//! those two configs and flipping the mode.
//!
//! Two layers, the same split OpenLogi documents:
//!
//! - **Themes** are committed as JSON by the UI framework (and by users in a
//!   `themes/` directory); we only *select* them.
//! - **Appearance** is our own preference — which mode, and which theme per
//!   mode — persisted in [`AppConfig`].

use std::path::PathBuf;

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, SharedString, Window, WindowAppearance};

use trove_core::config::{AppConfig, Appearance};

/// Names of the framework's built-in themes, used when nothing is configured
/// or when the stored name no longer exists (a retired theme).
pub const DEFAULT_LIGHT: &str = "Default Light";
pub const DEFAULT_DARK: &str = "Default Dark";

// Defines `BUILTIN_THEME_JSON: &[&str]` from the build-time-embedded copies of
// the gpui-kit `themes/` directory (`build.rs`).
include!(concat!(env!("OUT_DIR"), "/builtin_themes.rs"));

/// Load every bundled theme into the registry. Call once at startup, after
/// `gpui_kit::init` has seeded the registry global.
pub fn register_builtin_themes(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for json in BUILTIN_THEME_JSON {
        if let Err(error) = registry.load_themes_from_str(json) {
            eprintln!("failed to load a bundled theme: {error}");
        }
    }
}

/// Directory the user drops their own `*.json` themes into. Created on
/// demand; it simply does not contribute anything when missing.
pub fn themes_dir() -> Option<PathBuf> {
    AppConfig::config_dir().map(|d| d.join("themes"))
}

/// Load the user's own themes on top of the bundled ones.
///
/// Same JSON shape as the framework's (`load_themes_from_str` accepts one
/// theme object or an array of them), so a file copied out of the gpui-kit
/// `themes/` directory is a valid starting point.
pub fn register_user_themes(cx: &mut App) -> usize {
    let Some(dir) = themes_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut loaded = 0;
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    // Stable order: a file that redefines an existing name wins by sorting
    // last, so the result does not depend on directory iteration order.
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(json) => {
                let registry = ThemeRegistry::global_mut(cx);
                match registry.load_themes_from_str(&json) {
                    Ok(()) => loaded += 1,
                    Err(error) => {
                        eprintln!("failed to load theme {}: {error}", path.display())
                    }
                }
            }
            Err(error) => eprintln!("failed to read theme {}: {error}", path.display()),
        }
    }
    loaded
}

/// Every theme available for `mode`, in the registry's display order.
pub fn theme_names(cx: &App, mode: ThemeMode) -> Vec<SharedString> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .filter(|theme| theme.mode == mode)
        .map(|theme| theme.name.clone())
        .collect()
}

/// Which mode the stored preference resolves to, given the OS appearance.
pub fn resolve_mode(appearance: Appearance, os: WindowAppearance) -> ThemeMode {
    match appearance {
        Appearance::System => ThemeMode::from(os),
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
    }
}

/// Apply the stored appearance and theme choices to the global `Theme`.
///
/// Pass the window being built so its OS appearance is read directly: on
/// Linux `cx.window_appearance()` routes through a `RefCell` that the
/// appearance observer already holds borrowed, and querying it from there
/// panics. With no window (a settings edit) the platform query is safe, and
/// every open window is refreshed instead.
pub fn apply_from_settings(window: Option<&mut Window>, cx: &mut App) {
    let config = AppConfig::load();
    let os = window
        .as_ref()
        .map_or_else(|| cx.window_appearance(), |w| w.appearance());
    let mode = resolve_mode(config.appearance, os);

    // Pull both configs out of the registry before borrowing the Theme
    // mutably — both live as globals.
    let (light, dark) = {
        let registry = ThemeRegistry::global(cx);
        let pick = |name: Option<&str>, fallback: &str| {
            name.and_then(|n| registry.themes().get(n).cloned())
                .or_else(|| registry.themes().get(fallback).cloned())
        };
        (
            pick(config.theme_light.as_deref(), DEFAULT_LIGHT),
            pick(config.theme_dark.as_deref(), DEFAULT_DARK),
        )
    };
    {
        let theme = Theme::global_mut(cx);
        if let Some(light) = light {
            theme.light_theme = light;
        }
        if let Some(dark) = dark {
            theme.dark_theme = dark;
        }
    }

    Theme::change(mode, window, cx);
    // Theme tokens are app-global: every window has to repaint.
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    // Explicit imports: a glob would drag in a `test` attribute macro from the
    // gpui prelude and make `#[test]` expansion recurse.
    use super::{Appearance, DEFAULT_DARK, DEFAULT_LIGHT, resolve_mode};
    use gpui_kit::WindowAppearance;
    use gpui_kit::component::ThemeMode;

    #[test]
    fn system_follows_the_os_appearance() {
        assert_eq!(
            resolve_mode(Appearance::System, WindowAppearance::Light),
            ThemeMode::Light
        );
        assert_eq!(
            resolve_mode(Appearance::System, WindowAppearance::Dark),
            ThemeMode::Dark
        );
    }

    #[test]
    fn forced_modes_ignore_the_os() {
        assert_eq!(
            resolve_mode(Appearance::Light, WindowAppearance::Dark),
            ThemeMode::Light
        );
        assert_eq!(
            resolve_mode(Appearance::Dark, WindowAppearance::Light),
            ThemeMode::Dark
        );
    }

    #[test]
    fn appearance_round_trips_through_its_config_value() {
        for appearance in [Appearance::System, Appearance::Light, Appearance::Dark] {
            assert_eq!(Appearance::parse(appearance.as_str()), appearance);
        }
        assert_eq!(Appearance::parse("nonsense"), Appearance::System);
    }

    /// The registry is seeded with the framework defaults; if those names ever
    /// change, the picker must be updated with them (or every lookup silently
    /// falls back and the user's stored theme stops applying).
    #[test]
    fn default_theme_names_match_the_framework() {
        assert_eq!(DEFAULT_LIGHT, "Default Light");
        assert_eq!(DEFAULT_DARK, "Default Dark");
    }
}
