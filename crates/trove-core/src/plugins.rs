//! The plugin system: named bundles of hooks that extend Trove without the
//! call sites knowing their names.
//!
//! v1 ships one hook — **import-pipeline stages** — because it is where the
//! codebase already points (`media/pipeline.rs` assembles a stage graph from
//! a list) and where third-party behaviour pays off first (a sidecar reader,
//! a custom thumbnailer, extra metadata mining). The registry is deliberately
//! dumb: it stores plugins and flattens their stages; the pipeline owns
//! ordering and validation.
//!
//! The contract, which [`default_pipeline`](crate::media::pipeline) relies on:
//!
//! - registration happens at startup, before the first import — the pipeline
//!   is built once, on first use, and never re-reads the registry;
//! - plugin stages run **after** the built-in ones, so a stage that enriches
//!   what the miner found reads finished metadata;
//! - which plugins are enabled is read from [`AppConfig::disabled_plugins`]
//!   at the same moment, so a toggle takes effect on the next launch.
//!
//! Plugins are compiled in for now. A separate API crate (so an external
//! crate can implement `Plugin` without linking all of trove-core) and
//! dynamic loading are the next steps — see `docs/PLUGIN-SYSTEM.md`.

use std::sync::{Arc, Mutex, OnceLock};

use crate::media::pipeline::Stage;

/// One command a plugin contributes — something a user can bind a key to in
/// Settings ▸ Shortcuts, exactly like the built-in actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    /// Stable id, namespaced `"plugin-id/command-id"` (e.g.
    /// `"sidecar-notes/toggle-mode"`). It is the keybinding configuration
    /// key and the dispatch payload; never renamed once shipped.
    pub action: &'static str,
    /// Default key (`"ctrl-alt-s"`); empty means unbound until the user
    /// assigns one — the command still exists, it is just not on the
    /// keyboard yet.
    pub key: &'static str,
    /// Whether the command answers wherever focus is (bound without a key
    /// context) rather than only inside the asset grid's `Workspace` context.
    pub global: bool,
}

impl PluginCommand {
    /// The plugin that owns this command: everything before the `/`.
    pub fn plugin_name(&self) -> &str {
        self.action.split('/').next().unwrap_or(self.action)
    }
}

/// One Trove plugin: a stable name plus whatever hooks it implements.
///
/// Hooks have default implementations, so a plugin only writes the ones it
/// cares about — a pipeline-only plugin never touches the UI layer.
pub trait Plugin: Send + Sync {
    /// Stable identifier used in configuration (`disabled_plugins`) and logs.
    /// Kebab-case by convention (`"sidecar-notes"`); never renamed once
    /// shipped, it is a persisted key.
    fn name(&self) -> &'static str;

    /// Stages appended to the import pipeline, after the built-in ones. Every
    /// file being imported runs through them; a per-file failure marks that
    /// file skipped, exactly like a built-in stage.
    fn pipeline_stages(&self) -> Vec<Arc<dyn Stage>> {
        Vec::new()
    }

    /// Commands this plugin contributes to the keyboard. Dispatch goes
    /// through one generic action carrying the command id; the app routes it
    /// back to the plugin that declared it.
    fn commands(&self) -> Vec<PluginCommand> {
        Vec::new()
    }
}

/// The set of registered plugins. A plain struct so tests can build their own
/// without touching the process-wide registry in [`global`].
#[derive(Default)]
pub struct Registry {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Add a plugin. A name that is already registered is ignored —
    /// re-registering must not run a plugin's stages twice.
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        if self
            .plugins
            .iter()
            .any(|known| known.name() == plugin.name())
        {
            tracing::debug!(
                plugin = plugin.name(),
                "plugin already registered; ignoring"
            );
            return;
        }
        self.plugins.push(plugin);
    }

    /// Every registered plugin, in registration order.
    pub fn all(&self) -> &[Arc<dyn Plugin>] {
        &self.plugins
    }

    /// Stages contributed by every plugin whose name is not in `disabled`,
    /// in registration order.
    pub fn pipeline_stages(&self, disabled: &[String]) -> Vec<Arc<dyn Stage>> {
        self.plugins
            .iter()
            .filter(|plugin| !disabled.iter().any(|name| name == plugin.name()))
            .flat_map(|plugin| plugin.pipeline_stages())
            .collect()
    }
}

fn global() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::new()))
}

/// Register a plugin process-wide. Call at startup, before the first import —
/// the pipeline snapshots its stages once and never re-reads the registry.
/// Registering the same name twice is a no-op.
pub fn register(plugin: Arc<dyn Plugin>) {
    global()
        .lock()
        .expect("plugin registry lock")
        .register(plugin);
}

/// Every registered plugin, in registration order (the settings page reads
/// this).
pub fn all() -> Vec<Arc<dyn Plugin>> {
    global()
        .lock()
        .expect("plugin registry lock")
        .all()
        .to_vec()
}

/// Stages the import pipeline appends: every enabled plugin's contribution,
/// in registration order. `disabled` holds plugin names switched off in the
/// configuration.
pub fn pipeline_stages(disabled: &[String]) -> Vec<Arc<dyn Stage>> {
    global()
        .lock()
        .expect("plugin registry lock")
        .pipeline_stages(disabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::pipeline::StageIo;

    /// A plugin that contributes one no-op marker stage.
    struct Stages(&'static str);
    impl Plugin for Stages {
        fn name(&self) -> &'static str {
            self.0
        }
        fn pipeline_stages(&self) -> Vec<Arc<dyn Stage>> {
            vec![Arc::new(Marker)]
        }
    }

    /// A stage that does nothing; only its presence in the list is observed.
    struct Marker;
    impl Stage for Marker {
        fn name(&self) -> &'static str {
            "marker"
        }
        fn run(&self, _io: &mut StageIo) -> crate::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_fresh_registry_is_empty() {
        let registry = Registry::new();
        assert!(registry.all().is_empty());
        assert!(registry.pipeline_stages(&[]).is_empty());
    }

    #[test]
    fn registering_the_same_name_twice_keeps_one_plugin() {
        let mut registry = Registry::new();
        registry.register(Arc::new(Stages("a")));
        registry.register(Arc::new(Stages("a")));
        assert_eq!(registry.all().len(), 1);
    }

    #[test]
    fn disabled_plugins_contribute_no_stages() {
        let mut registry = Registry::new();
        registry.register(Arc::new(Stages("stages")));
        assert_eq!(registry.pipeline_stages(&[]).len(), 1);
        assert!(registry.pipeline_stages(&["stages".to_string()]).is_empty());
    }
}
