//! System font browser: every font installed on the OS (machine + user
//! directories), live previews through the same registration path the
//! library's font cards use, per-user uninstall and one-click import into
//! the library. Machine-owned fonts are listed but flagged non-removable —
//! trove never elevates.

use std::rc::Rc;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Sizable as _, WindowExt};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::library::LibraryController;
use trove_core::services::font_manager::{self, SystemFont};

/// Scan the system font directories on the background executor, then open
/// the browser dialog. Name-table parsing costs a few ms per file, so the
/// scan must not run on the UI thread.
pub(crate) fn open(controller: &Entity<LibraryController>, window: &mut Window, cx: &mut App) {
    // Open instantly with a scanning placeholder; the background scan fills
    // the shared list in place and refreshes the window when done.
    let fonts: Rc<RefCellState> = Rc::new(std::cell::RefCell::new(None));
    show_dialog(fonts.clone(), controller, window, cx);

    let scan = cx
        .background_executor()
        .spawn(async move { font_manager::scan_system_fonts() });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let scanned = scan.await;
        let _ = handle.update(cx, |_, _, cx| {
            *fonts.borrow_mut() = Some(scanned);
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Shared, fill-later scan result. `None` while the background scan runs.
type RefCellState = std::cell::RefCell<Option<Vec<SystemFont>>>;

/// The results dialog. The list lives in an `Rc<RefCell>` so the uninstall
/// buttons can drop rows from the still-rendered dialog.
fn show_dialog(
    fonts: Rc<RefCellState>,
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    let list = fonts;
    let total = list.borrow().as_ref().map(Vec::len).unwrap_or(0);
    // Every rendered row decodes and registers its font file with the text
    // system on first paint, so an unfiltered library (hundreds of files)
    // must not render whole — the filter input keeps the row count bounded.
    const RENDER_CAP: usize = 80;
    let filter =
        cx.new(|cx| InputState::new(window, cx).placeholder(rust_i18n::t!("sysfonts.filter")));
    let controller = controller.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let query = filter.read(cx).value().to_lowercase();
        let guard = list.borrow();
        let empty_scan = guard.is_none();
        let all: &[SystemFont] = guard.as_deref().unwrap_or(&[]);
        let matched_len = all
            .iter()
            .filter(|font| {
                query.is_empty()
                    || font.family.to_lowercase().contains(&query)
                    || font
                        .style
                        .as_deref()
                        .is_some_and(|style| style.to_lowercase().contains(&query))
            })
            .count();
        let shown: Vec<SystemFont> = all
            .iter()
            .filter(|font| {
                query.is_empty()
                    || font.family.to_lowercase().contains(&query)
                    || font
                        .style
                        .as_deref()
                        .is_some_and(|style| style.to_lowercase().contains(&query))
            })
            .take(RENDER_CAP)
            .cloned()
            .collect();
        let body = if empty_scan {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("sysfonts.scanning").to_string())
                .into_any_element()
        } else if total == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("sysfonts.empty").to_string())
                .into_any_element()
        } else if matched_len == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("sysfonts.no_match").to_string())
                .into_any_element()
        } else {
            let rows = shown
                .iter()
                .map(|font| font_row(font, &list, &controller, cx))
                .collect::<Vec<_>>();
            div()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(v_flex().gap_0p5().children(rows))
                .into_any_element()
        };

        dialog
            .title(rust_i18n::t!("sysfonts.title").to_string())
            .width(px(720.))
            .close_button(false)
            .button_props(
                gpui_kit::component::dialog::DialogButtonProps::default()
                    .show_cancel(true)
                    .cancel_text(rust_i18n::t!("settings.close").to_string()),
            )
            .child(
                v_flex()
                    .w_full()
                    .h(px(480.))
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Input::new(&filter).small().appearance(true).w_full())
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        rust_i18n::t!(
                                            "sysfonts.count",
                                            shown = matched_len.min(RENDER_CAP),
                                            total = total
                                        )
                                        .to_string(),
                                    ),
                            ),
                    )
                    .child(body),
            )
    });
}

/// One font row: live preview (registers the file with the text system on
/// first paint, exactly like the library's font cards), family + style,
/// path, then uninstall (user fonts only) and import buttons.
fn font_row(
    font: &SystemFont,
    list: &Rc<RefCellState>,
    controller: &Entity<LibraryController>,
    cx: &mut App,
) -> AnyElement {
    let family = font.family.clone();
    let style = font.style.clone();
    let path = font.path.clone();
    let writable = font.writable;

    // No add_fonts() here: these fonts are installed by definition, so the
    // platform's own font source (fontconfig / DirectWrite / CoreText)
    // resolves them by family name. Registering each row's file on the main
    // thread was what made the dialog jank — one full read + parse per row.
    let preview = crate::panels::common::font_live_preview(&family, cx)
        .w(px(120.))
        .h(px(40.))
        .text_size(px(15.));

    let path_text = path.display().to_string();
    let row_id = SharedString::from(format!(
        "sysfont-{}",
        path.display().to_string().replace(['\\', '/'], "-")
    ));

    let list_uninstall = list.clone();
    let path_uninstall = path.clone();
    let family_uninstall = family.clone();
    let controller_uninstall = controller.clone();
    let controller_import = controller.clone();
    let path_import = path.clone();

    h_flex()
        .px_1()
        .py_1()
        .rounded(cx.theme().radius)
        .gap_2()
        .items_center()
        .hover(|this| this.bg(cx.theme().secondary))
        .child(preview)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(family.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(style.unwrap_or_else(|| {
                                    rust_i18n::t!("sysfonts.no_style").to_string()
                                })),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(path_text),
                ),
        )
        .child(
            h_flex()
                .flex_none()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("{row_id}-import")))
                        .outline()
                        .xsmall()
                        .label(rust_i18n::t!("sysfonts.import").to_string())
                        .tooltip(rust_i18n::t!("sysfonts.import_tooltip").to_string())
                        .on_click(move |_, window, cx| {
                            crate::library::jobs::import_paths_app(
                                &controller_import,
                                vec![path_import.clone()],
                                window,
                                cx,
                            );
                            window.push_notification(
                                gpui_kit::component::notification::Notification::info(
                                    rust_i18n::t!("sysfonts.import_started").to_string(),
                                ),
                                cx,
                            );
                        }),
                )
                .when(writable, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("{row_id}-uninstall")))
                            .danger()
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("sysfonts.uninstall").to_string())
                            .on_click(move |_, _window, cx| {
                                match font_manager::uninstall_system_font(&path_uninstall) {
                                    Ok(()) => {
                                        if let Some(fonts) = list_uninstall.borrow_mut().as_mut() {
                                            fonts.retain(|f| f.path != path_uninstall);
                                        }
                                        controller_uninstall.update(cx, |ctl, cx| {
                                            ctl.notice = Some(
                                                rust_i18n::t!(
                                                    "sysfonts.uninstalled",
                                                    family = family_uninstall
                                                )
                                                .to_string(),
                                            );
                                            cx.notify();
                                        });
                                        cx.refresh_windows();
                                    }
                                    Err(e) => {
                                        controller_uninstall.update(cx, |ctl, cx| {
                                            ctl.notice = Some(e);
                                            cx.notify();
                                        });
                                    }
                                }
                            }),
                    )
                })
                .when(!writable, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("sysfonts.system_note").to_string()),
                    )
                }),
        )
        .into_any_element()
}
