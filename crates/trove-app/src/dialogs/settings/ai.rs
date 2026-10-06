//! AI page: the vendor profiles the AI features share, then each feature's
//! own slice — which profile to talk to and which model to ask, with the
//! probe line that proves the pairing works.
//!
//! This started as one group on the Search page and moved out once it grew a
//! second operation: an endpoint is its own subsystem with its own failure
//! modes (an unreachable server, a key the server rejects, a model name it
//! does not know, rows left over from a differently-configured provider),
//! while that page is about the two model-free indexes. The endpoints
//! themselves live in the vendor registry at the top of the page — one
//! server, one entry — so a feature's own group only has to answer "which
//! vendor, which model".

use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::setting::NumberFieldOptions;
use gpui_kit::component::switch::Switch;

use super::*;
use crate::app::settings_write;
use crate::components::controls;
use crate::library::{AiProbe, AnalysisProbe, ModelDownload, TranscriptionProbe};

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
    settings_write::note(config.save(), "AI settings");
    cx.refresh_windows();
    // An engine or model pick here is what search will talk to next; the
    // warm-up keeps the provider matching the pick already on the device,
    // and is a microsecond cache hit when the pick changed nothing.
    crate::library::jobs::warm_local_embedder_app(cx);
}

// ============================ page ==========================================

/// The AI page: the vendor registry, then each feature's slice of it —
/// which profile to talk to, which model to ask, and the probe that proves
/// the pairing works.
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
            .embedding_coverage(&config.model_id())
            .ok()
    } else {
        None
    };
    let probe = controller.read(cx).ai_probe.clone();
    let analysis_probe = controller.read(cx).analysis_probe.clone();
    let transcription_probe = controller.read(cx).transcription_probe.clone();

    SettingPage::new(rust_i18n::t!("settings.ai").to_string())
        .icon(IconName::Bot)
        .description(rust_i18n::t!("settings.ai_desc").to_string())
        .resettable(false)
        .group(vendors_group())
        .group(endpoint_group(controller, &probe))
        .group(vector_group(controller, coverage))
        .group(analysis_group(controller, &analysis_probe))
        .group(analysis_run_group(controller))
        .group(transcription_group(controller, &transcription_probe))
        .group(transcription_run_group(controller))
}

// ============================ vendor registry ================================

/// The saved AI servers — one entry per server, shared by every feature —
/// plus the control that adds one. Editing a profile is editing every
/// feature that points at it, which is exactly the point: the base URL and
/// the key are typed once.
fn vendors_group() -> SettingGroup {
    let profiles = AppConfig::load().vendors;
    let mut group = SettingGroup::new().title(rust_i18n::t!("settings.vendors").to_string());
    for profile in &profiles {
        let profile = profile.clone();
        group = group.item(SettingItem::new(
            profile.name.clone(),
            SettingField::render(move |_, _, cx| vendor_row(&profile, cx)),
        ));
    }
    if profiles.is_empty() {
        group = group.item(SettingItem::new(
            rust_i18n::t!("settings.vendors_empty").to_string(),
            SettingField::render(|_, _, cx| {
                controls::empty_note(rust_i18n::t!("settings.vendors_empty_note").to_string(), cx)
            }),
        ));
    }
    group.item(
        SettingItem::new(
            rust_i18n::t!("settings.vendors_add").to_string(),
            SettingField::render(|_, _, cx| add_vendor_row(cx)),
        )
        .description(rust_i18n::t!("settings.vendors_desc").to_string()),
    )
}

/// One saved server: its name and host, a "local" chip when the service
/// runs on this machine, and the two acts a registry entry supports.
fn vendor_row(profile: &trove_core::config::VendorProfile, cx: &mut App) -> Div {
    let editing = profile.clone();
    let deleting = profile.clone();
    h_flex()
        .w_full()
        .items_center()
        .justify_end()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(profile.base_url.clone()),
        )
        .when(profile.local, |row| {
            row.child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(cx.theme().secondary)
                    .text_xs()
                    .text_color(cx.theme().secondary_foreground)
                    .child(rust_i18n::t!("settings.vendors_local").to_string()),
            )
        })
        .child(
            Button::new(SharedString::from(format!("vendor-edit-{}", profile.id)))
                .ghost()
                .xsmall()
                .icon(IconName::Pencil)
                .tooltip(rust_i18n::t!("settings.vendors_edit").to_string())
                .on_click(move |_, window, cx| open_vendor_editor(window, cx, editing.clone())),
        )
        .child(
            Button::new(SharedString::from(format!("vendor-delete-{}", profile.id)))
                .ghost()
                .xsmall()
                .icon(IconName::Trash)
                .tooltip(rust_i18n::t!("settings.vendors_delete").to_string())
                .on_click(move |_, window, cx| {
                    open_vendor_delete_confirm(window, cx, deleting.clone())
                }),
        )
}

