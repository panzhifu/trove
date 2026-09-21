//! Search-by-image and search-by-colour: pHash + colour histogram, no
//! model involved. The ranked hits take over the workspace grid itself —
//! the same cells, selection and preview interactions as any other view —
//! instead of opening a results dialog.

use std::path::Path;

use gpui_kit::{App, Entity, Window};
use uuid::Uuid;

use crate::library::LibraryController;

/// Open a similar-image search for `asset_id`: pHash + colour histogram
/// similarity against the stored per-asset visual signatures. The ranking
/// runs on a background thread (it decodes the query image and scans every
/// signed asset); when it lands the workspace grid switches to the results.
pub(crate) fn open_image_search(
    asset_id: Uuid,
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    // Resolve the query image + asset.
    let (query_path, title) = {
        let ctl = controller.read(cx);
        let conn = ctl.library.store().conn();
        let library_root = ctl.library.root().to_path_buf();
        let cache_root = ctl.library.cache().to_path_buf();
        let asset = trove_core::store::assets::get(conn, asset_id)
            .ok()
            .flatten();
        let Some(asset) = asset else { return };
        if asset.kind != trove_core::model::AssetKind::Image {
            return;
        }
        let name = asset.file_name.clone();
        let thumb = asset
            .content_hash
            .as_deref()
            .map(|hash| trove_core::media::thumb::abs_path(&cache_root, hash));
        let path = thumb.filter(|p| p.is_file()).or_else(|| {
            asset
                .rel_path
                .as_ref()
                .map(|rel| library_root.join(rel))
                .filter(|p| p.is_file())
        });
        (path, name)
    };

    let Some(query_path) = query_path else { return };

    let db_path = {
        let ctl = controller.read(cx);
        ctl.library.root().join("library.db")
    };
    let handle = window.window_handle();
    let mode_label = rust_i18n::t!("settings.search_mode_visual").to_string();
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        let results = cx
            .background_executor()
            .spawn(async move { visual_search(&db_path, &query_path) })
            .await;
        let _ = handle.update(cx, |_, _, cx| {
            controller.update(cx, |ctl, cx| {
                ctl.open_visual_search(format!("{mode_label} · {title}"), results);
                cx.notify();
            });
        });
    })
    .detach();
}

/// Search images whose palette contains a colour close to `hex` (Inspector
/// swatch right-click, the workspace colour picker) and show the ranked
/// hits in the workspace grid.
pub(crate) fn open_color_search(
    hex: &str,
    controller: &Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    let db_path = {
        let ctl = controller.read(cx);
        ctl.library.root().join("library.db")
    };
    let handle = window.window_handle();
    let mode_label = rust_i18n::t!("workspace.color_search").to_string();
    let hex_owned = hex.to_string();
    let controller = controller.clone();
    cx.spawn(async move |cx| {
        let hex_for_search = hex_owned.clone();
        let results = cx
            .background_executor()
            .spawn(async move { color_search(&db_path, &hex_for_search) })
            .await;
        let _ = handle.update(cx, |_, _, cx| {
            controller.update(cx, |ctl, cx| {
                ctl.open_visual_search(format!("{mode_label} · {hex_owned}"), results);
                cx.notify();
            });
        });
    })
    .detach();
}

/// Rank images by pHash + colour-histogram similarity to `query_path`.
/// Runs on the background executor: it decodes the query and reads every
/// stored visual signature.
fn visual_search(db_path: &Path, query_path: &Path) -> Vec<(Uuid, f32)> {
    let Ok(store) = trove_core::store::Store::open(db_path) else {
        return Vec::new();
    };
    trove_core::store::visual_search::search_by_image(store.conn(), query_path, Some(50))
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.asset.id, r.score))
        .collect()
}

/// Search images whose palette contains a colour close to `hex`.
fn color_search(db_path: &Path, hex: &str) -> Vec<(Uuid, f32)> {
    let Ok(store) = trove_core::store::Store::open(db_path) else {
        return Vec::new();
    };
    trove_core::store::visual_search::search_by_color(store.conn(), hex, Some(50))
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.asset.id, r.score))
        .collect()
}
