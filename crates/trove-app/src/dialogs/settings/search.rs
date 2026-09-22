//! Search page: the full-text index and the visual fingerprints (pHash +
//! colour histogram) that power "search by image" and "search by colour".
//!
//! Everything here is model-free and derived from the assets themselves —
//! which is why the AI embedding endpoint, a server-backed vector store with
//! its own credentials and its own failure modes, has a page of its own (see
//! `ai`).

use super::files::{finish_job, start_job};
use super::*;

// ============================ tiers =========================================

/// The stored search configuration.
fn search_config() -> trove_core::config::SearchConfig {
    AppConfig::load().search
}

/// Persist a search-tier change and re-resolve the controller's tiers, so the
/// next data pass sees the new switches without a restart.
fn save_search(
    controller: &Entity<LibraryController>,
    edit: impl FnOnce(&mut trove_core::config::SearchConfig),
    cx: &mut App,
) {
    let mut config = AppConfig::load();
    edit(&mut config.search);
    let _ = config.save();
    controller.update(cx, |ctl, cx| {
        ctl.refresh_search_tiers();
        cx.notify();
    });
    cx.refresh_windows();
}

/// The three search legs and their toggles.
///
/// The full-text index is local and nearly free; the other two call a cloud
/// endpoint and cost money per use, so each has its own switch. A leg that is
/// on but unconfigured is inert — the search falls back to the layers below it
/// rather than failing.
fn tiers_group(controller: &Entity<LibraryController>) -> SettingGroup {
    // Each setter owns a clone of the controller handle; the settings
    // framework builds the row once and calls the closure later.
    let full_text = controller.clone();
    let semantic = controller.clone();
    let ai = controller.clone();
    let ai_vendor = controller.clone();
    let ai_url = controller.clone();
    let ai_key = controller.clone();
    let ai_model = controller.clone();

    SettingGroup::new()
        .title(rust_i18n::t!("settings.search_tiers").to_string())
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.search_full_text").to_string(),
                SettingField::switch(
                    |_cx| search_config().full_text,
                    move |value, cx| save_search(&full_text, |config| config.full_text = value, cx),
                ),
            )
            .description(rust_i18n::t!("settings.search_full_text_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.search_semantic").to_string(),
                SettingField::switch(
                    |_cx| search_config().semantic_enabled,
                    move |value, cx| {
                        save_search(&semantic, |config| config.semantic_enabled = value, cx)
                    },
                ),
            )
            .description(rust_i18n::t!("settings.search_semantic_desc").to_string()),
        )
        .item(
            SettingItem::new(
                rust_i18n::t!("settings.search_ai").to_string(),
                SettingField::switch(
                    |_cx| search_config().ai.enabled,
                    move |value, cx| save_search(&ai, |config| config.ai.enabled = value, cx),
                ),
            )
            .description(rust_i18n::t!("settings.search_ai_desc").to_string()),
        )
        .item(SettingItem::new(
            rust_i18n::t!("settings.search_ai_vendor").to_string(),
            SettingField::dropdown(
                vendor_options(),
                |_cx| SharedString::from(search_config().ai.vendor.clone()),
                move |value, cx| {
                    save_search(
                        &ai_vendor,
                        |config| {
                            apply_vendor_choice(
                                &mut config.ai.vendor,
                                &mut config.ai.base_url,
                                &value,
                            )
                        },
                        cx,
                    )
                },
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_base_url").to_string(),
            SettingField::input(
                |_cx| SharedString::from(search_config().ai.base_url.clone()),
                move |value, cx| {
                    save_search(&ai_url, |config| config.ai.base_url = value.to_string(), cx)
                },
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.ai_api_key").to_string(),
            SettingField::input(
                |_cx| SharedString::from(search_config().ai.api_key.clone()),
                move |value, cx| {
                    save_search(&ai_key, |config| config.ai.api_key = value.to_string(), cx)
                },
            ),
        ))
        .item(SettingItem::new(
            rust_i18n::t!("settings.search_ai_model").to_string(),
            SettingField::input(
                |_cx| SharedString::from(search_config().ai.model.clone()),
                move |value, cx| {
                    save_search(&ai_model, |config| config.ai.model = value.to_string(), cx)
                },
            ),
        ))
}

// ============================ search page ===================================

/// Search ▸ the full-text index, and the per-image fingerprints that power
/// "search by image" and "search by colour". The coverage number is the
/// [`StatsSnapshot`] value, not a live query.
pub(super) fn search_page(
    controller: &Entity<LibraryController>,
    sig_coverage: (u64, u64),
) -> SettingPage {
    SettingPage::new(rust_i18n::t!("settings.search").to_string())
        .icon(IconName::Search)
        .resettable(false)
        .group(tiers_group(controller))
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.search_index").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.rebuild_index").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| index_row(&controller, cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.rebuild_index_desc").to_string()),
                ),
        )
        .group(
            SettingGroup::new()
                .title(rust_i18n::t!("settings.search_fingerprints").to_string())
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.sig_coverage").to_string(),
                        SettingField::render(move |_, _, cx| sig_coverage_row(sig_coverage, cx)),
                    )
                    .description(rust_i18n::t!("settings.sig_coverage_desc").to_string()),
                )
                .item(
                    SettingItem::new(
                        rust_i18n::t!("settings.backfill_sigs").to_string(),
                        SettingField::render({
                            let controller = controller.clone();
                            move |_, _, cx| backfill_row(controller.clone(), cx)
                        }),
                    )
                    .description(rust_i18n::t!("settings.backfill_sigs_desc").to_string()),
                ),
        )
}

