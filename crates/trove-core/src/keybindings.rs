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
            action: "OpenSettings",
            key: "ctrl-comma",
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
            // The same letter while the stage is up, so `f` toggles — the
            // stage replaces the preview, so the enter binding is out of the
            // dispatch path there and the exit needs its own key.
            action: "ExitVideoFullscreen",
            key: "f",
            context: Some("VideoFullscreen"),
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
