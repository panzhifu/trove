//! Custom keybinding configuration.
//!
//! Users can customize keyboard shortcuts via the Settings ▸ Shortcuts page.
//! Custom keybindings are persisted in `AppConfig.keybindings` and override
//! defaults at startup.

use std::collections::HashMap;

/// A keybinding entry: maps a key string to an action name.
///
/// Example: `"enter" => "OpenPreview"`, `"ctrl-p" => "OpenPreview"`.
///
/// The key string follows gpui-kit's `KeyBinding` format:
/// - Modifier prefixes: `ctrl-`, `shift-`, `alt-`, `cmd-` (order doesn't matter)
/// - Key name: `enter`, `delete`, `backspace`, `escape`, `tab`, `space`,
///   `left`, `right`, `up`, `down`, `home`, `end`, `page-up`, `page-down`,
///   `f1`-`f12`, or a single character (`a`-`z`, `0`-`9`, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct KeyBindingConfig {
    /// The key string (e.g. `"enter"`, `"ctrl-shift-z"`).
    pub key: String,
    /// Human-readable description of the action.
    pub description: String,
    /// The context in which this binding is active (`None` = global).
    pub context: Option<String>,
}

/// All configurable keybindings with their default values.
///
/// This is the single source of truth for what can be rebound. Add a new
/// entry here to make an action's shortcut user-configurable.
pub fn default_keybindings() -> Vec<KeyBindingConfig> {
    vec![
        KeyBindingConfig {
            key: "left".into(),
            description: "Move selection left".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "right".into(),
            description: "Move selection right".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "up".into(),
            description: "Move selection up".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "down".into(),
            description: "Move selection down".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "enter".into(),
            description: "Open preview".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "delete".into(),
            description: "Move to trash".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "backspace".into(),
            description: "Move to trash".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "ctrl-a".into(),
            description: "Select all".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "escape".into(),
            description: "Clear selection".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "ctrl-z".into(),
            description: "Undo".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "ctrl-shift-z".into(),
            description: "Redo".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "i".into(),
            description: "Import files".into(),
            context: None,
        },
        KeyBindingConfig {
            key: "ctrl-comma".into(),
            description: "Open settings".into(),
            context: None,
        },
        KeyBindingConfig {
            key: "ctrl-f".into(),
            description: "Search".into(),
            context: Some("Workspace".into()),
        },
        KeyBindingConfig {
            key: "?".into(),
            description: "Show shortcuts help".into(),
            context: None,
        },
    ]
}

/// Resolve the effective key string for an action.
///
/// Returns the custom binding from `overrides` if present, otherwise falls back
/// to the default.
pub fn resolve_key(
    action_name: &str,
    overrides: &HashMap<String, String>,
    defaults: &[KeyBindingConfig],
) -> Option<String> {
    if let Some(custom) = overrides.get(action_name) {
        return Some(custom.clone());
    }
    defaults
        .iter()
        .find(|d| d.description == action_name || d.key == action_name)
        .map(|d| d.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keybindings_not_empty() {
        let defaults = default_keybindings();
        assert!(!defaults.is_empty());
        // Each key should be unique.
        let mut keys: Vec<&str> = defaults.iter().map(|d| d.key.as_str()).collect();
        let keys_clone = keys.clone();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), keys_clone.len(), "duplicate keys in defaults");
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
            resolve_key("left", &overrides, &defaults),
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
