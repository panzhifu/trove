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
            action: "ScreenshotFull",
            key: "",
            context: None,
        },
        KeyBindingConfig {
            action: "ScreenshotRegion",
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