/// Fingerprint coverage row: the snapshot values (see [`StatsSnapshot`]).
fn sig_coverage_row((signed, total): (u64, u64), cx: &mut App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(
            rust_i18n::t!(
                "settings.sig_coverage_value",
                signed = signed,
                total = total
            )
            .to_string(),
        )
}

/// Backfill button: computes pHash + colour signatures for live images that
/// predate fingerprints. The Store is thread-confined (`Rc<RefCell>`), so
/// this runs one asset per main-thread turn with a short yield between —
/// the UI repaints continuously instead of freezing for the whole batch.
fn backfill_row(controller: Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().w_full().justify_end().child(
        Button::new("backfill-signatures")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.backfill_sigs").to_string())
            .on_click(move |_, _, cx| {
                let ctl = controller.clone();
                ctl.update(cx, |ctl, cx| {
                    ctl.busy = true;
                    ctl.notice = Some(rust_i18n::t!("settings.backfill_running").to_string());
                    cx.notify();
                });
                cx.spawn(async move |cx| {
                    // Plan on the main thread (Library is not Send).
                    let (root, missing): (std::path::PathBuf, Vec<uuid::Uuid>) =
                        ctl.update(cx, |ctl, _| {
                            let root = ctl.library.root().to_path_buf();
                            let missing =
                                trove_core::store::visual_search::assets_needing_signature(
                                    ctl.library.store(),
                                )
                                .unwrap_or_default();
                            (root, missing)
                        });
                    let mut done = 0u64;
                    for id in &missing {
                        let updated = ctl.update(cx, |ctl, _| {
                            trove_core::store::visual_search::compute_and_store_signature(
                                ctl.library.store(),
                                &root,
                                *id,
                            )
                            .map(u64::from)
                            .unwrap_or(0)
                        });
                        done += updated;
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(20))
                            .await;
                    }
                    ctl.update(cx, |ctl, cx| {
                        ctl.busy = false;
                        ctl.notice = Some(
                            rust_i18n::t!(
                                "settings.backfill_done",
                                done = done,
                                total = missing.len()
                            )
                            .to_string(),
                        );
                        cx.notify();
                    });
                })
                .detach();
            }),
    )
}

/// Search-index row: a synchronous rebuild (database-bound, quick).
fn index_row(controller: &Entity<LibraryController>, cx: &mut App) -> Div {
    let busy = controller.read(cx).busy;
    h_flex().flex_1().justify_end().child(
        Button::new("rebuild-index")
            .outline()
            .small()
            .disabled(busy)
            .label(rust_i18n::t!("settings.rebuild_index").to_string())
            .on_click({
                let controller = controller.clone();
                move |_, _, cx| {
                    if !start_job(&controller, cx) {
                        return;
                    }
                    let result = {
                        let library = &controller.read(cx).library;
                        trove_core::services::maintenance::rebuild_search_index(library)
                    };
                    let message = match result {
                        Ok(count) => {
                            rust_i18n::t!("settings.rebuild_index_done", count = count).to_string()
                        }
                        Err(e) => {
                            rust_i18n::t!("settings.job_failed", error = e.to_string()).to_string()
                        }
                    };
                    finish_job(&controller, message, cx);
                }
            }),
    )
}
