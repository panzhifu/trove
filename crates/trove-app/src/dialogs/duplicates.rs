//! Duplicate finder: lists clusters of visually identical images (perceptual
//! hash distance ≤ 8) and offers per-group cleanup — keep the newest, trash
//! the rest. Content-exact duplicates cannot occur among live assets (the
//! importer deduplicates by SHA-256), so this catches re-encoded variants.
//!
//! The dialog re-queries the library on every render: cleaning one group
//! shrinks the list in place, no manual refresh needed.

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
            let groups = controller
                .read(cx)
                .library
                .find_duplicates()
                .unwrap_or_default();
            let content: AnyElement = if groups.is_empty() {
                v_flex()
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("duplicates.empty").to_string()),
                    )
                    .into_any_element()
            } else {
                let mut list = v_flex().gap_2();
                for group in &groups {
                    list = list.child(group_row(controller.clone(), group, cx));
                }
                div()
                    .max_h(px(420.))
                    .flex_1()
                    .overflow_y_scrollbar()
                    .child(list)
                    .into_any_element()
            };

            dialog
                .title(rust_i18n::t!("duplicates.title").to_string())
                .width(px(620.))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .cancel_text(rust_i18n::t!("settings.close").to_string()),
                )
                .child(content)
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
