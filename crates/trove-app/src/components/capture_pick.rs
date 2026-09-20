//! Screenshot picker: a borderless fullscreen overlay over a frozen frame,
//! where the pointer decides the granularity.
//!
//! The in-process capture only produces whole-workspace frames, and KDE has
//! no picker to call: the Screenshot portal offers full screen / current
//! screen / active window only (KDE bug 523521), and
//! `org.kde.KWin.ScreenShot2.CaptureInteractive` can pick a window or a
//! screen but not an area. So the overlay is ours — and owning it is what
//! lets one gesture cover both granularities: moving the pointer highlights
//! the window under it (its rectangle comes from the compositor's window
//! stack, see `trove_core::services::kwin_script`), clicking takes that
//! window, and dragging anywhere takes the dragged rectangle instead.
//!
//! The two outcomes are captured differently on purpose. A window is
//! rendered by the compositor (`CaptureWindow`, decoration included), so it
//! comes back without its neighbours and at full resolution; a dragged
//! rectangle is cropped out of the frozen frame, because those are the
//! pixels the user was looking at rather than what a re-render would show a
//! moment later. Esc — or a click with neither a drag nor a window under it
//! — cancels without importing anything.
//!
//! Without a window list (no scripting interface, or the compositor took
//! longer than the window list's timeout) the picker still works: it just
//! never highlights a window.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::Root;
use gpui_kit::component::{ActiveTheme as _, ThemeStyled as _};
use gpui_kit::*;

use crate::app::actions::CancelCapturePick;
use crate::library::LibraryController;
use crate::library::jobs;

/// Restore size of the overlay: where it would sit if it were ever
/// un-fullscreened.
const RESTORE_SIZE: Size<Pixels> = size(px(1280.), px(720.));

/// Shortest selection (in frame pixels) that counts as a drag rather than a
/// stray click.
const MIN_SELECTION: f32 = 4.;

/// How much the area outside the highlight is darkened.
const DIM_OPACITY: f32 = 0.45;

/// A window the pointer can snap to, in workspace coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The compositor's handle for the window — what `CaptureWindow` wants.
    pub handle: String,
    /// "application — caption", for the hint that names what is lit up.
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Candidate {
    /// Whether a workspace point falls inside.
    ///
    /// Half-open at the right and bottom edges, so two neighbouring windows
    /// never both claim the pixel on their shared boundary.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }
}

/// A window's name as the hint shows it: the application first, because
/// that is what tells two similar windows apart at a glance.
pub(crate) fn window_label(app: &str, caption: &str) -> String {
    match (app.trim(), caption.trim()) {
        ("", "") => String::new(),
        (app, "") => app.to_string(),
        ("", caption) => caption.to_string(),
        (app, caption) => format!("{app} — {caption}"),
    }
}

/// The topmost candidate under a workspace point.
///
/// `candidates` arrive in the compositor's stacking order, bottom first, so
/// the last match is the one the user is looking at.
fn topmost_at(candidates: &[Candidate], x: i32, y: i32) -> Option<usize> {
    candidates.iter().rposition(|c| c.contains(x, y))
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

/// A workspace rectangle in the overlay's own coordinates.
fn to_view(
    rect: (i32, i32, u32, u32),
    scale: f32,
    origin_x: Pixels,
    origin_y: Pixels,
) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            origin_x + px(rect.0 as f32) * scale,
            origin_y + px(rect.1 as f32) * scale,
        ),
        size: size(px(rect.2 as f32) * scale, px(rect.3 as f32) * scale),
    }
}

/// The inverse of [`to_view`] for a point: where the pointer sits in the
/// workspace, which is the space the window rectangles are measured in.
fn to_workspace(
    position: Point<Pixels>,
    scale: f32,
    origin_x: Pixels,
    origin_y: Pixels,
) -> (i32, i32) {
    let x = (f32::from(position.x) - f32::from(origin_x)) / scale;
    let y = (f32::from(position.y) - f32::from(origin_y)) / scale;
    (x.round() as i32, y.round() as i32)
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

/// Open the picker over a freshly captured frame.
///
/// `candidates` may be empty; `main` is the window that runs the capture,
/// because the overlay itself is about to disappear.
pub(crate) fn open(
    frame: image::RgbaImage,
    candidates: Vec<Candidate>,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    main: AnyWindowHandle,
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
        let view = cx.new(|cx| CapturePick::new(frame, candidates, dest, controller, main, cx));
        // Same root as the main window: it owns theme and overlays.
        cx.new(|cx| Root::new(view, window, cx))
    });
}

