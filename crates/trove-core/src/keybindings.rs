//! Custom keybinding configuration.
//!
//! Users can customize keyboard shortcuts via the Settings ▸ Shortcuts page.
//! Custom keybindings are persisted in `AppConfig.keybindings` (mapping an
//! action id to a key string) and override defaults at startup.

use std::collections::HashMap;

/// A keybinding entry. `action` is the stable id used as the config key and
/// to resolve the concrete `Action`; `description` is a canonical (English)
/// label used as fallback text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyBindingConfig {
    /// Stable action id (e.g. `"MoveLeft"`).
    pub action: &'static str,
    /// Default key string (e.g. `"enter"`, `"ctrl-shift-z"`).
    pub key: &'static str,
    /// Context in which this binding is active (`None` = global).
    pub context: Option<&'static str>,
}

/// All configurable keybindings with their default values.
///
/// This is the single source of truth for what can be rebound. Add a new
/// entry here to make an action's shortcut user-configurable.
pub fn default_keybindings() -> Vec<KeyBindingConfig> {
    vec![
        KeyBindingConfig {
            action: "MoveLeft",
            key: "left",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "MoveRight",
            key: "right",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "MoveUp",
            key: "up",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "MoveDown",
            key: "down",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "OpenPreview",
            key: "enter",
            context: Some("Workspace"),
        },
        // The grid's own context, not `Workspace`: the search input sits inside
        // `Workspace` as well, and a binding for a bare character there would
        // take that character away from typing.
        KeyBindingConfig {
            action: "QuickLook",
            key: "space",
            context: Some("AssetGrid"),
        },
        KeyBindingConfig {
            action: "TrashSelected",
            key: "delete",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "SelectAll",
            key: "ctrl-a",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "ClearSelection",
            key: "escape",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "Undo",
            key: "ctrl-z",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "Redo",
            key: "ctrl-shift-z",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            // Not plain `ctrl-c`: that would shadow text copy in the search
            // box and the inline editors, which live in the same context.
            action: "CopyImage",
            key: "ctrl-shift-c",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "ImportFiles",
            key: "ctrl-o",
            context: None,
        },
        KeyBindingConfig {
            // The character, not the keysym name: gpui-linux maps `Keysym::comma`
            // to `","` (`platform.rs:1108`), so `"ctrl-comma"` parsed, displayed in
            // Settings ▸ Shortcuts, and never matched a press. The app-side test
            // `every_default_binding_names_a_key_the_platform_can_emit` pins the
            // rule for every entry in this table.
            action: "OpenSettings",
            key: "ctrl-,",
            context: None,
        },
        KeyBindingConfig {
            action: "Screenshot",
            key: "",
            context: None,
        },
        KeyBindingConfig {
            action: "RefreshLibrary",
            key: "f5",
            context: None,
        },
        // Menu-only actions: an empty default key means "unbound until the
        // user assigns one" — `register_keys` skips empty keys, so these stay
        // reachable from the menu while remaining rebindable.
        KeyBindingConfig {
            action: "BatchRename",
            key: "",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "BatchConvert",
            key: "",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            action: "AutoTag",
            key: "",
            context: Some("Workspace"),
        },
        KeyBindingConfig {
            // `f`, as in mpv/VLC/YouTube. The context is the video preview's,
            // not `Workspace`: a bare letter bound there would shadow typing
            // in the search box, which lives in the same context.
            action: "EnterVideoFullscreen",
            key: "f",
            context: Some("VideoPreview"),
        },
        KeyBindingConfig {
            // The player's own space bar, in the same context as `f` and for
            // the same reason: it is live only while a video is on screen, so
            // the grid's search box keeps its spaces. `QuickLook` binds space
            // in `AssetGrid`, which is not rendered during a preview.
            action: "TogglePlayback",
            key: "space",
            context: Some("VideoPreview"),
        },
        KeyBindingConfig {
            // The same letter while the stage is up, so `f` toggles — the
            // stage replaces the preview, so the enter binding is out of the
            // dispatch path there and the exit needs its own key.
            action: "ExitVideoFullscreen",
            key: "f",
            context: Some("VideoFullscreen"),
        },
        KeyBindingConfig {
            // `,` steps one frame back on the previewed clip — the editing
            // convention (Premiere, DaVinci), and the pair is free here. A
            // soundtrack has no frames, so the same keys nudge it five
            // seconds. Bound in the preview's own context, so the character
            // is live only while a clip covers the grid and the search box
            // keeps its typing.
            //
            // The key is written as the character, not the keysym name:
            // gpui-linux maps `Keysym::comma` to `","` and `Keysym::period` to
            // `"."` (`platform.rs:1108-1109`), and `Keystroke::parse` keeps any
            // non-modifier component verbatim as the key — so `"comma"` would
            // parse, display, and never match a press.
            action: "StepFrameBack",
            key: ",",
            context: Some("VideoPreview"),
        },
        KeyBindingConfig {
            // `.` steps one frame forward, and holds: the point is to look at
            // the frame you landed on, not to watch it pass.
            action: "StepFrameForward",
            key: ".",
            context: Some("VideoPreview"),
        },
        KeyBindingConfig {
            // Ctrl+K, the summon-everywhere chord (browsers, launchers): the
            // search box opens with the caret in it wherever the focus was.
            // Global on purpose — a search is worth reaching from the
            // settings dialog or the tags panel too — and a chord no text
            // input claims by default, so it never shadows typing.
            action: "FocusSearch",
            key: "ctrl-k",
            context: None,
        },
    ]
}

