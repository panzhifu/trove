//! Interface-language resolution and live switching.
//!
//! Catalogs live in `locales/*.toml` and are embedded at compile time by the
//! `rust_i18n::i18n!` invocation in `main.rs` (`en.toml` is the English source
//! of truth). The active locale is the process-global inside `rust_i18n`, so
//! switching it re-localizes our call sites *and* gpui-kit's built-in widget
//! strings, which read the same global. After a runtime switch the caller must
//! `cx.refresh_windows()` (and rebuild the native menus) for open views to
//! pick up the new catalog.

use trove_core::config::AppConfig;

/// Supported UI languages: `(code, native display name)`, picker order.
pub const SUPPORTED: &[(&str, &str)] = &[
    ("en", "English"),
    ("zh-CN", "简体中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("es", "Español"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("pt", "Português"),
    ("ru", "Русский"),
];

/// Map a BCP-47-ish code onto a supported catalog: an exact match wins,
/// otherwise the language prefix (`zh-*` → `zh-CN`), else English.
fn resolve(code: &str) -> &'static str {
    if let Some((exact, _)) = SUPPORTED.iter().find(|(c, _)| *c == code) {
        return exact;
    }
    let prefix = code.split(['-', '_']).next().unwrap_or("");
    SUPPORTED
        .iter()
        .find(|(c, _)| c.split('-').next() == Some(prefix))
        .map(|(c, _)| *c)
        .unwrap_or("en")
}

/// Locale chosen by the config: an explicit language, else the system
/// preference, else English.
fn effective(language: Option<&str>) -> &'static str {
    language
        .map(resolve)
        .or_else(|| sys_locale::get_locale().as_deref().map(resolve))
        .unwrap_or("en")
}

/// Apply the configured language at startup, before any window opens. The
/// locale is a plain process global, so this is safe pre-GPUI. The config
/// comes in from the caller: the boot reads it once, and every consumer
/// afterwards works from that same read.
pub fn init_from_config(config: &AppConfig) {
    rust_i18n::set_locale(effective(config.language.as_deref()));
}

