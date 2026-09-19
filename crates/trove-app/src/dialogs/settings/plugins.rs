//! Plugins ▸ every registered plugin with its enable switch.
//!
//! The page iterates the core plugin registry (see `trove_core::plugins`),
//! so a plugin shows up here as soon as it registers — even a pipeline-only
//! one with no settings of its own. App plugins that contribute pages have
//! them appended after this one by the renderer (see `plugins::settings_pages`).

use super::*;

/// The plugins page: one switch per registered plugin, plus the note that a
/// toggle is read when the import pipeline is first built.
pub(super) fn plugins_page() -> SettingPage {
    let mut group = SettingGroup::new().title(rust_i18n::t!("plugins.page_group").to_string());

    let plugins = trove_core::plugins::all();
    if plugins.is_empty() {
        group = group.item(
            SettingItem::new(
                rust_i18n::t!("plugins.none").to_string(),
                SettingField::element(SettingText {
                    text: rust_i18n::t!("plugins.none_desc").to_string(),
                }),
            )
            .description(rust_i18n::t!("plugins.none_hint").to_string()),
        );
    }
    for plugin in plugins {
        let name = plugin.name().to_string();
        // The copy is looked up by plugin name (`plugins.<name>.name` /
        // `.description`); a plugin that ships no catalog entry falls back
        // to the key itself, same as every other missing key.
        let title = localized(&name, "name").unwrap_or_else(|| name.clone());
        let mut item = SettingItem::new(
            title,
            SettingField::switch(
                {
                    let name = name.clone();
                    move |_cx| {
                        !trove_core::config::AppConfig::load()
                            .disabled_plugins
                            .iter()
                            .any(|known| known == &name)
                    }
                },
                {
                    let name = name.clone();
                    move |value: bool, cx| {
                        let mut config = trove_core::config::AppConfig::load();
                        if value {
                            config.disabled_plugins.retain(|known| known != &name);
                        } else if !config.disabled_plugins.iter().any(|known| known == &name) {
                            config.disabled_plugins.push(name.clone());
                        }
                        let _ = config.save();
                        cx.refresh_windows();
                    }
                },
            ),
        );
        if let Some(description) = localized(&name, "description") {
            item = item.description(description);
        }
        group = group.item(item);
    }

    SettingPage::new(rust_i18n::t!("plugins.page_title").to_string())
        .icon(IconName::Cpu)
        .description(rust_i18n::t!("plugins.page_desc").to_string())
        .group(group)
}

/// `plugins.<name>_<suffix>` (the catalogs' flat key shape) in the active
/// locale; `None` when the plugin ships no entry for it, so the caller can
/// fall back to the raw name. The plugin's own language files are consulted
/// first (they travel with the plugin), the app catalog second.
fn localized(name: &str, suffix: &str) -> Option<String> {
    let key = format!("plugins.{}_{suffix}", name.replace('-', "_"));
    crate::plugins::i18n::translate(&key, &[], &[]).or_else(|| {
        let text = rust_i18n::t!(key.as_str()).to_string();
        (text != key).then_some(text)
    })
}

/// A read-only text field, used for the empty-registry explainer row.
struct SettingText {
    text: String,
}

impl gpui_kit::component::setting::SettingFieldElement for SettingText {
    type Element = gpui_kit::Div;

    fn render_field(
        &self,
        _options: &gpui_kit::component::setting::RenderOptions,
        _window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::App,
    ) -> Self::Element {
        gpui_kit::div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(self.text.clone())
    }
}