/// Resolve the effective key string for an action id.
pub fn resolve_key(
    action: &str,
    overrides: &HashMap<String, String>,
    defaults: &[KeyBindingConfig],
) -> Option<String> {
    if let Some(custom) = overrides.get(action) {
        return Some(custom.clone());
    }
    defaults
        .iter()
        .find(|d| d.action == action)
        .map(|d| d.key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keybindings_not_empty() {
        let defaults = default_keybindings();
        assert!(!defaults.is_empty());
        // Each action should be unique.
        let actions: Vec<&str> = defaults.iter().map(|d| d.action).collect();
        let mut dedup = actions.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), actions.len(), "duplicate actions in defaults");
    }

    #[test]
    fn fullscreen_key_is_scoped_to_the_video_preview() {
        let defaults = default_keybindings();
        let binding = defaults
            .iter()
            .find(|b| b.action == "EnterVideoFullscreen")
            .expect("EnterVideoFullscreen must stay configurable");
        assert_eq!(binding.key, "f");
        assert_eq!(binding.context, Some("VideoPreview"));
    }

    #[test]
    fn quick_look_is_scoped_to_the_grid_not_the_workspace() {
        // The same reason `f` is scoped to `VideoPreview`: `Workspace` wraps the
        // search input too, and a bare `space` bound there would take the space
        // character away from typing a two-word query.
        let defaults = default_keybindings();
        let binding = defaults
            .iter()
            .find(|b| b.action == "QuickLook")
            .expect("QuickLook must stay configurable");
        assert_eq!(binding.key, "space");
        assert_eq!(binding.context, Some("AssetGrid"));
    }

    #[test]
    fn the_two_space_bindings_never_share_a_context() {
        // One space lights up the tile the arrow keys are on, the other holds
        // the video that replaced the grid. Bound in the same context they would
        // both match wherever that context is on the focus path, and a preview
        // that steals the grid's space is a grid you cannot quick-look.
        let defaults = default_keybindings();
        let spaces: Vec<(&str, Option<&str>)> = defaults
            .iter()
            .filter(|b| b.key == "space")
            .map(|b| (b.action, b.context))
            .collect();
        assert_eq!(
            spaces.as_slice(),
            [
                ("QuickLook", Some("AssetGrid")),
                ("TogglePlayback", Some("VideoPreview"))
            ]
        );
    }

    #[test]
    fn the_fullscreen_keys_mirror_each_other() {
        // `f` toggles: the same letter enters the stage and leaves it, so a
        // rebind of one without the other breaks the pairing.
        let defaults = default_keybindings();
        let enter = defaults
            .iter()
            .find(|b| b.action == "EnterVideoFullscreen")
            .expect("enter binding");
        let exit = defaults
            .iter()
            .find(|b| b.action == "ExitVideoFullscreen")
            .expect("exit binding");
        assert_eq!(enter.key, exit.key, "f must toggle fullscreen both ways");
    }

    #[test]
    fn no_bare_letter_in_the_workspace_context() {
        // The search box shares the `Workspace` context, so a single-letter
        // binding there would eat that letter while typing. The fullscreen
        // key is scoped to `VideoPreview` for exactly this reason — and so is
        // the space bar, scoped to `AssetGrid`, because a grid that swallows
        // spaces is a grid you cannot search for two words at once.
        let offenders: Vec<&str> = default_keybindings()
            .iter()
            .filter(|b| b.context == Some("Workspace"))
            .filter(|b| {
                let mut chars = b.key.chars();
                matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == ' ')
                    && chars.next().is_none()
            })
            .map(|b| b.action)
            .collect();
        assert!(
            offenders.is_empty(),
            "bare letter(s) in Workspace: {offenders:?}"
        );
    }

    #[test]
    fn resolve_key_custom_over_default() {
        let defaults = default_keybindings();
        let mut overrides = HashMap::new();
        overrides.insert("OpenPreview".into(), "ctrl-p".into());
        assert_eq!(
            resolve_key("OpenPreview", &overrides, &defaults),
            Some("ctrl-p".into())
        );
    }

    #[test]
    fn resolve_key_falls_back_to_default() {
        let defaults = default_keybindings();
        let overrides = HashMap::new();
        assert_eq!(
            resolve_key("MoveLeft", &overrides, &defaults),
            Some("left".into())
        );
    }

    #[test]
    fn resolve_key_unknown_action() {
        let defaults = default_keybindings();
        let overrides = HashMap::new();
        assert_eq!(resolve_key("NonExistent", &overrides, &defaults), None);
    }
}