struct CapturePick {
    /// The captured frame, kept for cropping.
    frame: image::RgbaImage,
    /// The same pixels in the order gpui renders.
    display: Arc<RenderImage>,
    /// Windows the pointer can snap to, in stacking order (bottom first).
    candidates: Vec<Candidate>,
    dest: PathBuf,
    controller: Entity<LibraryController>,
    /// The window that runs the capture once the overlay is gone.
    main: AnyWindowHandle,
    focus: FocusHandle,
    focused: bool,
    /// Drag anchor and current pointer, in window coordinates.
    start: Option<Point<Pixels>>,
    current: Option<Point<Pixels>>,
    /// Which candidate the pointer is over, when it is not dragging.
    hovered: Option<usize>,
}

impl CapturePick {
    fn new(
        frame: image::RgbaImage,
        candidates: Vec<Candidate>,
        dest: PathBuf,
        controller: Entity<LibraryController>,
        main: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let display = display_handle(&frame);
        Self {
            frame,
            display,
            candidates,
            dest,
            controller,
            main,
            focus: cx.focus_handle(),
            focused: false,
            start: None,
            current: None,
            hovered: None,
        }
    }

    fn frame_size(&self) -> (f32, f32) {
        (self.frame.width() as f32, self.frame.height() as f32)
    }

    /// The drag as a rectangle in frame pixels, clamped to the frame;
    /// `None` when it is too small to be a selection.
    fn selection(&self, view: Size<Pixels>) -> Option<(u32, u32, u32, u32)> {
        let (start, current) = (self.start?, self.current?);
        let (scale, origin_x, origin_y) = fitted(self.frame_size(), view);
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

    /// What the overlay draws as lit up, in view coordinates: the drag if
    /// there is one, otherwise the window under the pointer. A drag wins
    /// because it is the more deliberate gesture.
    fn highlight(&self, view: Size<Pixels>) -> Option<Bounds<Pixels>> {
        let (scale, origin_x, origin_y) = fitted(self.frame_size(), view);
        if let Some((start, current)) = self.start.zip(self.current) {
            let origin = point(start.x.min(current.x), start.y.min(current.y));
            let end = point(start.x.max(current.x), start.y.max(current.y));
            return Some(Bounds {
                origin,
                size: size(end.x - origin.x, end.y - origin.y),
            });
        }
        let candidate = self.candidates.get(self.hovered?)?;
        Some(to_view(
            (candidate.x, candidate.y, candidate.width, candidate.height),
            scale,
            origin_x,
            origin_y,
        ))
    }

    /// Re-derive which window the pointer is over. Runs on every move, so it
    /// only notifies when the answer changes.
    fn update_hover(
        &mut self,
        view: Size<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let (scale, origin_x, origin_y) = fitted(self.frame_size(), view);
        let (x, y) = to_workspace(position, scale, origin_x, origin_y);
        let next = topmost_at(&self.candidates, x, y);
        if next != self.hovered {
            self.hovered = next;
            cx.notify();
        }
    }

    /// Crop the drag out of the frame, write it, hand it to the importer.
    fn crop_and_import(
        &mut self,
        (x, y, width, height): (u32, u32, u32, u32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(parent) = self.dest.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::error!(dir = %parent.display(), error = %error, "capture pick: output dir");
            window.remove_window();
            return;
        }
        let cropped = image::imageops::crop_imm(&self.frame, x, y, width, height).to_image();
        if let Err(error) = cropped.save(&self.dest) {
            tracing::error!(dest = %self.dest.display(), error = %error, "capture pick: save failed");
            window.remove_window();
            return;
        }
        tracing::info!(
            dest = %self.dest.display(),
            x,
            y,
            width,
            height,
            "capture pick: cropped png written"
        );
        jobs::import_paths_app(&self.controller, vec![self.dest.clone()], window, cx);
        window.remove_window();
    }

    /// Hand a highlighted window to the main window's capture path: the
    /// compositor renders it there, on the background executor, because this
    /// overlay is on its way out.
    fn capture_window(
        &mut self,
        candidate: Candidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (dest, controller, main) = (self.dest.clone(), self.controller.clone(), self.main);
        tracing::info!(
            handle = %candidate.handle,
            label = %candidate.label,
            "capture pick: window chosen"
        );
        window.remove_window();
        let _ = main.update(cx, |_, window, cx| {
            crate::app::capture_picked_window(candidate, dest, controller, window, cx);
        });
    }

    /// Take what the gesture selected, or cancel. A click that dragged far
    /// enough is a rectangle; a click on a window is that window; anything
    /// else closes without importing.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selection) = self.selection(window.bounds().size) {
            self.crop_and_import(selection, window, cx);
            return;
        }
        if let Some(candidate) = self.hovered.and_then(|i| self.candidates.get(i)).cloned() {
            self.capture_window(candidate, window, cx);
            return;
        }
        tracing::info!("capture pick: no usable selection; cancelling");
        window.remove_window();
    }

    fn cancel(&mut self, _: &CancelCapturePick, window: &mut Window, _: &mut Context<Self>) {
        tracing::info!("capture pick: cancelled");
        window.remove_window();
    }

    /// The hint line: name the window a click would take, and say what the
    /// other gesture does.
    fn hint(&self) -> String {
        match self.hovered.and_then(|i| self.candidates.get(i)) {
            Some(candidate) if !candidate.label.is_empty() => rust_i18n::t!(
                "notice.screenshot_window_hint",
                window = candidate.label.clone()
            )
            .to_string(),
            _ => rust_i18n::t!("notice.screenshot_region_hint").to_string(),
        }
    }
}

