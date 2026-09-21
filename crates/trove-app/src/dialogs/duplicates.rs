//! Duplicate finder: lists clusters of visually identical images (perceptual
//! hash distance ≤ 8) and offers per-group cleanup — keep the newest, trash
//! the rest. Content-exact duplicates cannot occur among live assets (the
//! importer deduplicates by content hash), so this catches re-encoded
//! variants.
//!
//! The scan runs once, on a backend thread (own database connection — the
//! O(n²) pHash pass must never run per render frame); the dialog renders the
//! cached result and invalidates it after a cleanup so the next frame
//! recomputes.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::store::assets::DuplicateGroup;
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::common::{display_name, human_bytes};

pub struct DuplicateDialog;

impl DuplicateDialog {
    pub fn open(window: &mut Window, cx: &mut App, controller: Entity<LibraryController>) {
        window.open_dialog(cx, move |dialog, _window, cx| {
            // Kick the backend scan off exactly once; the cached result (or
            // the "computing" note) renders until it lands.
            let needs_scan = {
                let ctl = controller.read(cx);
                ctl.duplicates.is_none() && !ctl.duplicates_computing
            };
            if needs_scan {
                let root = controller.read(cx).library.root().to_path_buf();
                controller.update(cx, |ctl, _| ctl.duplicates_computing = true);
                let controller = controller.clone();
                cx.spawn(async move |cx| {
                    let groups = cx
                        .background_executor()
                        .spawn(async move {
                            // Store::open creates or checks the schema.
                            let store =
                                trove_core::store::Store::open(&root.join("library.db")).ok()?;
                            trove_core::store::assets::duplicate_groups(store.conn()).ok()
                        })
                        .await
                        .unwrap_or_default();
                    controller.update(cx, |ctl, cx| {
                        ctl.duplicates = Some(std::sync::Arc::new(groups));
                        ctl.duplicates_computing = false;
                        cx.notify();
                    });
                })
                .detach();
            }
            let groups = controller.read(cx).duplicates.clone();
            let content: AnyElement = match groups {
                None => v_flex()
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("duplicates.computing").to_string()),
                    )
                    .into_any_element(),
                Some(groups) if groups.is_empty() => v_flex()
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("duplicates.empty").to_string()),
                    )
                    .into_any_element(),
                Some(groups) => {
                    let mut list = v_flex().gap_2();
                    for group in groups.iter() {
                        list = list.child(group_row(controller.clone(), group, cx));
                    }
                    div()
                        .max_h(px(420.))
                        .flex_1()
                        .overflow_y_scrollbar()
                        .child(list)
                        .into_any_element()
                }
            };

            dialog
                .title(rust_i18n::t!("duplicates.title").to_string())
                .width(px(620.))
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .cancel_text(rust_i18n::t!("settings.close").to_string()),
                )
                .child(super::with_close_x("duplicates-close-x", content))
        });
    }
}

/// One cluster: member rows plus the keep-newest action.
fn group_row(controller: Entity<LibraryController>, group: &DuplicateGroup, cx: &App) -> Div {
    let count = group.assets.len();
    // The keep-newest button is only meaningful with something left over.
    let trash_ids: Vec<Uuid> = group.assets.iter().skip(1).map(|a| a.id).collect();
    let trash_count = trash_ids.len();

    let mut card = v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child(rust_i18n::t!("duplicates.group_count", count = count).to_string()),
                )
                .when(trash_count > 0, |row| {
                    row.child(
                        Button::new(format!("dup-keep-{}", group.assets[0].id))
                            .outline()
                            .xsmall()
                            .label(
                                rust_i18n::t!("duplicates.keep_newest", count = trash_count)
                                    .to_string(),
                            )
                            .on_click(move |_, _, cx| {
                                controller.update(cx, |ctl, cx| {
                                    if let Err(e) = ctl.library.trash_assets(&trash_ids) {
                                        ctl.notice = Some(
                                            rust_i18n::t!(
                                                "workspace.trash_failed",
                                                error = e.to_string()
                                            )
                                            .to_string(),
                                        );
                                    }
                                    ctl.deselect(&trash_ids);
                                    ctl.duplicates = None;
                                    ctl.generation += 1;
                                    cx.notify();
                                });
                            }),
                    )
                }),
        );

    for asset in &group.assets {
        let added = asset.created_at.format("%Y-%m-%d %H:%M").to_string();
        card = card.child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .pl_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(display_name(asset)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} · {}", human_bytes(asset.size_bytes), added)),
                ),
        );
    }
    card
}