/// The add-vendor control: one row per well-known service, prefilled with
/// its endpoint (a local one for Ollama and LM Studio), "Custom" opening the
/// editor blank.
fn add_vendor_row(_cx: &mut App) -> AnyElement {
    let presets: Vec<(usize, String)> = trove_core::config::VENDOR_PRESETS
        .iter()
        .enumerate()
        .map(|(index, (name, _, local))| {
            let label = match *local {
                true => format!("{name} ({})", rust_i18n::t!("settings.vendors_local")),
                false => name.to_string(),
            };
            (index, label)
        })
        .collect();
    Button::new("add-vendor")
        .outline()
        .small()
        .label(rust_i18n::t!("settings.vendors_add").to_string())
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
            let mut menu = menu.min_w(px(220.));
            for (index, label) in presets.clone() {
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    open_vendor_preset_editor(window, cx, index);
                }));
            }
            menu
        })
        .into_any_element()
}

/// A vendor profile mid-edit: the entry's id (empty when adding), the three
/// text fields as input states, and the local flag as a plain bool the
/// switch writes back into.
struct VendorEditor {
    id: String,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    api_key: Entity<InputState>,
    local: bool,
}

/// Open the add-or-edit dialog for one vendor profile. An empty `id` means a
/// new entry (the add path, prefilled by the preset that opened the editor);
/// otherwise the saved entry is replaced in place, and every feature
/// pointing at it picks up the change by id.
fn open_vendor_editor(
    window: &mut Window,
    cx: &mut App,
    profile: trove_core::config::VendorProfile,
) {
    let is_new = profile.id.is_empty();
    let editor = cx.new(|cx| {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(profile.name.clone()));
        let base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(profile.base_url.clone())
                .placeholder("http://localhost:11434/v1")
        });
        let api_key =
            cx.new(|cx| InputState::new(window, cx).default_value(profile.api_key.clone()));
        VendorEditor {
            id: profile.id.clone(),
            name,
            base_url,
            api_key,
            local: profile.local,
        }
    });
    let title = match is_new {
        true => rust_i18n::t!("settings.vendors_add").to_string(),
        false => {
            rust_i18n::t!("settings.vendors_edit_title", name = profile.name.clone()).to_string()
        }
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let editor = editor.clone();
        let local = editor.read(cx).local;
        let (name, base_url, api_key) = {
            let editor = editor.read(cx);
            (
                editor.name.clone(),
                editor.base_url.clone(),
                editor.api_key.clone(),
            )
        };
        let switch_editor = editor.clone();
        dialog
            .title(title.clone())
            .width(px(460.))
            .child(
                v_flex()
                    .gap_3()
                    .child(form_label(
                        rust_i18n::t!("settings.vendors_name").to_string(),
                    ))
                    .child(Input::new(&name).small().appearance(true))
                    .child(form_label(
                        rust_i18n::t!("settings.ai_base_url").to_string(),
                    ))
                    .child(Input::new(&base_url).small().appearance(true))
                    .child(form_label(rust_i18n::t!("settings.ai_api_key").to_string()))
                    .child(Input::new(&api_key).small().appearance(true))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(form_label(
                                rust_i18n::t!("settings.vendors_local").to_string(),
                            ))
                            .child(Switch::new("vendor-editor-local").checked(local).on_click(
                                move |checked: &bool, _, cx| {
                                    switch_editor.update(cx, |editor, _| editor.local = *checked);
                                },
                            )),
                    ),
            )
            .on_ok(move |_, window, cx| save_vendor_editor(&editor, window, cx))
    });
}

/// A small form label above a dialog field.
fn form_label(text: String) -> Div {
    div().text_sm().child(text)
}

/// The add path: a preset chosen from the dropdown opens the editor with its
/// endpoint and local flag already in place, so Ollama is one confirmation
/// away from being a registry entry. "Custom" has no endpoint and opens
/// blank.
pub(super) fn open_vendor_preset_editor(window: &mut Window, cx: &mut App, preset: usize) {
    let Some((name, base_url, local)) = trove_core::config::VENDOR_PRESETS.get(preset) else {
        return;
    };
    open_vendor_editor(
        window,
        cx,
        trove_core::config::VendorProfile {
            id: String::new(),
            name: name.to_string(),
            base_url: base_url.to_string(),
            api_key: String::new(),
            local: *local,
        },
    );
}