/// Switch the live locale (None = follow system) and persist the choice.
/// The locale switch always applies; a persist failure is returned to the
/// caller (which can surface it in the UI) — the choice just won't survive
/// a restart. The caller repaints open windows and rebuilds menus afterwards.
pub fn set_language(language: Option<String>) -> trove_core::error::Result<()> {
    let mut config = AppConfig::load();
    let result = config.set_language(language);
    rust_i18n::set_locale(effective(config.language.as_deref()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn resolves_exact_prefix_and_fallback() {
        assert_eq!(resolve("en"), "en");
        assert_eq!(resolve("zh-CN"), "zh-CN");
        assert_eq!(resolve("zh-TW"), "zh-CN");
        assert_eq!(resolve("zh"), "zh-CN");
        assert_eq!(resolve("fr"), "fr");
        assert_eq!(resolve("pt-BR"), "pt");
        assert_eq!(resolve("pt"), "pt");
        assert_eq!(resolve("ja-JP"), "ja");
        assert_eq!(resolve("ko"), "ko");
        assert_eq!(resolve("es"), "es");
        assert_eq!(resolve("de"), "de");
        assert_eq!(resolve("ru"), "ru");
        assert_eq!(resolve("ar"), "en"); // Arabic not yet supported
        assert_eq!(effective(Some("zh-TW")), "zh-CN");
    }

    /// Parse a locale catalog into its flat `section.key` set. Line-based on
    /// purpose: the catalogs are one level of sections and the parser must
    /// stay as dumb as the files it guards.
    fn catalog_keys(lang: &str) -> BTreeSet<String> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let text = std::fs::read_to_string(dir.join(format!("{lang}.toml")))
            .unwrap_or_else(|e| panic!("{lang}.toml: {e}"));
        let mut section = String::new();
        let mut keys = BTreeSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(head) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = head.to_string();
                continue;
            }
            if let Some((key, _)) = line.split_once('=') {
                keys.insert(format!("{section}.{}", key.trim()));
            }
        }
        keys
    }

    /// Every catalog must carry exactly the same keys as English.
    ///
    /// `t!` resolves at runtime, so a key present in `en.toml` and missing
    /// elsewhere does not fail the build; the interface silently shows the raw
    /// key instead. That is how a settings section added in English only would
    /// stay invisible — which is what happened once: the seven non-English
    /// catalogs had drifted to ~63 keys behind English, all of them the AI /
    /// search-tier pages, until 2026-10-01 closed the gap and the allowance
    /// went to zero.
    ///
    /// So this is an equality with memory. Adding a key to `en.toml` without
    /// translating it fails the test here; the per-language rows below stay in
    /// the table (instead of one shared `0`) so a future breach names the
    /// language that slipped.
    #[test]
    fn catalogs_do_not_drift_further_from_english() {
        /// (missing, extra) relative to `en.toml`. Full parity since
        /// 2026-10-01; a non-zero row here means a key was added (or renamed)
        /// without its translations.
        const ALLOWED: [(&str, usize, usize); 8] = [
            ("de", 0, 0),
            ("es", 0, 0),
            ("fr", 0, 0),
            ("ja", 0, 0),
            ("ko", 0, 0),
            ("pt", 0, 0),
            ("ru", 0, 0),
            ("zh-CN", 0, 0),
        ];

        let english = catalog_keys("en");
        assert!(!english.is_empty(), "the English catalog read empty");
        for (lang, allowed_missing, allowed_extra) in ALLOWED {
            let other = catalog_keys(lang);
            let missing: Vec<_> = english.difference(&other).collect();
            let extra: Vec<_> = other.difference(&english).collect();
            assert!(
                missing.len() <= allowed_missing && extra.len() <= allowed_extra,
                "{lang}.toml drifted past its allowance ({} missing / {} extra allowed                  {allowed_missing}/{allowed_extra}):\n  missing: {missing:?}\n  extra: {extra:?}",
                missing.len(),
                extra.len(),
            );
            assert!(
                missing.len() >= allowed_missing && extra.len() >= allowed_extra,
                "{lang}.toml is better than recorded ({} missing / {} extra, allowance                  {allowed_missing}/{allowed_extra}) — lower the allowance in ALLOWED so the                  ratchet keeps it from sliding back",
                missing.len(),
                extra.len(),
            );
        }
    }

    /// Literal `t!(\"…\")` keys across the app source, with the file they
    /// came from. The scan is deliberately narrow: a `t!` whose `t` is
    /// preceded by an identifier character (that is `pt!`, the plugin
    /// lookup) is skipped, and keys built at runtime
    /// (`format!("workspace.filter_tool_{tool}")`) are invisible to it by
    /// construction — those are the two known ways a used key can hide from
    /// this guard.
    fn literal_t_keys() -> Vec<(String, String)> {
        fn collect(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, files);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }

        let mut src = Vec::new();
        collect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut src);
        src.sort();

        let mut found = Vec::new();
        for file in src {
            let text = std::fs::read_to_string(&file).unwrap();
            let bytes = text.as_bytes();
            let mut index = 0;
            while let Some(offset) = text[index..].find("t!(") {
                let at = index + offset;
                index = at + 3;
                // Skip `pt!(` and any other ident ending in `t!`, and the
                // `t!(` occurrences that live inside string literals or
                // comments (this scanner's own needle among them).
                if at > 0
                    && (bytes[at - 1].is_ascii_alphanumeric()
                        || bytes[at - 1] == b'"'
                        || bytes[at - 1] == b'`'
                        || bytes[at - 1] == b'\\')
                {
                    continue;
                }
                // Allow whitespace between the `(` and the first literal
                // (multi-line `t!(` call sites exist).
                let rest = &text[index..];
                let Some(indent) = rest.find(|c: char| !c.is_whitespace()) else {
                    continue;
                };
                let rest = &rest[indent..];
                let Some(key) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) else {
                    continue;
                };
                found.push((
                    file.file_name().unwrap().to_string_lossy().into_owned(),
                    key.to_string(),
                ));
            }
        }
        found
    }

    /// A key the code asks for but no catalog carries is the one i18n bug
    /// the parity test above cannot see: every catalog agrees on the same
    /// wrong key set, and rust_i18n answers the miss with the raw key, so
    /// the UI shows `workspace.create_sequence` where a menu label should
    /// be — which is what happened when the sequence menu entries landed
    /// as numbered `0`…`3` keys. This holds the catalogs to the source:
    /// every literal `t!` key must exist in `en.toml`.
    #[test]
    fn every_literal_t_key_exists_in_the_catalog() {
        let keys = catalog_keys("en");
        let used = literal_t_keys();
        let missing: Vec<&(String, String)> =
            used.iter().filter(|(_, key)| !keys.contains(key)).collect();
        assert!(
            missing.is_empty(),
            "keys used in code but absent from en.toml (rust_i18n shows the raw key in the UI):\n{}",
            missing
                .iter()
                .map(|(file, key)| format!("  {file}: {key}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
