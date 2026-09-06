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
pub const SUPPORTED: &[(&str, &str)] = &[("en", "English"), ("zh-CN", "简体中文")];

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
/// The caller repaints open windows and rebuilds menus afterwards.
pub fn set_language(language: Option<String>) {
    let mut config = AppConfig::load();
    if let Err(e) = config.set_language(language) {
        eprintln!("save language setting: {e}");
    }
    rust_i18n::set_locale(effective(config.language.as_deref()));
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
        assert_eq!(resolve("fr"), "en");
        assert_eq!(effective(Some("zh-TW")), "zh-CN");
    }
}
