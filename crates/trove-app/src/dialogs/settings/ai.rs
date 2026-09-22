//! AI page: the OpenAI-compatible embedding endpoint and the vector store it
//! feeds.
//!
//! This started as one group on the Search page and moved out once it grew a
//! second operation: an endpoint is its own subsystem with its own failure
//! modes (an unreachable server, a key the server rejects, a model name it
//! does not know, rows left over from a differently-configured provider),
//! while that page is about the two model-free indexes. Everything a vector
//! needs — the endpoint, its coverage, generating it, deleting it — is on
//! this page, in the order a user meets it.

use gpui_kit::component::setting::NumberFieldOptions;

use super::*;
use crate::library::{AiProbe, ChatProbe};

// ============================ config ========================================

/// The saved embedding config, or defaults when the feature has never been
/// configured.
fn embedding_config() -> trove_core::config::EmbeddingConfig {
    AppConfig::load().ai_embedding.unwrap_or_default()
}

/// Persist one field change to the embedding config (`config.json`, like
/// every other setting).
fn save_embedding_config(
    edit: impl FnOnce(&mut trove_core::config::EmbeddingConfig),
    cx: &mut App,
) {
    let mut config = AppConfig::load();
    edit(config.ai_embedding.get_or_insert_with(Default::default));
    let _ = config.save();
    cx.refresh_windows();
}

// ============================ page ==========================================

/// The AI page: endpoint + connection test, then the vector store's coverage
/// and its two operations.
pub(super) fn ai_page(controller: &Entity<LibraryController>, cx: &App) -> SettingPage {
    let config = embedding_config();
    // One query per paint, shared by the coverage line and the delete button:
    // the count decides both what the line says and whether deleting has
    // anything to do. A library swap or a finished backfill repaints the
    // window (`refresh_windows`), so the number is never stale on screen.
    let coverage = if config.is_configured() {
        controller
            .read(cx)
            .library
            .embedding_coverage(&config.model)
            .ok()
    } else {
        None
    };
    let probe = controller.read(cx).ai_probe.clone();
    let chat_probe = controller.read(cx).chat_probe.clone();

    SettingPage::new(rust_i18n::t!("settings.ai").to_string())
        .icon(IconName::Bot)
        .description(rust_i18n::t!("settings.ai_desc").to_string())
        .resettable(false)
        .group(endpoint_group(controller, &probe))
        .group(vector_group(controller, coverage))
        .group(chat_group(controller, &chat_probe))
        .group(tagging_group(controller))
}

// ============================ endpoint ======================================

/// Base URL / API key / model, plus the connection test. The three inputs
/// follow the shape every OpenAI-compatible server speaks (OpenAI itself,
/// Ollama, LM Studio, vLLM, proxies), so the same three fields cover cloud
/// and local setups alike.
fn endpoint_group(controller: &Entity<LibraryController>, probe: &AiProbe) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.ai_endpoint").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_base_url").to_string(),
            SettingField::input(
                |_cx| SharedString::from(embedding_config().base_url.clone()),
                |value, cx| save_embedding_config(|c| c.base_url = value.to_string(), cx),
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_api_key").to_string(),
            SettingField::input(
                |_cx| SharedString::from(embedding_config().api_key.clone()),
                |value, cx| save_embedding_config(|c| c.api_key = value.to_string(), cx),
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_model").to_string(),
            SettingField::input(
                |_cx| SharedString::from(embedding_config().model.clone()),
                |value, cx| save_embedding_config(|c| c.model = value.to_string(), cx),
            ),
        ))
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_probe").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    let probe = probe.clone();
                    move |_, _, cx| probe_row(&controller, &probe, cx)
                }),
            )
            .description(rust_i18n::t!("settings.ai_probe_desc").to_string()),
        )
}

/// The connection-test row: the last result on the left, the button on the
/// right. The result is the point of the row — a failure names the reason
/// (unreachable, refused, unknown model), which is precisely what the user
/// cannot guess from the three fields above it.
fn probe_row(controller: &Entity<LibraryController>, probe: &AiProbe, cx: &mut App) -> Div {
    let (text, color) = match probe {
        AiProbe::Idle => (
            rust_i18n::t!("settings.ai_probe_idle").to_string(),
            cx.theme().muted_foreground,
        ),
        AiProbe::Running => (
            rust_i18n::t!("settings.ai_probe_running").to_string(),
            cx.theme().muted_foreground,
        ),
        AiProbe::Ok { dim } => (
            rust_i18n::t!("settings.ai_probe_ok", dim = *dim).to_string(),
            cx.theme().success,
        ),
        AiProbe::Failed { message } => (
            rust_i18n::t!("settings.ai_probe_failed", error = message.as_str()).to_string(),
            cx.theme().danger,
        ),
    };
    let running = probe.is_running();
    let controller = controller.clone();
    h_flex()
        .w_full()
        .items_center()
        .justify_end()
        .gap_2()
        .child(div().text_sm().text_color(color).child(text))
        .child(
            Button::new("ai-probe")
                .outline()
                .small()
                .disabled(running)
                .label(rust_i18n::t!("settings.ai_probe_run").to_string())
                .on_click(move |_, _, cx| {
                    crate::library::jobs::test_embedding_endpoint_app(&controller, cx);
                }),
        )
}

