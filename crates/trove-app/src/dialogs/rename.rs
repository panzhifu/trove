//! Batch rename: rewrite the titles of the current selection with a pattern.
//!
//! `{n}` expands to the running index (start value editable), `{name}` to
//! the original file stem. The draft lives in an entity because the dialog's
//! content closure re-runs per frame; the preview re-computes on every
//! keystroke so the user sees exactly what "Rename" will write.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt as _};
use gpui_kit::*;

use trove_core::store::assets;

use crate::library::LibraryController;

pub struct RenameDialog;

impl RenameDialog {
    /// Open for the current selection. Refuses (with a toast) when nothing
    /// is selected.
    pub fn open(window: &mut Window, cx: &mut App, controller: Entity<LibraryController>) {
        let selection = controller.read(cx).selected_assets.clone();
        if selection.is_empty() {
            window.push_notification(
                gpui_kit::component::notification::Notification::warning(
                    rust_i18n::t!("rename.empty_selection").to_string(),
                ),
                cx,
            );
            return;
        }
        let controller = controller.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let pattern = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("rename.pattern_hint").to_string())
            });
            let start = cx.new(|cx| InputState::new(window, cx).placeholder("1"));
            let draft = cx.new(|_| RenameDraft {
                pattern: pattern.clone(),
                start: start.clone(),
                selection: selection.clone(),
                controller: controller.clone(),
            });
            let draft_ok = draft.clone();
            dialog
                .title(rust_i18n::t!("rename.title").to_string())
                .width(px(460.))
                .close_button(false)
                .child(super::with_close_x(
                    "rename-close-x",
                    v_flex()
                        .gap_2()
                        .p_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("rename.pattern").to_string()),
                        )
                        .child(Input::new(&pattern).small().appearance(true))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(rust_i18n::t!("rename.start").to_string()),
                                )
                                .child(Input::new(&start).small().appearance(true).w(px(90.))),
                        )
                        .child(preview_block(&draft_ok, cx)),
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(rust_i18n::t!("rename.apply").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let (pattern, start, selection) = {
                        let d = draft_ok.read(cx);
                        (
                            d.pattern.read(cx).value().to_string(),
                            d.start.read(cx).value().to_string(),
                            d.selection.clone(),
                        )
                    };
                    let start_number: u32 = start.trim().parse().unwrap_or(1);
                    let result = draft_ok.update(cx, |d, cx| {
                        let outcome = d.controller.update(cx, |ctl, _| {
                            ctl.library.batch_rename(&selection, &pattern, start_number)
                        });
                        match outcome {
                            Ok(count) => {
                                d.controller.update(cx, |ctl, cx| {
                                    ctl.notice = Some(
                                        rust_i18n::t!("rename.done", count = count).to_string(),
                                    );
                                    ctl.generation += 1;
                                    cx.notify();
                                });
                                None
                            }
                            Err(e) => Some(e.to_string()),
                        }
                    });
                    if let Some(error) = result {
                        draft_ok.update(cx, |d, cx| {
                            d.controller.update(cx, |ctl, cx| {
                                ctl.notice =
                                    Some(rust_i18n::t!("rename.failed", error = error).to_string());
                                cx.notify();
                            });
                        });
                    }
                    true
                })
        });
    }
}

/// Dialog draft: inputs plus the frozen selection the rename applies to.
struct RenameDraft {
    pattern: Entity<InputState>,
    start: Entity<InputState>,
    selection: Vec<uuid::Uuid>,
    controller: Entity<LibraryController>,
}

/// First few titles the current pattern would produce.
fn preview_block(draft: &Entity<RenameDraft>, cx: &mut App) -> Div {
    let (pattern, start, selection, controller) = {
        let d = draft.read(cx);
        (
            d.pattern.read(cx).value().trim().to_string(),
            d.start.read(cx).value().trim().to_string(),
            d.selection.clone(),
            d.controller.clone(),
        )
    };
    let start_number: u32 = start.trim().parse().unwrap_or(1);

    let conn = controller.read(cx).library.store().conn();
    let mut rows: Vec<String> = Vec::new();
    if !pattern.is_empty() {
        for (ix, id) in selection.iter().take(4).enumerate() {
            if let Ok(Some(asset)) = assets::get(conn, *id) {
                let stem = std::path::Path::new(&asset.file_name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&asset.file_name)
                    .to_string();
                rows.push(format!(
                    "{} → {}",
                    asset.file_name,
                    pattern
                        .replace("{n}", &(start_number + ix as u32).to_string())
                        .replace("{name}", &stem)
                ));
            }
        }
    }

    let more = selection.len().saturating_sub(4);
    let mut text = if rows.is_empty() {
        rust_i18n::t!("rename.pattern_hint").to_string()
    } else {
        rows.join("\n")
    };
    if more > 0 {
        text.push_str(&format!("\n… +{more}"));
    }

    v_flex()
        .gap_0p5()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("rename.preview").to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(text),
        )
}
