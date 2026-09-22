//! XMP sidecar export for the current selection: one asset per UI-thread turn
//! (the store connection is thread-confined, so this cannot move to a
//! background executor), with a keyed toast tracking progress.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use crate::library::LibraryController;

/// Marker for the keyed XMP export toast: pushing with the same id replaces the
/// previous toast instead of stacking one per asset.
pub struct XmpNotice;

/// Write XMP sidecars for the current selection.
///
/// One asset per turn on the UI thread: the store connection is thread-confined,
/// so the export cannot move to a background executor, and a selection of a few
/// thousand assets would otherwise freeze the window for the whole run. A
/// sidecar lands next to the asset's file — the library's blob for a stored
/// asset, the original for a linked one — which is why this is the one edit-side
/// operation that works on a link-only library.
pub fn export_xmp_app(controller: &Entity<LibraryController>, window: &mut Window, cx: &mut App) {
    let ids: Vec<uuid::Uuid> = controller.read(cx).selected_assets.as_ref().clone();
    if ids.is_empty() {
        window.push_notification(
            Notification::warning(rust_i18n::t!("xmp.empty_selection").to_string()),
            cx,
        );
        return;
    }

    let total = ids.len();
    let handle = window.window_handle();
    let controller = controller.clone();
    window.push_notification(
        Notification::info(rust_i18n::t!("xmp.started", count = total).to_string())
            .id1::<XmpNotice>("xmp-progress"),
        cx,
    );

    cx.spawn(async move |cx| {
        let mut written = 0u64;
        let mut skipped = 0u64;
        for (done, id) in ids.iter().enumerate() {
            let outcome = controller.update(cx, |ctl, _cx| ctl.library.export_xmp_sidecars(&[*id]));
            match outcome {
                Ok(report) => {
                    written += report.written;
                    skipped += report.skipped;
                }
                Err(_) => skipped += 1,
            }
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("xmp.running", done = done + 1, total = total).to_string(),
                    )
                    .id1::<XmpNotice>("xmp-progress"),
                    cx,
                );
            });
            // Yield: a spawned foreground future runs to completion once
            // polled, so without this the whole selection would be processed
            // inside one frame — the grid would freeze and this toast would
            // never repaint until the end.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;
        }

        let note = if skipped == 0 {
            Notification::success(rust_i18n::t!("xmp.done", count = written).to_string())
        } else {
            Notification::warning(
                rust_i18n::t!("xmp.done_skipped", written = written, skipped = skipped).to_string(),
            )
        };
        let _ = handle.update(cx, |_view, window, cx| {
            window.push_notification(note, cx);
        });
    })
    .detach();
}