/// Validate and persist one edited profile. `false` keeps the dialog open —
/// the toast says what is missing.
fn save_vendor_editor(editor: &Entity<VendorEditor>, window: &mut Window, cx: &mut App) -> bool {
    let (name, base_url, api_key) = {
        let editor = editor.read(cx);
        (
            editor.name.read(cx).value().trim().to_string(),
            editor.base_url.read(cx).value().trim().to_string(),
            editor.api_key.read(cx).value().trim().to_string(),
        )
    };
    let base_url = base_url.trim_end_matches('/').to_string();
    // A URL that is not a URL would only surface as per-request failures
    // later; refuse it here, where fixing it costs nothing.
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        window.push_notification(
            Notification::warning(
                rust_i18n::t!("settings.vendors_bad_url", url = base_url.clone()).to_string(),
            ),
            cx,
        );
        return false;
    }
    // A nameless profile is named after its host — the same fold the
    // migration does for an endpoint it already knows.
    let name = if name.is_empty() {
        let host = super::host_of(&base_url).to_string();
        if host.is_empty() {
            "Custom".into()
        } else {
            host
        }
    } else {
        name
    };

    let id = editor.read(cx).id.clone();
    let mut config = AppConfig::load();
    match config.vendors.iter_mut().find(|vendor| vendor.id == id) {
        // Editing in place: the id survives, so every feature pointing at
        // this profile picks up the new endpoint without a re-pick.
        Some(entry) => {
            entry.name = name;
            entry.base_url = base_url.clone();
            entry.api_key = api_key;
            entry.local = editor.read(cx).local;
        }
        None => config.vendors.push(trove_core::config::VendorProfile {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name,
            base_url,
            api_key,
            local: editor.read(cx).local,
        }),
    }
    settings_write::note(config.save(), "vendor profile");
    cx.refresh_windows();
    true
}

/// Deleting a registry entry silently re-points nothing: every feature that
/// referenced it falls back to the first remaining profile, and a registry
/// reduced to zero leaves every cloud feature unconfigured. Name that in
/// the gate before the delete runs.
fn open_vendor_delete_confirm(
    window: &mut Window,
    cx: &mut App,
    profile: trove_core::config::VendorProfile,
) {
    let id = profile.id.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let profile = profile.clone();
        let id = id.clone();
        alert
            .title(rust_i18n::t!("settings.vendors_delete_title").to_string())
            .description(
                rust_i18n::t!("settings.vendors_delete_body", name = profile.name.clone())
                    .to_string(),
            )
            .confirm()
            .ok_text(rust_i18n::t!("settings.vendors_delete").to_string())
            .on_ok(move |_, _, cx| {
                let mut config = AppConfig::load();
                config.vendors.retain(|vendor| vendor.id != id);
                settings_write::note(config.save(), "vendor profile");
                cx.refresh_windows();
                true
            })
    })
}

// ============================ embedding ======================================

// ============================ endpoint ======================================

