//! The app-side plugin surface: hooks that need the UI layer, and the
//! registration point that ties both halves together.
//!
//! Split from [`trove_core::plugins`](trove_core::plugins) along the crate
//! boundary: the core registry holds hooks whose types live in trove-core
//! (pipeline stages), this one holds hooks whose types need gpui-kit
//! (settings pages). A plugin implements both traits on one type and
//! [`init`] registers it in each registry.
//!
//! The v1 UI hook is settings pages — the settings window appends
//! [`settings_pages`] after its built-in pages, and every entry the user can
//! toggle (enable/disable) lives on the built-in plugins page
//! (`dialogs/settings/plugins.rs`), which iterates the core registry so a
//! pipeline-only plugin still shows up.

use std::sync::Arc;

use gpui_kit::component::setting::SettingPage;
use gpui_kit::{App, Global, Window};

pub mod builtin;
pub mod i18n;

/// A plugin's UI-facing hooks, implemented on the same type as
/// [`trove_core::plugins::Plugin`].
pub trait AppPlugin: Send + Sync {
    /// The stable name from the core [`trove_core::plugins::Plugin`] trait —
    /// repeated here so the UI can match a hook back to its configuration
    /// entry without a downcast.
    fn name(&self) -> &'static str;

    /// The plugin's own language files, as `(language code, TOML text)`
    /// pairs — the files live beside the plugin's code (embedded with
    /// `include_str!`) and mirror the app catalogs' shape, so the plugin's
    /// `zh-CN.toml` reads exactly like the slice of the app catalog it
    /// replaces. Registered once at startup; lookups follow the live
    /// interface language and fall back to English. See [`i18n`].
    fn translations(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Setting pages appended after the built-in pages. Rebuilt on every
    /// render of the settings window, so re-reading the locale or config is
    /// safe and a language switch is picked up like any built-in page.
    fn settings_pages(&self) -> Vec<SettingPage> {
        Vec::new()
    }

    /// Run one of this plugin's declared commands (see
    /// [`trove_core::plugins::Plugin::commands`]) — the payload of a bound
    /// key or a menu item, already routed to this plugin by name.
    fn run_command(&self, command: &str, window: &mut Window, cx: &mut App) {
        let _ = (command, window, cx);
    }
}

/// Registered app plugins, living as a gpui global so every window reads the
/// same set (the same pattern the theme registry uses).
#[derive(Default)]
struct AppPlugins {
    plugins: Vec<Arc<dyn AppPlugin>>,
}

impl Global for AppPlugins {}

/// Register the built-in plugins. Called once at startup, after
/// `gpui_kit::init` and before the first window opens — the core registry
/// must be complete before the first import builds the pipeline.
pub fn init(cx: &mut App) {
    let sidecar_notes = Arc::new(builtin::SidecarNotes::new());
    // One plugin, two registries: the pipeline hook goes to the core (which
    // knows nothing of gpui), the settings hook stays here.
    trove_core::plugins::register(sidecar_notes.clone());
    // The plugin's own language files join the translation store before any
    // window can render its copy.
    i18n::register(sidecar_notes.name(), &sidecar_notes.translations());
    cx.set_global(AppPlugins {
        plugins: vec![sidecar_notes],
    });
}

/// Setting pages from every app plugin that is not disabled, in registration
/// order — the settings window appends these after its own pages.
pub fn settings_pages(cx: &App) -> Vec<SettingPage> {
    let Some(registry) = cx.try_global::<AppPlugins>() else {
        return Vec::new();
    };
    let disabled = trove_core::config::AppConfig::load().disabled_plugins;
    registry
        .plugins
        .iter()
        .filter(|plugin| !disabled.iter().any(|name| name == plugin.name()))
        .flat_map(|plugin| plugin.settings_pages())
        .collect()
}

/// Route a plugin command (the payload of [`RunPluginCommand`]) to the
/// plugin whose name prefixes it. Unknown or disabled commands are dropped —
/// a stale keybinding must not keep firing a plugin the user switched off.
pub fn run_command(command: &str, window: &mut Window, cx: &mut App) {
    let Some(registry) = cx.try_global::<AppPlugins>() else {
        return;
    };
    let owner = command.split('/').next().unwrap_or(command);
    let disabled = trove_core::config::AppConfig::load().disabled_plugins;
    if disabled.iter().any(|name| name == owner) {
        return;
    }
    // The registry borrow ends before the call: the plugin's own handler
    // receives `cx` and may re-enter the app.
    let plugin = registry
        .plugins
        .iter()
        .find(|plugin| plugin.name() == owner)
        .cloned();
    if let Some(plugin) = plugin {
        plugin.run_command(command, window, cx);
    }
}
