//! Search-by-image dialog: visual (pHash + colour) and semantic (CLIP).

use std::path::{Path, PathBuf};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Icon, IconName};
use gpui_kit::*;
use uuid::Uuid;

use crate::state::LibraryController;

/// One search result row (unified across visual + semantic backends).
pub(crate) struct SearchResult {
    pub name: String,
    pub score: f32,
    pub sha256: Option<String>,
}

/// Open the search-by-image dialog for the given asset, using the backend
/// selected in Settings ▸ Search (visual or semantic).
pub(crate) fn open_image_search(
    asset_id: Uuid,
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    // Resolve the query image + asset, then pick the backend from config.
    let (query_path, title, mode) = {
        let ctl = controller.read(cx);
        let conn = ctl.library.store().conn();
        let library_root = ctl.library.root().to_path_buf();
        let asset = trove_core::store::assets::get(conn, asset_id)
            .ok()
            .flatten();
        let Some(asset) = asset else { return };
        if asset.kind != trove_core::model::AssetKind::Image {
            return;
        }
        let name = asset.file_name.clone();
        let thumb = asset
            .sha256
            .as_deref()
            .map(|sha| trove_core::media::thumb::abs_path(&library_root, sha));
        let path = thumb.filter(|p| p.is_file()).or_else(|| {
            asset
                .rel_path
                .as_ref()
                .map(|rel| library_root.join(rel))
                .filter(|p| p.is_file())
        });
        let mode = trove_core::config::AppConfig::load().search_mode();
        (path, name, mode)
    };

    let Some(query_path) = query_path else { return };

    // Run the selected backend.
    let (store, library_root) = {
        let ctl = controller.read(cx);
        (
            ctl.library.store().clone(),
            ctl.library.root().to_path_buf(),
        )
    };
    let results: Vec<SearchResult> = match mode.as_str() {
        "semantic" => semantic_search(&store, controller, &query_path, cx),
        _ => visual_search(&store, &query_path),
    };

    // Show results in a dialog (backend-agnostic).
    let mode_label = match mode.as_str() {
        "semantic" => rust_i18n::t!("settings.search_mode_semantic").to_string(),
        _ => rust_i18n::t!("settings.search_mode_visual").to_string(),
    };
    show_results_dialog(results, title, mode_label, library_root, window, cx);
}

/// Visual search: pHash + colour histogram (no model needed).
fn visual_search(store: &trove_core::store::Store, query_path: &Path) -> Vec<SearchResult> {
    trove_core::store::visual_search::search_by_image(store.conn(), query_path, Some(50))
        .unwrap_or_default()
        .into_iter()
        .map(|r| SearchResult {
            name: r.asset.file_name,
            score: r.score,
            sha256: r.asset.sha256,
        })
        .collect()
}

/// Semantic search: CLIP embedding cosine similarity.
fn semantic_search(
    store: &trove_core::store::Store,
    controller: &Entity<LibraryController>,
    query_path: &Path,
    cx: &mut App,
) -> Vec<SearchResult> {
    if !trove_core::media::clip::semantic_ready() {
        let _ = controller.update(cx, |ctl, _| {
            ctl.notice = Some(rust_i18n::t!("workspace.semantic_not_ready").to_string());
        });
        return Vec::new();
    }
    let Ok(query_vec) = trove_core::media::clip::image_embedding(query_path) else {
        let _ = controller.update(cx, |ctl, _| {
            ctl.notice = Some(rust_i18n::t!("workspace.embed_failed").to_string());
        });
        return Vec::new();
    };
    let query_emb = trove_core::media::clip::Embedding::new(query_vec);
    let scored =
        trove_core::media::clip::semantic_search(store, &query_emb, Some(50)).unwrap_or_default();
    let conn = store.conn();
    scored
        .into_iter()
        .filter_map(|(id, score)| {
            let a = trove_core::store::assets::get(conn, id).ok().flatten()?;
            Some(SearchResult {
                name: a.file_name,
                score,
                sha256: a.sha256,
            })
        })
        .collect()
}

/// Render the results dialog (shared by both backends). Row-building
/// happens inside the dialog closure so that all borrows are moved in.
fn show_results_dialog(
    results: Vec<SearchResult>,
    title: String,
    mode_label: String,
    library_root: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let thumb_for = |sha: Option<&str>| -> Option<PathBuf> {
            sha.and_then(|s| {
                let p = trove_core::media::thumb::abs_path(&library_root, s);
                p.is_file().then_some(p)
            })
        };
        let rows = results
            .iter()
            .take(24)
            .map(|r| {
                let pct = (r.score * 100.0) as u32;
                let p = thumb_for(r.sha256.as_deref());
                div()
                    .px_1()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().secondary))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(match &p {
                                Some(path) => img(path.clone())
                                    .w(px(48.))
                                    .h(px(36.))
                                    .object_fit(gpui_kit::ObjectFit::Cover)
                                    .rounded(px(4.))
                                    .into_any_element(),
                                None => div()
                                    .w(px(48.))
                                    .h(px(36.))
                                    .items_center()
                                    .justify_center()
                                    .bg(cx.theme().secondary)
                                    .child(Icon::new(IconName::File).size_3())
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(r.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .w(px(44.))
                                    .text_right()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{pct}%")),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        dialog
            .title(
                rust_i18n::t!("workspace.search_results").to_string()
                    + " ["
                    + &mode_label
                    + "] "
                    + &title,
            )
            .width(px(520.))
            .child(
                v_flex()
                    .w_full()
                    .h(px(440.))
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                rust_i18n::t!(
                                    "workspace.search_results_count",
                                    count = results.len()
                                )
                                .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(v_flex().gap_0p5().children(rows)),
                    ),
            )
    });
}
