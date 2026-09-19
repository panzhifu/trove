//! Plugin-owned translations: every plugin carries its own language files
//! instead of borrowing slots in the app's catalogs.
//!
//! A plugin embeds its `locales/<lang>.toml` files (they live beside the
//! plugin's code, shaped exactly like the app catalogs — same sections, same
//! keys) and hands them up through [`crate::plugins::AppPlugin::translations`]
//! as `(language code, TOML text)` pairs. [`register`] flattens each file into
//! dotted keys at startup; [`translate`] reads the live interface language
//! from `rust_i18n`'s global, falls back to English, and interpolates `%{var}`
//! placeholders the same way `t!` does.
//!
//! Call sites use [`pt!`], which is `t!` shaped: the plugin's own catalogs
//! win, and a miss falls through to the app catalog, so a key that has not
//! moved yet keeps rendering. When a plugin grows its own catalog later, it
//! ships the same way — a dynamic plugin of some future version will read the
//! same files from its plugin directory and call the same [`register`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Language code → (dotted key → text).
fn store() -> &'static Mutex<HashMap<String, HashMap<String, String>>> {
    static STORE: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register one plugin's language files. Called once at startup, from
/// `plugins::init`; the plugin name is only carried for the log line.
pub fn register(plugin: &str, files: &[(&'static str, &'static str)]) {
    let mut store = store().lock().expect("plugin translations lock");
    for (lang, text) in files {
        let Ok(table) = toml::from_str::<toml::Table>(text) else {
            tracing::warn!(
                plugin,
                lang,
                "the plugin's language file did not parse; it is ignored"
            );
            continue;
        };
        let mut flat = HashMap::new();
        flatten("", &toml::Value::Table(table), &mut flat);
        let entries = flat.len();
        store.entry(lang.to_string()).or_default().extend(flat);
        tracing::debug!(plugin, lang, entries, "registered plugin translations");
    }
}

/// Flatten a TOML table into dotted keys, keeping strings only — locale
/// catalogs carry copy, not numbers or nested arrays.
fn flatten(prefix: &str, value: &toml::Value, out: &mut HashMap<String, String>) {
    match value {
        toml::Value::String(text) => {
            out.insert(prefix.to_string(), text.clone());
        }
        toml::Value::Table(table) => {
            for (key, nested) in table {
                let dotted = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&dotted, nested, out);
            }
        }
        _ => {}
    }
}

/// Look a plugin string up in the live interface language (English when the
/// language has no catalog for it), with `t!`'s `%{var}` interpolation.
/// `None` when no plugin carries the key — the caller decides the fallback.
pub fn translate(key: &str, patterns: &[&str], values: &[String]) -> Option<String> {
    let store = store().lock().expect("plugin translations lock");
    let current = rust_i18n::locale().to_string();
    let text = store
        .get(&current)
        .and_then(|catalog| catalog.get(key))
        .or_else(|| store.get("en").and_then(|catalog| catalog.get(key)));
    let Some(text) = text else {
        // A miss here is how a key-shape mismatch between the code and a
        // plugin's catalogs surfaces — silent at compile time, invisible at
        // the default log level, and exactly what a `plugins.*` literal in
        // the UI means. One debug line makes it findable.
        tracing::debug!(key, "no plugin translation carries this key");
        return None;
    };
    Some(rust_i18n::replace_patterns(text, patterns, values))
}

/// The plugin-translation lookup, `t!`-shaped: same keys, same `%{var}`
/// interpolation, but the plugin's own catalogs are consulted before the
/// app's.
macro_rules! pt {
    ($key:literal $(, $name:ident = $value:expr)* $(,)?) => {{
        let values: Vec<String> = vec![$($value.to_string()),*];
        // `replace_patterns` matches the bare name inside `%{…}`, braces off.
        $crate::plugins::i18n::translate(
            $key,
            &[$(stringify!($name)),*],
            &values,
        )
        .unwrap_or_else(|| rust_i18n::t!($key $(, $name = $value)*).to_string())
    }};
}
pub(crate) use pt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_catalogs_resolve_interpolate_and_fall_back() {
        register("test-plugin", &[
            (
                "en",
                "[plugins]\ntest_plugin_greeting = \"Hello %{name}\"\ntest_plugin_plain = \"Plain\"\n",
            ),
            ("zh-CN", "[plugins]\ntest_plugin_greeting = \"你好 %{name}\"\n"),
        ]);

        // The live locale in the test process is `en`: the interpolated
        // English string wins.
        assert_eq!(
            translate(
                "plugins.test_plugin_greeting",
                &["name"],
                &["Trove".to_string()]
            ),
            Some("Hello Trove".to_string())
        );
        assert_eq!(
            translate("plugins.test_plugin_plain", &[], &[]),
            Some("Plain".to_string())
        );

        // A key no plugin carries: the caller's fallback decides, so `None`
        // is the contract.
        assert_eq!(translate("plugins.test_plugin_missing", &[], &[]), None);

        // The `pt!` shape: plugin hit interpolates, plugin miss falls through
        // to the app catalog (which returns the key itself for a miss).
        assert_eq!(pt!("plugins.test_plugin_greeting", name = "Trove"), "Hello Trove");
        assert_eq!(pt!("plugins.test_plugin_plain"), "Plain");
    }
}
