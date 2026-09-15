//! Region picker for screenshots: a borderless fullscreen overlay showing a
//! frozen frame that the user drags a rectangle on.
//!
//! The in-process capture only produces whole-workspace frames, and KDE has
//! no region picker to call: the Screenshot portal offers full screen /
//! current screen / active window only (KDE bug 523521), and
//! `org.kde.KWin.ScreenShot2.CaptureInteractive` can pick a window or a
//! single point but not an area. So the overlay is ours. Dragging a
//! rectangle crops it out of the frame, writes the PNG and hands it to the
//! importer; Esc (or a click without a drag) cancels without importing.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::Root;
use gpui_kit::component::{ActiveTheme as _, ThemeStyled as _};
use gpui_kit::*;

use crate::app::actions::CancelScreenshotRegion;
use crate::library::LibraryController;
use crate::library::jobs;

/// Restore size of the overlay: where it would sit if it were ever
/// un-fullscreened.
const RESTORE_SIZE: Size<Pixels> = size(px(1280.), px(720.));

/// Shortest selection (in frame pixels) that counts as a drag rather than a
/// stray click.
const MIN_SELECTION: f32 = 4.;

/// How much the area outside the selection is darkened.
const DIM_OPACITY: f32 = 0.45;

/// Open the picker over a freshly captured frame. The selection is cropped
/// from `frame`, saved to `dest` and imported.
pub(crate) fn open(
    frame: image::RgbaImage,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    cx: &mut App,
) {
    let options = gpui_kit::WindowOptions {
        window_bounds: Some(WindowBounds::Fullscreen(Bounds::centered(
            None,
            RESTORE_SIZE,
            cx,
        ))),
        ..crate::app::title_bar::window_options()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| RegionSelect::new(frame, dest, controller, cx));
        // Same root as the main window: it owns theme and overlays.
        cx.new(|cx| Root::new(view, window, cx))
    });
}

/// Where the frame lands inside a window of `view` size when letterboxed:
/// the scale factor and the top-left corner of the drawn image.
fn fitted(image: (f32, f32), view: Size<Pixels>) -> (f32, Pixels, Pixels) {
    let scale = (f32::from(view.width) / image.0).min(f32::from(view.height) / image.1);
    (
        scale,
        (view.width - px(image.0) * scale) / 2.,
        (view.height - px(image.1) * scale) / 2.,
    )
}

/// gpui's `RenderImage` wants BGRA byte order, so swap the channels of the
/// RGBA frame for display. Cropping keeps using the untouched original.
fn display_handle(frame: &image::RgbaImage) -> Arc<RenderImage> {
    let mut bytes = frame.as_raw().clone();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(frame.width(), frame.height(), bytes)
        .expect("the buffer came from an image of the same size");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

struct RegionSelect {
    /// The captured frame, kept for cropping.
    frame: image::RgbaImage,
    /// The same pixels in the order gpui renders.
    display: Arc<RenderImage>,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    focus: FocusHandle,
    focused: bool,
    /// Drag anchor and current pointer, in window coordinates.
    start: Option<Point<Pixels>>,
    current: Option<Point<Pixels>>,
}

impl RegionSelect {
    fn new(
        frame: image::RgbaImage,
        dest: PathBuf,
        controller: Entity<LibraryController>,
        cx: &mut Context<Self>,
    ) -> Self {
        let display = display_handle(&frame);
        Self {
            frame,
            display,
            dest,
            controller,
            focus: cx.focus_handle(),
            focused: false,
            start: None,
            current: None,
        }
    }

    /// The drag as a rectangle in frame pixels, clamped to the frame;
    /// `None` when it is too small to be a selection.
    fn selection(&self, view: Size<Pixels>) -> Option<(u32, u32, u32, u32)> {
        let (start, current) = (self.start?, self.current?);
        let (scale, origin_x, origin_y) = fitted(
            (self.frame.width() as f32, self.frame.height() as f32),
            view,
        );
        let to_frame =
            |value: Pixels, origin: Pixels| (f32::from(value) - f32::from(origin)) / scale;
        let left = to_frame(start.x.min(current.x), origin_x).clamp(0., self.frame.width() as f32);
        let top = to_frame(start.y.min(current.y), origin_y).clamp(0., self.frame.height() as f32);
        let right = to_frame(start.x.max(current.x), origin_x).clamp(0., self.frame.width() as f32);
        let bottom =
            to_frame(start.y.max(current.y), origin_y).clamp(0., self.frame.height() as f32);
        let (width, height) = (right - left, bottom - top);
        if width < MIN_SELECTION || height < MIN_SELECTION {
            return None;
        }
        Some((left as u32, top as u32, width as u32, height as u32))
    }

    /// Crop the selection, write it, hand it to the importer and close.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((x, y, width, height)) = self.selection(window.bounds().size) else {
            tracing::info!("region select: no usable selection; cancelling");
            window.remove_window();
            return;
        };
        if let Some(parent) = self.dest.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::error!(dir = %parent.display(), error = %error, "region select: output dir");
            window.remove_window();
            return;
        }
        let cropped = image::imageops::crop_imm(&self.frame, x, y, width, height).to_image();
        if let Err(error) = cropped.save(&self.dest) {
            tracing::error!(dest = %self.dest.display(), error = %error, "region select: save failed");
            window.remove_window();
            return;
        }
        tracing::info!(
            dest = %self.dest.display(),
            x,
            y,
            width,
            height,
            "region select: cropped png written"
        );
        jobs::import_paths_app(&self.controller, vec![self.dest.clone()], window, cx);
        window.remove_window();
    }

    fn cancel(&mut self, _: &CancelScreenshotRegion, window: &mut Window, _: &mut Context<Self>) {
        tracing::info!("region select: cancelled");
        window.remove_window();
    }
}