/// Base URL / API key / model, plus the connection test. The three inputs
/// follow the shape every OpenAI-compatible server speaks (OpenAI itself,
/// Ollama, LM Studio, vLLM, proxies), so the same three fields cover cloud
/// and local setups alike.
fn endpoint_group(controller: &Entity<LibraryController>, probe: &AiProbe) -> SettingGroup {
    let engine = embedding_config().engine;
    let group = SettingGroup::new()
        .title(rust_i18n::t!("settings.ai_endpoint").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.embed_engine").to_string(),
                engine_field(),
            )
            .description(rust_i18n::t!("settings.embed_engine_desc").to_string()),
        );
    // The endpoint fields name a server, which means nothing to the local
    // engine; they only show when a server is what the runs talk to.
    let group = if engine == trove_core::config::EmbeddingEngine::Cloud {
        group
            .item(
                SettingItem::new(
                    rust_i18n::t!("settings.feature_vendor").to_string(),
                    embedding_vendor_field(),
                )
                .description(rust_i18n::t!("settings.feature_vendor_desc").to_string()),
            )
            .item(SettingItem::new(
                rust_i18n::t!("settings.ai_model").to_string(),
                embedding_model_field(),
            ))
            .item(
                SettingItem::new(
                    rust_i18n::t!("settings.embed_multimodal").to_string(),
                    SettingField::switch(
                        |_cx| embedding_config().multimodal,
                        |value, cx| save_embedding_config(|c| c.multimodal = value, cx),
                    ),
                )
                .description(rust_i18n::t!("settings.embed_multimodal_desc").to_string()),
            )
    } else {
        group
            .item(
                SettingItem::new(
                    rust_i18n::t!("settings.embed_model_pick").to_string(),
                    embed_model_pick_field(),
                )
                .description(rust_i18n::t!("settings.embed_model_pick_desc").to_string()),
            )
            .item(
                SettingItem::new(
                    rust_i18n::t!("settings.embed_model_item").to_string(),
                    SettingField::render({
                        let controller = controller.clone();
                        move |_, _, cx| embed_models_block(&controller, cx)
                    }),
                )
                .description(rust_i18n::t!("settings.embed_model_desc").to_string()),
            )
    };
    group.item(
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

/// The cloud/local switch the two model-bearing engines share (this one and
/// transcription's, below): the local engine reads no server, so the
/// endpoint fields fold away under it.
fn engine_field() -> SettingField<SharedString> {
    SettingField::dropdown(
        vec![
            (
                SharedString::from("cloud"),
                SharedString::from(rust_i18n::t!("settings.embed_engine_cloud").to_string()),
            ),
            (
                SharedString::from("local"),
                SharedString::from(rust_i18n::t!("settings.embed_engine_local").to_string()),
            ),
        ],
        move |_cx| {
            SharedString::from(match embedding_config().engine {
                trove_core::config::EmbeddingEngine::Cloud => "cloud",
                trove_core::config::EmbeddingEngine::Local => "local",
            })
        },
        |value, cx| {
            save_embedding_config(
                |config| {
                    config.engine = if value == "local" {
                        trove_core::config::EmbeddingEngine::Local
                    } else {
                        trove_core::config::EmbeddingEngine::Cloud
                    }
                },
                cx,
            )
        },
    )
}

fn embedding_vendor_field() -> SettingField<SharedString> {
    vendor_field(
        || embedding_config().vendor_id.clone(),
        |value, cx| save_embedding_config(|config| config.vendor_id = Some(value), cx),
    )
}

/// The embedding model row: presets for the referenced profile's host, free
/// text for everything else.
fn embedding_model_field() -> SettingField<SharedString> {
    let presets_url = AppConfig::load()
        .vendor(embedding_config().vendor_id.as_deref())
        .map(|profile| profile.base_url.clone())
        .unwrap_or_default();
    model_field(
        "embedding-model",
        presets_url,
        super::ModelPresets::Embedding,
        || embedding_config().model,
        |value, cx| save_embedding_config(|config| config.model = value, cx),
    )
}

/// Which catalog model the local engine runs: one row per BGE checkpoint,
/// the size annotating what a pick costs to fetch. The pick only names the
/// target — the row below downloads it and shows whether it is present.
fn embed_model_pick_field() -> SettingField<SharedString> {
    let options: Vec<(SharedString, SharedString)> = trove_core::services::embed_model::MODELS
        .iter()
        .map(|model| {
            (
                SharedString::from(model.id),
                SharedString::from(format!("{} · {} MB", model.id, model.download_mb)),
            )
        })
        .collect();
    let current = SharedString::from(embedding_config().local_model_id().to_string());
    SettingField::dropdown(
        options,
        move |_| current.clone(),
        |value, cx| {
            save_embedding_config(|config| config.local_model = Some(value.to_string()), cx)
        },
    )
}

/// The selected local embedding model — the one the picker above names, and
/// the only model this row talks about: what it is, whether it is on disk,
/// and the one control that matters for its state (download or retry while
/// it is missing, delete once it is on disk). Another model enters the row
/// by being picked, not by being listed.
fn embed_models_block(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    use trove_core::services::embed_model as em;

    let model = em::resolve(embedding_config().local_model_id());
    let embed_download = controller.read(cx).embed_model_download.clone();
    let download_for = controller.read(cx).embed_model_download_for.clone();
    let any_running = embed_download
        .as_ref()
        .is_some_and(ModelDownload::is_running);
    // The download slot is page-global, but its state belongs to the model it
    // was started for: a row left behind by a re-pick must not wear another
    // model's progress or failure.
    let download = (download_for.as_deref() == Some(model.id))
        .then_some(embed_download)
        .flatten();
    let ready = matches!(em::status(model.id), em::ModelStatus::Ready { .. });

    let (status_text, status_color) = match &download {
        Some(ModelDownload::Running { received, total }) => {
            let text = if *total > 0 {
                rust_i18n::t!(
                    "settings.local_model_progress",
                    received = received / 1_048_576,
                    total = total / 1_048_576
                )
                .to_string()
            } else {
                rust_i18n::t!("settings.local_model_downloading").to_string()
            };
            (text, cx.theme().muted_foreground)
        }
        Some(ModelDownload::Failed { message }) => (
            rust_i18n::t!("settings.local_model_failed", error = message.as_str()).to_string(),
            cx.theme().danger,
        ),
        None if ready => (
            rust_i18n::t!("settings.model_downloaded").to_string(),
            cx.theme().success,
        ),
        None => (
            rust_i18n::t!("settings.local_model_missing").to_string(),
            cx.theme().muted_foreground,
        ),
    };

    let mut row = h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(format!("{} · {} MB", model.id, model.download_mb)),
                )
                // The status can be an entire failure sentence (a URL plus a
                // reason). It takes the free space and wraps; the button keeps
                // its own width and stays reachable instead of being pushed
                // out of the row by the text.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(status_color)
                        .child(status_text),
                ),
        );

    // Not on disk: start (or retry) its download. While any embedding-model
    // download is in flight the button steps aside — the slot is single, and
    // its progress is the downloading model's own status text.
    if !ready && !any_running {
        let label = if matches!(download, Some(ModelDownload::Failed { .. })) {
            rust_i18n::t!("settings.local_model_retry").to_string()
        } else {
            rust_i18n::t!("settings.local_model_download").to_string()
        };
        let controller = controller.clone();
        row = row.child(
            Button::new("embed-model-download")
                .outline()
                .small()
                .flex_none()
                .label(label)
                .on_click(move |_, window, cx| {
                    crate::library::jobs::start_embed_model_download_app(&controller, window, cx);
                }),
        );
    }
    // On disk: free it. The vectors it built stay — clearing those is the
    // Vectors group's own delete button below.
    if ready {
        let controller = controller.clone();
        let id = model.id.to_string();
        row = row.child(
            Button::new("embed-model-delete")
                .ghost()
                .small()
                .flex_none()
                .icon(IconName::Trash)
                .tooltip(rust_i18n::t!("settings.model_delete").to_string())
                .on_click(move |_, window, cx| {
                    crate::library::jobs::delete_embed_model_id_app(
                        &controller,
                        id.clone(),
                        window,
                        cx,
                    );
                }),
        );
    }

    row
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
                .on_click(move |_, window, cx| {
                    crate::library::jobs::test_embedding_endpoint_app(&controller, window, cx);
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
        .is_active(&trove_core::tasks::TaskKind::EmbeddingBackfill);
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

/// The saved analysis config, or defaults when the analysis has never been
/// configured.
fn analysis_config() -> trove_core::config::AiAnalysisConfig {
    AppConfig::load().ai_analysis.unwrap_or_default()
}

/// Persist one field change to the analysis config (`config.json`, like every
/// other setting).
fn save_analysis_config(
    edit: impl FnOnce(&mut trove_core::config::AiAnalysisConfig),
    cx: &mut App,
) {
    let mut config = AppConfig::load();
    edit(config.ai_analysis.get_or_insert_with(Default::default));
    settings_write::note(config.save(), "AI settings");
    cx.refresh_windows();
}

/// The model that reads each asset. Vendor, endpoint and model are stored
/// together; the embedding endpoint above is configured apart from it, since
/// the two are different models on the same host as often as not.
fn analysis_group(controller: &Entity<LibraryController>, probe: &AnalysisProbe) -> SettingGroup {
    // The referenced profile's endpoint decides which presets the model
    // dropdown offers — a local Ollama gets its vision models, a known
    // brand gets its table.
    let presets_url = AppConfig::load()
        .vendor(analysis_config().vendor_id.as_deref())
        .map(|profile| profile.base_url.clone())
        .unwrap_or_default();
    SettingGroup::new()
        .title(rust_i18n::t!("settings.chat_endpoint").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.feature_vendor").to_string(),
                vendor_field(
                    || analysis_config().vendor_id.clone(),
                    |value, cx| {
                        save_analysis_config(
                            |config| {
                                config.vendor_id = Some(value.clone());
                                // The stored family follows the profile's host,
                                // so a legacy Anthropic/Gemini config keeps its
                                // native adapter when its profile is re-picked
                                // and a new pick speaks the wire every profile
                                // can serve.
                                if let Some(profile) =
                                    AppConfig::load().vendors.iter().find(|v| v.id == value)
                                {
                                    config.vendor = family_for_base_url(&profile.base_url);
                                }
                            },
                            cx,
                        )
                    },
                ),
            )
            .description(rust_i18n::t!("settings.feature_vendor_desc").to_string()),
        )
        .item(SettingItem::new(
            rust_i18n::t!("settings.chat_model").to_string(),
            model_field(
                "analysis-model",
                presets_url,
                super::ModelPresets::Vision,
                || analysis_config().model,
                |value, cx| save_analysis_config(|config| config.model = value, cx),
            ),
        ))
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_send_images").to_string(),
                SettingField::switch(
                    |_cx| analysis_config().send_images,
                    |value, cx| save_analysis_config(|config| config.send_images = value, cx),
                ),
            )
            .description(rust_i18n::t!("settings.chat_send_images_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_describe").to_string(),
                SettingField::switch(
                    |_cx| analysis_config().fields.description,
                    |value, cx| {
                        save_analysis_config(|config| config.fields.description = value, cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.chat_describe_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_rate").to_string(),
                SettingField::switch(
                    |_cx| analysis_config().fields.rating,
                    |value, cx| save_analysis_config(|config| config.fields.rating = value, cx),
                ),
            )
            .description(rust_i18n::t!("settings.chat_rate_desc").to_string()),
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
                    |_cx| analysis_config().max_new_tags as f64,
                    |value, cx| {
                        let value = value.clamp(0.0, 10.0) as u32;
                        save_analysis_config(|config| config.max_new_tags = value, cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.chat_new_tags_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.chat_parent_tag").to_string(),
                SettingField::input(
                    |_cx| SharedString::from(analysis_config().new_tag_parent.clone()),
                    |value, cx| {
                        save_analysis_config(|config| config.new_tag_parent = value.to_string(), cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.chat_parent_tag_desc").to_string()),
        )
        .item(SettingItem::new(
            rust_i18n::t!("settings.chat_language").to_string(),
            SettingField::input(
                |_cx| {
                    SharedString::from(analysis_config().tag_language.clone().unwrap_or_default())
                },
                |value, cx| {
                    let value = value.trim().to_string();
                    save_analysis_config(
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
                    move |_, _, cx| analysis_probe_row(&controller, &probe, cx)
                }),
            )
            .description(rust_i18n::t!("settings.chat_probe_desc").to_string()),
        )
}

/// The connection-test row for the analysis endpoint: the model's own words on
/// the left, the button on the right.
///
/// Showing what the model *said* is the point — a green tick only proves
/// something answered, while a sentence proves a model did.
fn analysis_probe_row(
    controller: &Entity<LibraryController>,
    probe: &AnalysisProbe,
    cx: &mut App,
) -> Div {
    let (text, color) = match probe {
        AnalysisProbe::Idle => (
            rust_i18n::t!("settings.ai_probe_idle").to_string(),
            cx.theme().muted_foreground,
        ),
        AnalysisProbe::Running => (
            rust_i18n::t!("settings.ai_probe_running").to_string(),
            cx.theme().muted_foreground,
        ),
        AnalysisProbe::Ok { reply } => (
            rust_i18n::t!("settings.chat_probe_ok", reply = reply.as_str()).to_string(),
            cx.theme().success,
        ),
        AnalysisProbe::Failed { message } => (
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
            Button::new("analysis-probe")
                .outline()
                .small()
                .disabled(running)
                .label(rust_i18n::t!("settings.ai_probe_run").to_string())
                .on_click(move |_, _, cx| {
                    crate::library::jobs::test_analysis_endpoint_app(&controller, cx);
                }),
        )
}

/// The analysis run: what it does, and the buttons that control it.
fn analysis_run_group(controller: &Entity<LibraryController>) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.autotag").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.autotag_scope").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| analysis_buttons(&controller, cx)
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
fn analysis_buttons(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let running = controller
        .read(cx)
        .library
        .tasks()
        .is_active(&trove_core::tasks::TaskKind::AiAnalysis);

    let run = if running {
        let controller = controller.clone();
        Button::new("analysis-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.ai_cancel").to_string())
            .on_click(move |_, _, cx| {
                crate::library::jobs::cancel_analysis_app(&controller, cx);
            })
    } else {
        let controller = controller.clone();
        Button::new("analysis-run")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.autotag_run").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_analysis_app(
                    &controller,
                    crate::library::jobs::AnalysisTarget::WholeLibrary,
                    window,
                    cx,
                );
            })
    };

    let undo = {
        let controller = controller.clone();
        Button::new("analysis-undo")
            .outline()
            .small()
            .disabled(running)
            .label(rust_i18n::t!("settings.autotag_undo").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_analysis_undo_app(&controller, window, cx);
            })
    };

    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .child(run)
        .child(undo)
}

// ============================ transcription ==================================

/// The saved transcription config, or defaults when the feature has never
/// been configured.
fn transcription_config() -> trove_core::config::TranscriptionConfig {
    AppConfig::load().ai_transcription.unwrap_or_default()
}

/// Persist one field change to the transcription config (`config.json`, like
/// every other setting).
fn save_transcription_config(
    edit: impl FnOnce(&mut trove_core::config::TranscriptionConfig),
    cx: &mut App,
) {
    let mut config = AppConfig::load();
    edit(config.ai_transcription.get_or_insert_with(Default::default));
    settings_write::note(config.save(), "AI settings");
    cx.refresh_windows();
}

/// Model presets that speak the OpenAI transcription wire: OpenAI's own two
/// generations and the SenseVoice endpoint SiliconFlow hosts. The input
/// beside the dropdown takes everything else — Groq, self-hosted whisper
/// servers, relays.
const TRANSCRIBE_MODEL_PRESETS: &[&str] = &[
    "whisper-1",
    "gpt-4o-mini-transcribe",
    "gpt-4o-transcribe",
    "FunAudioLLM/SenseVoiceLarge",
];

/// The speech-to-text engine: which recogniser runs, and — for the cloud —
/// where the audio goes, what it costs, and the one way to find out any of
/// it was right — the probe row at the bottom, which uploads a second of
/// synthesized silence. For the local engine the row instead tracks the
/// model files: where they are, how the download is going, and the one
/// button that starts it.
fn transcription_group(
    controller: &Entity<LibraryController>,
    probe: &TranscriptionProbe,
) -> SettingGroup {
    let engine = transcription_config().engine;
    let group = SettingGroup::new()
        .title(rust_i18n::t!("settings.transcription_endpoint").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.transcription_engine").to_string(),
                SettingField::dropdown(
                    vec![
                        (
                            SharedString::from("cloud"),
                            rust_i18n::t!("settings.transcription_engine_cloud")
                                .to_string()
                                .into(),
                        ),
                        (
                            SharedString::from("local"),
                            rust_i18n::t!("settings.transcription_engine_local")
                                .to_string()
                                .into(),
                        ),
                    ],
                    |_cx| {
                        SharedString::from(match transcription_config().engine {
                            trove_core::config::TranscriptionEngine::Cloud => "cloud",
                            trove_core::config::TranscriptionEngine::Local => "local",
                        })
                    },
                    |value, cx| {
                        save_transcription_config(
                            |config| {
                                config.engine = if value == "local" {
                                    trove_core::config::TranscriptionEngine::Local
                                } else {
                                    trove_core::config::TranscriptionEngine::Cloud
                                }
                            },
                            cx,
                        )
                    },
                ),
            )
            .description(rust_i18n::t!("settings.transcription_engine_desc").to_string()),
        );
    // The endpoint fields name a server, which means nothing to the local
    // engine; they only show when a server is what the runs talk to.
    let group = if engine == trove_core::config::TranscriptionEngine::Cloud {
        group
            .item(
                SettingItem::new(
                    rust_i18n::t!("settings.feature_vendor").to_string(),
                    vendor_field(
                        || transcription_config().vendor_id.clone(),
                        |value, cx| {
                            save_transcription_config(|config| config.vendor_id = Some(value), cx)
                        },
                    ),
                )
                .description(rust_i18n::t!("settings.feature_vendor_desc").to_string()),
            )
            .item(SettingItem::new(
                rust_i18n::t!("settings.transcription_model").to_string(),
                SettingField::dropdown(
                    TRANSCRIBE_MODEL_PRESETS
                        .iter()
                        .map(|m| (SharedString::from(*m), SharedString::from(*m)))
                        .collect(),
                    |_cx| SharedString::from(transcription_config().model.clone()),
                    |value, cx| {
                        save_transcription_config(|config| config.model = value.to_string(), cx)
                    },
                ),
            ))
    } else {
        group
    };
    let group = group
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.transcription_language").to_string(),
                SettingField::input(
                    |_cx| {
                        SharedString::from(
                            transcription_config().language.clone().unwrap_or_default(),
                        )
                    },
                    |value, cx| {
                        let value = value.trim().to_string();
                        save_transcription_config(
                            move |config| config.language = (!value.is_empty()).then_some(value),
                            cx,
                        )
                    },
                ),
            )
            .description(rust_i18n::t!("settings.transcription_language_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.transcription_prompt").to_string(),
                SettingField::input(
                    |_cx| {
                        SharedString::from(
                            transcription_config().prompt.clone().unwrap_or_default(),
                        )
                    },
                    |value, cx| {
                        let value = value.trim().to_string();
                        save_transcription_config(
                            move |config| config.prompt = (!value.is_empty()).then_some(value),
                            cx,
                        )
                    },
                ),
            )
            .description(rust_i18n::t!("settings.transcription_prompt_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.ai_probe").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    let probe = probe.clone();
                    move |_, _, cx| transcription_probe_row(&controller, &probe, cx)
                }),
            )
            .description(rust_i18n::t!("settings.transcription_probe_desc").to_string()),
        );

    // The local model's own row: status, download progress, the button.
    if engine == trove_core::config::TranscriptionEngine::Local {
        group.item(
            SettingItem::new(
                rust_i18n::t!("settings.local_model_item").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| local_model_row(&controller, cx)
                }),
            )
            .description(rust_i18n::t!("settings.local_model_desc").to_string()),
        )
    } else {
        group
    }
}

