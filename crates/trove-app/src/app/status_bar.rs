//! The window's bottom status bar: selection count, library path, the undo /
//! redo history tooltip, the 3D-renderer line, the task center, the pending
//! update badge and the latest notice.
//!
//! Split out of `app::root`. The only two fields it ever read from the view —
//! the controller and the last renderer description — are now passed in, so the
//! bar is a pure function of controller state.

use gpui_kit::base::h_flex;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;

// Re-export gpui's styled-building names (div, Div, Entity, App, SharedString,
// px, …) plus gpui-kit's styling extensions.
use gpui_kit::*;

use crate::app::root::pending_update;
use crate::app::task_panel;
use crate::library::LibraryController;

/// Bottom status bar: selection count, library path, import state and the
/// latest notice (errors surface here even outside Settings).
pub fn status_bar(
    controller: &Entity<LibraryController>,
    viewport_backend: Option<String>,
    cx: &App,
) -> Div {
    let ctl = controller.read(cx);
    let selected = ctl.selected_assets.len();
    let root = ctl.library.root().display().to_string();
    let notice = ctl.notice.clone();
    let (undo_len, redo_len) = (ctl.library.undo_len(), ctl.library.redo_len());
    // Recent-operation descriptions for the status-bar history tooltip.
    let undo_entries = ctl.library.undo_entries(5);
    let redo_entries = ctl.library.redo_entries(3);
    let history_tooltip = if undo_len == 0 && redo_len == 0 {
        None
    } else {
        let mut lines: Vec<String> = undo_entries.iter().map(describe_op).collect();
        if !redo_entries.is_empty() {
            lines.push(rust_i18n::t!("statusbar.redo_header").to_string());
            lines.extend(redo_entries.iter().map(describe_op));
        }
        Some(lines.join("\n"))
    };
    h_flex()
        .h(px(26.))
        .px_3()
        .items_center()
        .gap_4()
        .flex_shrink_0()
        .border_t_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(rust_i18n::t!("statusbar.selected", count = selected).to_string())
        .when(undo_len > 0 || redo_len > 0, {
            let history_tooltip = history_tooltip.clone();
            move |bar| {
                let history_seg = div()
                    .id("statusbar-history")
                    .when(undo_len > 0, |seg| {
                        seg.child(rust_i18n::t!("statusbar.undo", count = undo_len).to_string())
                    })
                    .when(undo_len > 0 && redo_len > 0, |seg| seg.child(" · "))
                    .when(redo_len > 0, |seg| {
                        seg.child(rust_i18n::t!("statusbar.redo", count = redo_len).to_string())
                    });
                let seg = match history_tooltip {
                    Some(text) => {
                        let text = SharedString::from(text);
                        history_seg.tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(text.clone())
                                .build(window, cx)
                        })
                    }
                    None => history_seg,
                };
                bar.child(seg)
            }
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(rust_i18n::t!("statusbar.library", path = root).to_string()),
        )
        // Which renderer is drawing an open 3D model: the GPU adapter, a
        // CPU fallback reason, or the progress of a load that is still
        // running. The model viewport used to carry this in the panel's
        // title bar; the status bar is where machine-level facts belong,
        // and the vacated title-bar room is where the preview's next
        // tools go.
        .when_some(viewport_backend, |bar, backend| {
            bar.child(div().max_w(px(360.)).truncate().child(backend))
        })
        .child(task_panel::task_panel(controller, cx))
        // The release badge: the only place a pending update is announced
        // without the user asking. Clicking it opens the release page —
        // installing is the user's call, not ours.
        .when_some(pending_update(), |bar, (version, page)| {
            bar.child(
                div()
                    .id("statusbar-update")
                    .cursor_pointer()
                    .text_color(cx.theme().info)
                    .hover(|style| style.underline())
                    .child(
                        rust_i18n::t!("statusbar.update_available", version = version).to_string(),
                    )
                    .tooltip({
                        let hint = rust_i18n::t!("statusbar.update_hint").to_string();
                        move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(hint.clone())
                                .build(window, cx)
                        }
                    })
                    .on_click(move |_, _, _| {
                        let _ = trove_core::services::open_external::open_url(&page);
                    }),
            )
        })
        .when_some(notice, |bar, notice| {
            bar.child(
                div()
                    .max_w(px(420.))
                    .truncate()
                    .text_color(cx.theme().warning)
                    .child(notice),
            )
        })
}

/// One history line for the status-bar tooltip: localized verb plus the
/// recorded target (name when a single object was touched, a count
/// otherwise).
fn describe_op(desc: &trove_core::history::undo::OpDesc) -> String {
    let action = rust_i18n::t!(desc.action.key()).to_string();
    match (&desc.target, desc.count) {
        (Some(name), _) => format!("{action} {name}"),
        (None, n) if n > 1 => format!("{action} ×{n}"),
        (None, _) => action,
    }
}