// ============================ vectors =======================================

/// Coverage, generate and delete — the whole life of a vector store.
fn vector_group(
    controller: &Entity<LibraryController>,
    coverage: Option<(u64, u64)>,
) -> SettingGroup {
    let embedded = coverage.map_or(0, |(embedded, _)| embedded);
    SettingGroup::new()
        .title(rust_i18n::t!("settings.ai_vectors").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_coverage").to_string(),
                SettingField::render(move |_, _, cx| ai_coverage_row(coverage, cx)),
            )
            .description(rust_i18n::t!("settings.ai_coverage_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_generate").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| ai_action_row(controller.clone(), cx)
                }),
            )
            .description(rust_i18n::t!("settings.ai_generate_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_delete").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| ai_delete_row(controller.clone(), embedded, cx)
                }),
            )
            .description(rust_i18n::t!("settings.ai_delete_desc").to_string()),
        )
}

/// Live coverage for the configured model: live assets carrying a vector
/// under it, out of all live assets. Not configured → a dash.
fn ai_coverage_row(coverage: Option<(u64, u64)>, cx: &mut App) -> Div {
    let text = match coverage {
        Some((embedded, total)) => rust_i18n::t!(
            "settings.ai_coverage_value",
            embedded = embedded,
            total = total
        )
        .to_string(),
        None => "—".into(),
    };
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// Generate or cancel, depending on whether a backfill is running. The job
/// runs on the backend task thread ([`crate::library::jobs`] starts and
/// watches it), so the button never blocks the window.
fn ai_action_row(controller: Entity<LibraryController>, cx: &mut App) -> Div {
    let running = controller
        .read(cx)
        .library
        .tasks()
        .is_running(trove_core::tasks::TaskKind::EmbeddingBackfill);
    let button = if running {
        let controller = controller.clone();
        Button::new("embedding-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.ai_cancel").to_string())
            .on_click(move |_, _, cx| {
                crate::library::jobs::cancel_embedding_backfill_app(&controller, cx);
            })
    } else {
        let controller = controller.clone();
        Button::new("embedding-generate")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.ai_generate").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_embedding_backfill_app(&controller, window, cx);
            })
    };
    h_flex().w_full().justify_end().child(button)
}

/// Delete every vector of the configured model. Disabled when there is
/// nothing stored (or a maintenance job holds the store) — a button that
/// cannot do anything should not look like it can.
fn ai_delete_row(controller: Entity<LibraryController>, embedded: u64, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().w_full().justify_end().child(
        Button::new("ai-delete-vectors")
            .outline()
            .small()
            .disabled(busy || embedded == 0)
            .label(rust_i18n::t!("settings.ai_delete").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::delete_embeddings_app(&controller, window, cx);
            }),
    )
}

// ============================ tagging (chat endpoint) ========================

/// The saved chat config, or defaults when the tagger has never been
/// configured.
fn chat_config() -> trove_core::config::ChatConfig {
    AppConfig::load().ai_chat.unwrap_or_default()
}

/// Persist one field change to the chat config (`config.json`, like every
/// other setting).
fn save_chat_config(edit: impl FnOnce(&mut trove_core::config::ChatConfig), cx: &mut App) {
    let mut config = AppConfig::load();
    edit(config.ai_chat.get_or_insert_with(Default::default));
    let _ = config.save();
    cx.refresh_windows();
}

