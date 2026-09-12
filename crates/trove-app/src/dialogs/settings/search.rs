//! Search page: visual-fingerprint coverage and the backfill action
//! for images imported before fingerprints existed.

use super::*;

// ============================ search page ===================================

/// Search ▸ per-image visual fingerprints (pHash + colour histogram) that
/// power "search by image" and "search by colour", plus a backfill button
/// for libraries imported before fingerprints were computed. The coverage
/// number is the [`StatsSnapshot`] value, not a live query.
pub(super) fn search_page(
    controller: &Entity<LibraryController>,
    sig_coverage: (u64, u64),
) -> SettingPage {
    SettingPage::new(rust_i18n::t!("settings.search").to_string())
        .icon(IconName::Search)
        .resettable(false)
        .group(
            SettingGroup::new()
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