/// The local model's row: where it is, how the download is going, and the
/// one button that starts (or retries) it.
fn local_model_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    use trove_core::services::local_model as lm;

    let (text, color, download_label) = match controller.read(cx).local_model_download.clone() {
        Some(ModelDownload::Running { received, total }) => {
            let text = if total > 0 {
                rust_i18n::t!(
                    "settings.local_model_progress",
                    received = received / 1_048_576,
                    total = total / 1_048_576
                )
                .to_string()
            } else {
                rust_i18n::t!("settings.local_model_downloading").to_string()
            };
            (text, cx.theme().muted_foreground, None)
        }
        Some(ModelDownload::Failed { message }) => (
            rust_i18n::t!("settings.local_model_failed", error = message.as_str()).to_string(),
            cx.theme().danger,
            Some(rust_i18n::t!("settings.local_model_retry").to_string()),
        ),
        None => match lm::status() {
            lm::ModelStatus::Ready { path } => (
                rust_i18n::t!(
                    "settings.local_model_ready",
                    path = path.display().to_string()
                )
                .to_string(),
                cx.theme().success,
                None,
            ),
            lm::ModelStatus::Missing => (
                rust_i18n::t!("settings.local_model_missing").to_string(),
                cx.theme().muted_foreground,
                Some(rust_i18n::t!("settings.local_model_download").to_string()),
            ),
        },
    };

    let mut row = h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        // The message can be an entire failure sentence (a URL plus a reason).
        // It takes the free space and wraps; the button keeps its own width and
        // stays reachable instead of being pushed out of the row by the text.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(color)
                .child(text),
        );
    if let Some(label) = download_label {
        let controller = controller.clone();
        row = row.child(
            Button::new("local-model-download")
                .outline()
                .small()
                .flex_none()
                .label(label)
                .on_click(move |_, window, cx| {
                    crate::library::jobs::start_model_download_app(&controller, None, window, cx);
                }),
        );
    }
    if matches!(lm::status(), lm::ModelStatus::Ready { .. }) {
        let controller = controller.clone();
        row = row.child(
            Button::new("local-model-delete")
                .ghost()
                .small()
                .flex_none()
                .icon(IconName::Trash)
                .tooltip(rust_i18n::t!("settings.model_delete").to_string())
                .on_click(move |_, window, cx| {
                    crate::library::jobs::delete_local_model_app(&controller, window, cx);
                }),
        );
    }
    row
}
/// The connection-test row for the transcription endpoint. The probe uploads
/// silence, so an empty reply is the *success* shape here — the row says so
/// rather than showing a blank line.
fn transcription_probe_row(
    controller: &Entity<LibraryController>,
    probe: &TranscriptionProbe,
    cx: &mut App,
) -> Div {
    let (text, color) = match probe {
        TranscriptionProbe::Idle => (
            rust_i18n::t!("settings.ai_probe_idle").to_string(),
            cx.theme().muted_foreground,
        ),
        TranscriptionProbe::Running => (
            rust_i18n::t!("settings.ai_probe_running").to_string(),
            cx.theme().muted_foreground,
        ),
        TranscriptionProbe::Ok { reply } if reply.is_empty() => (
            rust_i18n::t!("settings.transcribe_probe_silence").to_string(),
            cx.theme().success,
        ),
        TranscriptionProbe::Ok { reply } => (
            rust_i18n::t!("settings.transcribe_probe_ok", reply = reply.as_str()).to_string(),
            cx.theme().success,
        ),
        TranscriptionProbe::Failed { message } => (
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
            Button::new("transcription-probe")
                .outline()
                .small()
                .disabled(running)
                .label(rust_i18n::t!("settings.ai_probe_run").to_string())
                .on_click(move |_, window, cx| {
                    crate::library::jobs::test_transcription_endpoint_app(&controller, window, cx);
                }),
        )
}

/// The transcription run: a whole-library sweep with its cancel button.
fn transcription_run_group(controller: &Entity<LibraryController>) -> SettingGroup {
    SettingGroup::new()
        .title(rust_i18n::t!("settings.transcription").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.transcription_scope").to_string(),
                SettingField::render({
                    let controller = controller.clone();
                    move |_, _, cx| transcription_buttons(&controller, cx)
                }),
            )
            .description(rust_i18n::t!("settings.transcription_scope_desc").to_string()),
        )
}

/// Run / cancel for the whole-library transcription sweep. Rebuilt per paint
/// like the analysis buttons, for the same reason.
fn transcription_buttons(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let running = controller
        .read(cx)
        .library
        .tasks()
        .is_active(&trove_core::tasks::TaskKind::Transcription);

    let run = if running {
        let controller = controller.clone();
        Button::new("transcription-cancel")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.ai_cancel").to_string())
            .on_click(move |_, _, cx| {
                crate::library::jobs::cancel_transcription_app(&controller, cx);
            })
    } else {
        let controller = controller.clone();
        Button::new("transcription-run")
            .outline()
            .small()
            .label(rust_i18n::t!("settings.transcription_run").to_string())
            .on_click(move |_, window, cx| {
                crate::library::jobs::start_transcription_app(
                    &controller,
                    crate::library::jobs::TranscribeTarget::WholeLibrary,
                    window,
                    cx,
                );
            })
    };

    h_flex().w_full().justify_end().gap_2().child(run)
}
