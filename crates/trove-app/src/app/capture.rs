//! The screenshot pipeline: run a capture on the background executor and
//! import the file it wrote, plus the Linux in-process window/region picker.
//!
//! Split out of `app::root`. The root view's `take_screenshot` action still
//! owns the "pick a destination then capture" decision; this module owns the
//! mechanics below it.

use std::path::PathBuf;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;

// Re-export gpui's types (App, Entity, Window, AnyWindowHandle, …).
use gpui_kit::*;

use crate::library::{LibraryController, jobs};

/// Run a capture on the background executor and import what it wrote.
///
/// `fallback` is the second chance when the first target fails — the picker
/// uses it for the rectangle it highlighted, so a window that vanished
/// between the click and the capture still hands over those pixels.
///
/// A cancelled capture (the user answered the platform's own picker with
/// Esc) is not an error: nothing was written, so nothing is imported and
/// nothing is reported.
pub(crate) fn run_capture_chain(
    target: trove_core::services::screenshot::CaptureTarget,
    fallback: Option<trove_core::services::screenshot::CaptureTarget>,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    handle: AnyWindowHandle,
    cx: &mut App,
) {
    use trove_core::services::screenshot::{self, Error};

    let requested = format!("{target:?}");
    cx.spawn(async move |cx| {
        let path = dest.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move {
                match screenshot::capture(&target, &path) {
                    Ok(source) => Ok(source),
                    Err(Error::Cancelled) => Err(Error::Cancelled),
                    Err(first) => match fallback {
                        Some(fallback) => {
                            tracing::warn!(
                                error = %first.message(),
                                "screenshot target failed; trying the highlighted rectangle"
                            );
                            screenshot::capture(&fallback, &path).map_err(|second| {
                                // Both reasons, so the log names what actually
                                // refused rather than just the first attempt.
                                Error::Failed(format!(
                                    "{}; then the highlighted rectangle: {}",
                                    first.message(),
                                    second.message()
                                ))
                            })
                        }
                        None => Err(first),
                    },
                }
            })
            .await;
        let _ = handle.update(cx, |_, window, cx| match outcome {
            Ok(source) => {
                tracing::info!(
                    source = ?source,
                    dest = %dest.display(),
                    "screenshot captured; importing"
                );
                jobs::import_paths_app(&controller, vec![dest], window, cx);
            }
            Err(Error::Cancelled) => {
                tracing::info!(target = %requested, "screenshot cancelled");
            }
            Err(error) => {
                tracing::error!(error = %error.message(), "screenshot capture failed");
                window.push_notification(
                    Notification::warning(
                        rust_i18n::t!("notice.screenshot_failed", error = error.message())
                            .to_string(),
                    ),
                    cx,
                );
            }
        });
    })
    .detach();
}

/// Open the screenshot picker over a frozen frame.
///
/// Both halves of the preparation are D-Bus round trips — the frame, then
/// the compositor's window list (bounded by its own timeout) — so they run
/// on the background executor, never on the UI thread. When even the frame
/// cannot be had, the platform's own picker takes over: on Linux that is
/// `grim -g "$(slurp)"` or `scrot -s`, a working region capture without the
/// window snapping.
#[cfg(target_os = "linux")]
pub(crate) fn open_capture_picker(
    controller: Entity<LibraryController>,
    dest: PathBuf,
    handle: AnyWindowHandle,
    cx: &mut App,
) {
    use trove_core::services::screenshot::CaptureTarget;

    let fallback = {
        let (dest, controller) = (dest.clone(), controller.clone());
        move |cx: &mut App| {
            run_capture_chain(CaptureTarget::PickArea, None, dest, controller, handle, cx);
        }
    };
    cx.spawn(async move |cx| {
        let prepared = cx
            .background_executor()
            .spawn(async { prepare_pick() })
            .await;
        match prepared {
            Ok((frame, candidates)) => {
                let _ = handle.update(cx, |_, _, cx| {
                    crate::components::capture_pick::open(
                        frame, candidates, dest, controller, handle, cx,
                    );
                });
            }
            Err(reason) => {
                tracing::warn!(reason, "no in-process picker; using the platform picker");
                let _ = handle.update(cx, |_, _, cx| fallback(cx));
            }
        }
    })
    .detach();
}

/// The frame the picker freezes on, plus the windows it can snap to.
///
/// The window list is a bonus, never a requirement: without it (no scripting
/// interface, a compositor that did not answer in time) the picker still drags
/// rectangles, it just never highlights a window. On a session without KWin
/// there is not even a frame to freeze — the compositor's own picker takes
/// over, which is the same region capture with a plainer interface.
#[cfg(target_os = "linux")]
fn prepare_pick() -> Result<
    (
        image::RgbaImage,
        Vec<crate::components::capture_pick::Candidate>,
    ),
    String,
> {
    use crate::components::capture_pick::{Candidate, window_label};
    use trove_core::services::{kwin, kwin_script};

    if !kwin::available() {
        return Err("this session has no compositor interface for a frozen frame".into());
    }
    let frame = kwin::capture_workspace_image().map_err(|failure| failure.labelled())?;
    let candidates = kwin_script::window_list()
        .unwrap_or_default()
        .into_iter()
        .map(|window| Candidate {
            handle: window.handle,
            label: window_label(&window.app, &window.caption),
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        })
        .collect();
    Ok((frame, candidates))
}

/// A window the picker highlighted: the compositor renders that window
/// itself (decoration included, native resolution, nothing else in frame),
/// and the rectangle the user clicked is the fallback if the window is gone
/// by the time we ask for it.
#[cfg(target_os = "linux")]
pub(crate) fn capture_picked_window(
    picked: crate::components::capture_pick::Candidate,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    window: &mut Window,
    cx: &mut App,
) {
    use trove_core::services::screenshot::CaptureTarget;

    let target = CaptureTarget::Window {
        handle: picked.handle,
    };
    let fallback = CaptureTarget::Area {
        x: picked.x,
        y: picked.y,
        width: picked.width,
        height: picked.height,
    };
    run_capture_chain(
        target,
        Some(fallback),
        dest,
        controller,
        window.window_handle(),
        cx,
    );
}
