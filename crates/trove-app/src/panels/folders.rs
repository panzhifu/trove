//! Folders panel: browse assets by where they were imported from. One flat
//! row per distinct *direct* parent folder with its live-asset count on the
//! right; clicking one filters the workspace to files imported from it,
//! clicking the active one clears the filter. The full path shows as the row
//! tooltip.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelEvent};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::store::assets::source_folders;

use crate::library::LibraryController;

// =========================== Folders panel ===================================

panel!(
    FoldersPanel,
    rust_i18n::t!("panel.folders").to_string(),
    // Import-parent rows with their asset counts, keyed by the controller
    // generation they were read at. `source_folders` walks every asset row,
    // pulls the folder out of each `source_path`, and materialises one
    // `String` per asset before grouping in Rust — 46.8 ms on a 100k library.
    // `render` runs every frame, so it may only read this on a miss.
    folders: Option<(u64, Vec<(String, u64)>)>
);

impl FoldersPanel {
    pub fn new(cx: &mut Context<Self>, controller: Entity<LibraryController>) -> Self {
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
            folders: None,
        };
        super::common::observe_controller(cx, &this.controller);
        this
    }

    fn folder_rows(
        &self,
        folders: &[(String, u64)],
        active: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut list = v_flex().gap_0p5().w_full();
        for (path, count) in folders {
            let is_active = active.as_deref() == Some(path.as_str());
            let controller = self.controller.clone();
            let path_for_click = path.clone();
            let path_for_tooltip = path.clone();
            let row = div()
                .id(format!("folder-{}", stable_id(path)))
                .cursor_pointer()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .when(is_active, |row| row.bg(cx.theme().secondary))
                .hover(|s| s.bg(cx.theme().secondary))
                .tooltip(move |window, cx| {
                    Tooltip::new(SharedString::from(path_for_tooltip.clone())).build(window, cx)
                })
                .on_click(move |_ev: &ClickEvent, _window, cx| {
                    let path = path_for_click.clone();
                    controller.update(cx, move |ctl, cx| {
                        if ctl.active_folder.as_deref() == Some(path.as_str()) {
                            ctl.select_folder(None);
                        } else {
                            ctl.select_folder(Some(path));
                        }
                        cx.notify();
                    });
                })
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Folder)
                                .size_3p5()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(cx.theme().foreground)
                                .child(folder_name(path)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(count.to_string()),
                        ),
                );
            list = list.child(row);
        }
        list
    }
}

impl Render for FoldersPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the parent list at most once per controller generation. Every
        // mutation bumps that counter, and nothing else can change which
        // folders exist.
        let generation = self.controller.read(cx).generation;
        if self.folders.as_ref().map(|(cached, _)| *cached) != Some(generation) {
            let conn = self.controller.read(cx).library.store().conn();
            let rows = source_folders(conn).unwrap_or_default();
            self.folders = Some((generation, rows));
        }
        let folders: &[(String, u64)] = match &self.folders {
            Some((_, rows)) => rows.as_slice(),
            None => &[],
        };
        let active = self.controller.read(cx).active_folder.clone();

        if folders.is_empty() {
            return v_flex()
                .size_full()
                .p_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!("folders.no_folders").to_string()),
                )
                .into_any_element();
        }

        let rows = self.folder_rows(folders, active, cx);
        v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .child(div().flex_1().min_h_0().overflow_y_scrollbar().child(rows))
            .into_any_element()
    }
}

/// Just the folder's own name — the panel is a flat list of import parents.
fn folder_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Stable-enough element id from a path.
fn stable_id(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
