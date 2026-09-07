//! Folders panel: browse assets by where they were imported from. Every
//! distinct source folder (including ancestors) renders as an indented tree
//! row; clicking one filters the workspace to that subtree, clicking the
//! active one clears the filter.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelEvent};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::store::assets::source_folders;

use crate::library::LibraryController;

// =========================== Folders panel ===================================

panel!(FoldersPanel, rust_i18n::t!("panel.folders").to_string());

impl FoldersPanel {
    pub fn new(cx: &mut Context<Self>, controller: Entity<LibraryController>) -> Self {
        let this = Self {
            focus_handle: cx.focus_handle(),
            controller,
        };
        super::common::observe_controller(cx, &this.controller);
        this
    }

    /// Depth of `path` relative to its longest ancestor already shown: walk
    /// the sorted list and indent by how much of the prefix matches the
    /// previous row.
    fn folder_rows(
        &self,
        rows: Vec<String>,
        active: Option<String>,
        cx: &mut Context<Self>,
    ) -> Div {
        let all_rows = rows.clone();
        let mut list = v_flex().gap_0p5().w_full();
        for path in rows {
            let depth = depth_of(&all_rows, &path);
            let is_active = active.as_deref() == Some(path.as_str());
            let controller = self.controller.clone();
            let path_for_click = path.clone();
            let row = div()
                .id(format!("folder-{}", stable_id(&path)))
                .cursor_pointer()
                .w_full()
                .ml(px(12. * depth as f32))
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .when(is_active, |row| row.bg(cx.theme().secondary))
                .hover(|s| s.bg(cx.theme().secondary))
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
                                .child(short_name(&path, depth)),
                        ),
                );
            list = list.child(row);
        }
        list
    }
}

impl Render for FoldersPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ctl = self.controller.read(cx);
        let conn = ctl.library.store().conn();
        let folders = source_folders(conn).unwrap_or_default();
        let active = ctl.active_folder.clone();

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
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .px_1()
                    .child(rust_i18n::t!("folders.title").to_string()),
            )
            .child(div().flex_1().min_h_0().overflow_y_scrollbar().child(rows))
            .into_any_element()
    }
}

/// Nesting depth = how many already-listed ancestors this path has.
fn depth_of(rows: &[String], path: &str) -> usize {
    rows.iter()
        .filter(|other| {
            let other = other.as_str();
            other != path && path.starts_with(other) && path[other.len()..].starts_with('/')
        })
        .count()
}

/// Show the last path component (full path as tooltip-ish fallback for
/// shallow roots: keep the whole path for drives).
fn short_name(path: &str, depth: usize) -> String {
    if depth == 0 {
        return path.to_string();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Stable-enough element id from a path.
fn stable_id(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