/// The model the automatic tagger asks. Same shape as the embedding endpoint
/// above and configured apart from it: the two are different models on the
/// same server as often as not.
fn chat_group(controller: &Entity<LibraryController>, probe: &ChatProbe) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.chat_endpoint").to_string())
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_base_url").to_string(),
            SettingField::input(
                |_cx| SharedString::from(chat_config().base_url.clone()),
                |value, cx| save_chat_config(|config| config.base_url = value.to_string(), cx),
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_api_key").to_string(),
            SettingField::input(
                |_cx| SharedString::from(chat_config().api_key.clone()),
                |value, cx| save_chat_config(|config| config.api_key = value.to_string(), cx),
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.chat_model").to_string(),
            SettingField::input(
                |_cx| SharedString::from(chat_config().model.clone()),
                |value, cx| save_chat_config(|config| config.model = value.to_string(), cx),
            ),
        ))
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_send_images").to_string(),
                SettingField::switch(
                    |_cx| chat_config().send_images,
                    |value, cx| save_chat_config(|config| config.send_images = value, cx),
                ),
            )
            .description(rust_i18n::t!("settings.chat_send_images_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_new_tags").to_string(),
                SettingField::number_input(
                    NumberFieldOptions {
                        min: 0.0,
                        max: 10.0,
                        step: 1.0,
                    },
                    |_cx| chat_config().max_new_tags as f64,
                    |value, cx| {
                        let value = value.clamp(0.0, 10.0) as u32;
                        save_chat_config(|config| config.max_new_tags = value, cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.chat_new_tags_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_parent_tag").to_string(),
                SettingField::input(
                    |_cx| SharedString::from(chat_config().new_tag_parent.clone()),
                    |value, cx| {
                        save_chat_config(|config| config.new_tag_parent = value.to_string(), cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.chat_parent_tag_desc").to_string()),
        )
        .item(SettingItem::new(
            rust_i18n::t!("settings.chat_language").to_string(),
            SettingField::input(
                |_cx| SharedString::from(chat_config().tag_language.clone().unwrap_or_default()),
                |value, cx| {
                    let value = value.trim().to_string();
                    save_chat_config(
                        move |config| config.tag_language = (!value.is_empty()).then_some(value),
                        cx,
                    )
                },
            ),
        ))
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_probe").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    let probe = probe.clone();
                    move |_, _, cx| chat_probe_row(&controller, &probe, cx)
                }),
            )
            .description(rust_i18n::t!("settings.chat_probe_desc").to_string()),
        )
}

/// The connection-test row for the chat endpoint: the model's own words on
/// the left, the button on the right.
///
/// Showing what the model *said* is the point — a green tick only proves
/// something answered, while a sentence proves a model did.
fn chat_probe_row(controller: &Entity<LibraryController>, probe: &ChatProbe, cx: &mut App) -> Div {
    let (text, color) = match probe {
        ChatProbe::Idle => (
            rust_i18n::t!("settings.ai_probe_idle").to_string(),
            cx.theme().muted_foreground,
        ),
        ChatProbe::Running => (
            rust_i18n::t!("settings.ai_probe_running").to_string(),
            cx.theme().muted_foreground,
        ),
        ChatProbe::Ok { reply } => (
            rust_i18n::t!("settings.chat_probe_ok", reply = reply.as_str()).to_string(),
            cx.theme().success,
        ),
        ChatProbe::Failed { message } => (
            rust_i18n::t!("settings.ai_probe_failed", error = message.as_str()).to_string(),
            cx.theme().danger,
        ),
    };
    let running = probe.is_running();
    let controller = controller.clone();
    h_flex()
        .w_full()
        .items_center()
        .justify_end()
        .gap_2()
        .child(div().text_sm().text_color(color).child(text))
        .child(
            Button::new("chat-probe")
                .outline()
                .small()
                .disabled(running)
                .label(rust_i18n::t!("settings.ai_probe_run").to_string())
                .on_click(move |_, _, cx| {
                    crate::library::jobs::test_chat_endpoint_app(&controller, cx);
                }),
        )
}

/// The tagging run: what it does, and the buttons that control it.
fn tagging_group(controller: &Entity<LibraryController>) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.autotag").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.autotag_scope").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| autotag_buttons(&controller, cx)
                }),
            )
            .description(rust_i18n::t!("settings.autotag_scope_desc").to_string()),
        )
}

/// Run / cancel, plus the undo that takes a whole run back.
///
/// The buttons are rebuilt here rather than captured because the row is
/// rendered once per paint: the run button has to read `is_running` at that
/// moment, and a captured element would freeze the state it was built with.
fn autotag_buttons(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let running = controller
        .read(cx)
        .library
        .tasks()
        .is_running(trove_core::tasks::TaskKind::AutoTag);

    let run = if running {
        let controller = controller.clone();
        Button::new("autotag-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.ai_cancel").to_string())
            .on_click(move |_, _, cx| {
                crate::library::jobs::cancel_auto_tag_app(&controller, cx);
            })
    } else {
        let controller = controller.clone();
        Button::new("autotag-run")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.autotag_run").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_auto_tag_app(
                    &controller,
                    crate::library::jobs::AutoTagTarget::WholeLibrary,
                    window,
                    cx,
                );
            })
    };

    let undo = {
        let controller = controller.clone();
        Button::new("autotag-undo")
            .outline()
            .small()
            .disabled(running)
            .label(rust_i18n::t!("settings.autotag_undo").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_auto_tag_undo_app(&controller, window, cx);
            })
    };

    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .child(run)
        .child(undo)
}