impl Focusable for CapturePick {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CapturePick {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take focus once so the CapturePick key context routes Esc.
        if !self.focused {
            self.focused = true;
            cx.focus_self(window);
        }
        let view = window.bounds().size;
        let (frame_width, frame_height) = self.frame_size();
        let (scale, origin_x, origin_y) = fitted((frame_width, frame_height), view);
        let highlight = self.highlight(view);

        let mut root = div()
            .relative()
            .size_full()
            .bg(black())
            .cursor(CursorStyle::Crosshair)
            .key_context(crate::CAPTURE_PICK_CONTEXT)
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
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.start.is_some() {
                    this.current = Some(event.position);
                    cx.notify();
                    return;
                }
                this.update_hover(window.bounds().size, event.position, cx);
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

        if let Some(bounds) = highlight {
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
                        .child(self.hint()),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    // No `use super::*`: the parent globs gpui in, and one of those names is
    // its own `#[test]` attribute macro, which would shadow the builtin one
    // and recurse. The geometry helpers come in by name instead.
    use gpui_kit::{point, px, size};

    use super::{Candidate, fitted, to_view, to_workspace, topmost_at, window_label};

    fn candidate(handle: &str, x: i32, y: i32, width: u32, height: u32) -> Candidate {
        Candidate {
            handle: handle.into(),
            label: handle.into(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn the_topmost_window_under_a_point_wins() {
        // Bottom first, as the compositor's stacking order reports it.
        let candidates = vec![
            candidate("bottom", 0, 0, 800, 600),
            candidate("top", 100, 100, 200, 200),
        ];
        let top = topmost_at(&candidates, 150, 150).unwrap();
        assert_eq!(candidates[top].handle, "top");
        let bottom = topmost_at(&candidates, 50, 50).unwrap();
        assert_eq!(candidates[bottom].handle, "bottom");
        assert!(topmost_at(&candidates, 900, 900).is_none());
    }

    #[test]
    fn a_windows_edges_are_half_open() {
        let candidates = vec![candidate("w", 10, 20, 30, 40)];
        // Top-left inclusive…
        assert_eq!(topmost_at(&candidates, 10, 20), Some(0));
        assert_eq!(topmost_at(&candidates, 39, 59), Some(0));
        // …bottom-right exclusive, so adjacent windows never both match.
        assert_eq!(topmost_at(&candidates, 40, 60), None);
    }

    #[test]
    fn overlay_coordinates_round_trip_to_the_workspace() {
        // A 1:1 frame letterboxed into a wider view: 100px bars each side.
        let view = size(px(1200.), px(800.));
        let (scale, origin_x, origin_y) = fitted((1000., 800.), view);
        assert_eq!(scale, 1.0);
        assert_eq!((origin_x, origin_y), (px(100.), px(0.)));

        let cursor = point(px(150.), px(300.));
        assert_eq!(to_workspace(cursor, scale, origin_x, origin_y), (50, 300));

        // And the rectangle a window maps to comes back at the same place.
        let bounds = to_view((50, 300, 200, 100), scale, origin_x, origin_y);
        assert_eq!(bounds.origin, cursor);
        assert_eq!(bounds.size, size(px(200.), px(100.)));
    }

    #[test]
    fn a_scaled_frame_keeps_the_workspace_in_frame_pixels() {
        // A downscaled frame: 2000 workspace pixels across 1000 view px.
        let view = size(px(1000.), px(500.));
        let (scale, origin_x, origin_y) = fitted((2000., 1000.), view);
        assert_eq!(scale, 0.5);
        let (x, y) = to_workspace(point(px(500.), px(250.)), scale, origin_x, origin_y);
        assert_eq!((x, y), (1000, 500));
    }

    #[test]
    fn the_label_leads_with_the_application() {
        assert_eq!(window_label("kitty", "nvim"), "kitty — nvim");
        assert_eq!(window_label("kitty", ""), "kitty");
        assert_eq!(window_label("", "Untitled"), "Untitled");
        assert_eq!(window_label("  ", "  "), "");
    }
}
