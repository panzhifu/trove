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
/// locale is a plain process global, so this is safe pre-GPUI.
pub fn init_from_config() {
    let config = AppConfig::load();
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

    /// Every catalog must carry the same keys as English — within a recorded
    /// allowance.
    ///
    /// `t!` resolves at runtime, so a key present in `en.toml` and missing
    /// elsewhere does not fail the build; the interface silently shows the raw
    /// key instead. That is how a settings section added in English only would
    /// stay invisible: seven catalogs currently carry ~63 fewer keys than
    /// English, all of them the AI / search-tier pages.
    ///
    /// So this is a ratchet rather than an equality. Adding a key to `en.toml`
    /// without translating it fails the test; translating one forces the
    /// recorded number down, so the table only ever shrinks. `zh-CN` is already
    /// at parity and holds there.
    #[test]
    fn catalogs_do_not_drift_further_from_english() {
        /// (missing, extra) relative to `en.toml`, as measured 2026-09-23.
        const ALLOWED: [(&str, usize, usize); 8] = [
            ("de", 64, 2),
            ("es", 63, 1),
            ("fr", 63, 1),
            ("ja", 63, 1),
            ("ko", 63, 1),
            ("pt", 63, 1),
            ("ru", 63, 1),
            ("zh-CN", 0, 0),
        ];

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let keys_of = |lang: &str| -> std::collections::BTreeSet<String> {
            let text = std::fs::read_to_string(dir.join(format!("{lang}.toml")))
                .unwrap_or_else(|e| panic!("{lang}.toml: {e}"));
            let mut section = String::new();
            let mut keys = std::collections::BTreeSet::new();
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
        };

        let english = keys_of("en");
        assert!(!english.is_empty(), "the English catalog read empty");
        for (lang, allowed_missing, allowed_extra) in ALLOWED {
            let other = keys_of(lang);
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
}