impl Focusable for RegionSelect {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RegionSelect {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take focus once so the ScreenshotRegion key context routes Esc.
        if !self.focused {
            self.focused = true;
            cx.focus_self(window);
        }
        let view = window.bounds().size;
        let (frame_width, frame_height) = (self.frame.width() as f32, self.frame.height() as f32);
        let (scale, origin_x, origin_y) = fitted((frame_width, frame_height), view);
        let selection = match (self.start, self.current) {
            (Some(start), Some(current)) => {
                let origin = point(start.x.min(current.x), start.y.min(current.y));
                let end = point(start.x.max(current.x), start.y.max(current.y));
                Some(Bounds {
                    origin,
                    size: size(end.x - origin.x, end.y - origin.y),
                })
            }
            _ => None,
        };

        let mut root = div()
            .relative()
            .size_full()
            .bg(black())
            .cursor(CursorStyle::Crosshair)
            .key_context(crate::REGION_SELECT_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.start = Some(event.position);
                    this.current = Some(event.position);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.start.is_some() {
                    this.current = Some(event.position);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, window, cx| this.confirm(window, cx)),
            )
            .child(
                img(ImageSource::Render(self.display.clone()))
                    .absolute()
                    .left(origin_x)
                    .top(origin_y)
                    .w(px(frame_width) * scale)
                    .h(px(frame_height) * scale),
            );

        if let Some(bounds) = selection {
            let dim = black().opacity(DIM_OPACITY);
            let right = bounds.origin.x + bounds.size.width;
            let bottom = bounds.origin.y + bounds.size.height;
            root = root
                .child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(0.))
                        .right(px(0.))
                        .h(bounds.origin.y)
                        .bg(dim),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(bottom)
                        .bottom(px(0.))
                        .bg(dim),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(bounds.origin.y)
                        .w(bounds.origin.x)
                        .h(bounds.size.height)
                        .bg(dim),
                )
                .child(
                    div()
                        .absolute()
                        .left(right)
                        .right(px(0.))
                        .top(bounds.origin.y)
                        .h(bounds.size.height)
                        .bg(dim),
                )
                .child(
                    div()
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .border_1()
                        .border_color(cx.theme().primary),
                );
        }

        root.child(
            div()
                .absolute()
                .top(px(16.))
                .left(px(0.))
                .right(px(0.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .rounded_full_style(cx)
                        .bg(black().opacity(0.6))
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(white())
                        .child(rust_i18n::t!("notice.screenshot_region_hint").to_string()),
                ),
        )
    }
}
