//! The viewport's element tree: the canvas, the toolbar, and the two things
//! that stand in for a frame before one exists.

use std::sync::Arc;

use gpui_kit::base::{ElementExt as _, h_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Backend, Drag, ModelViewport, drag_for};

/// The camera's distance bounds for the configured magnification limits.
///
/// [`render3d::Camera::zoom`] scales the eye *distance*, so it is the
/// reciprocal of a magnification: a small value moves the camera in and
/// makes the model look bigger. The config stores magnification (shared
/// with the image preview, where it is used directly), so the bounds are
/// inverted here — the smallest distance is the *largest* magnification.
pub(super) fn distance_bounds(cfg: &trove_core::config::AppConfig) -> (f32, f32) {
    (1.0 / cfg.max_preview_zoom(), 1.0 / cfg.min_preview_zoom())
}

impl ModelViewport {
    /// The canvas: the model, and every interaction that moves the camera.
    pub(super) fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let image = self.shown.clone();
        let cursor = if self.is_dragging() {
            CursorStyle::ClosedHand
        } else {
            CursorStyle::OpenHand
        };

        div()
            // `on_prepaint` lives on the plain `Div`, before the element
            // becomes `Stateful`; the id has to come after it.
            .on_prepaint(move |bounds: Bounds<Pixels>, _, cx| {
                let size = bounds.size;
                entity.update(cx, |this, cx| this.measure(size, cx));
            })
            .id("model-canvas")
            // The canvas takes focus itself: `on_key_down` only fires for an
            // element on the focus path, so without this the shortcuts below
            // are dead. `track_focus` puts it on that path; `render` claims
            // focus the first time the viewport is painted.
            .track_focus(&self.focus_handle)
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .cursor(cursor)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    // Clicking the model focuses it, so the keys work without
                    // having to know that they need to be aimed at the canvas.
                    window.focus(&this.focus_handle, cx);
                    if let Some(mode) = drag_for(event.button, event.modifiers) {
                        this.begin_drag(mode, event.position, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if let Some(mode) = drag_for(event.button, event.modifiers) {
                        this.begin_drag(mode, event.position, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(mode) = this.drag else {
                    return;
                };
                let dx = (event.position.x - this.drag_from.x).as_f32();
                let dy = (event.position.y - this.drag_from.y).as_f32();
                this.drag_from = event.position;
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                match mode {
                    // Grab-and-turn: the surface under the cursor follows it.
                    // Dragging right therefore *decreases* yaw — increasing it
                    // moves the eye towards +x, which swings the model's front
                    // face the other way — and dragging down tips the top
                    // towards the viewer.
                    Drag::Orbit => {
                        let step = this.turn_per_pixel();
                        this.camera.orbit(-dx * step, dy * step);
                        // Turning is the one gesture that happens *around*
                        // something, so say what that is: CloudCompare calls
                        // `showPivotSymbol(true)` from exactly this place in
                        // its mouse-move handler.
                        this.pivot_shown = true;
                    }
                    // The model follows the hand: the pivot moves the opposite
                    // way to the drag, so the geometry under the cursor stays
                    // under it.
                    Drag::Pan => {
                        let step = this.camera.pan_per_pixel(this.logical.1.max(1.0));
                        this.camera.pan_by([-dx * step, dy * step]);
                    }
                }
                this.dirty = true;
                // Render now, off the UI thread: the frame follows the cursor
                // instead of waiting for the button to come up. `pump` coalesces
                // — while a frame is in flight, later moves only update the pose
                // it will pick up next, so a fast drag never queues work.
                this.pump(cx);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            // Released outside the canvas: the drag still has to end.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let lines = match event.delta {
                    ScrollDelta::Lines(delta) => delta.y,
                    ScrollDelta::Pixels(delta) => delta.y.as_f32() / 40.0,
                };
                if lines == 0.0 {
                    return;
                }
                // Exponential, so every notch scales the distance equally.
                let cfg = trove_core::config::AppConfig::load();
                let (zmin, zmax) = distance_bounds(&cfg);
                this.camera.zoom_by((-lines * 0.12).exp(), zmin, zmax);
                // A wheel notch is a gesture with no button to release, so it
                // runs on the linger window: draft frames while the wheel
                // turns, one settled frame once it stops.
                this.begin_gesture(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let keystep = 0.05f32;
                // A nudge of a twentieth of the model's radius, which reads the
                // same at any zoom — the same idea as `turn_per_pixel`.
                let panstep = 0.05f32;
                let zoom_factor = 1.1f32;
                // Read zoom limits from config once per key press.
                let cfg = trove_core::config::AppConfig::load();
                let (zmin, zmax) = distance_bounds(&cfg);
                // Shift turns the arrow keys from turning the model into
                // sliding it, so the keyboard can do everything the mouse can.
                let pan = event.keystroke.modifiers.shift;
                let key = event.keystroke.key.as_str();
                // A turn shows the pivot symbol, exactly as the mouse's orbit
                // drag does. A key has no release event to hide it again, so
                // `finish_gesture` (frame.rs) puts it away when the keyboard
                // stops — the same job the mouse release does for a drag.
                let nudge = |this: &mut Self, dx: f32, dy: f32| {
                    if pan {
                        // Panning pushes the *view*: the model slides the other
                        // way, which is what an arrow key does in every viewer.
                        this.camera.pan_by([dx * panstep, dy * panstep]);
                    } else {
                        // Turning follows the drag's convention instead, so an
                        // arrow turns the model the way it points.
                        this.camera.orbit(-dx * keystep, dy * keystep);
                        this.pivot_shown = true;
                    }
                    this.dirty = true;
                };
                match key {
                    "left" => nudge(this, -1.0, 0.0),
                    "right" => nudge(this, 1.0, 0.0),
                    "up" => nudge(this, 0.0, -1.0),
                    "down" => nudge(this, 0.0, 1.0),
                    "w" => nudge(this, 0.0, -1.0),
                    "s" => nudge(this, 0.0, 1.0),
                    "a" => nudge(this, -1.0, 0.0),
                    "d" => nudge(this, 1.0, 0.0),
                    "e" => {
                        this.camera.zoom_by(1.0 / zoom_factor, zmin, zmax);
                        this.dirty = true;
                    }
                    "q" => {
                        this.camera.zoom_by(zoom_factor, zmin, zmax);
                        this.dirty = true;
                    }
                    "=" | "+" => {
                        this.camera.zoom_by(1.0 / zoom_factor, zmin, zmax);
                        this.dirty = true;
                    }
                    "-" | "_" => {
                        this.camera.zoom_by(zoom_factor, zmin, zmax);
                        this.dirty = true;
                    }
                    "r" => this.reset_camera(cx),
                    _ => return,
                }
                // The key was aimed at the viewport: keep an arrow — or a
                // `q`/`e` bound elsewhere — from also running the app's own
                // action for it.
                cx.stop_propagation();
                if key == "r" {
                    // `reset_camera` drew the settled frame itself.
                    cx.notify();
                } else {
                    // A held arrow repeats, so this is a gesture: draft frames
                    // while it moves, a settled frame once it stops.
                    this.begin_gesture(cx);
                }
            }))
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.reset_camera(cx);
                }
            }))
            .child(match image {
                Some(frame) => frame_element(frame),
                None => self.placeholder(cx),
            })
            // The pivot symbol is drawn over the model rather than inside it,
            // just as CloudCompare draws it last with no depth test: where the
            // rotation centre should be stays readable even when the model is
            // in front of it.
            .child(self.pivot_symbol(cx))
            .child(self.look_switches(cx))
            .child(self.shortcuts_hint(cx))
    }

    /// The one look switch, in the canvas's top-left corner: painting the
    /// model by height.
    ///
    /// On the canvas rather than in the toolbar because it changes what is
    /// drawn, not the panel's chrome. A persisted toggle: flipping it redraws,
    /// and the frame loop's periodic config re-read agrees with what is on
    /// screen instead of flicking it back.
    fn look_switches(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let height_on = self.height_color;
        h_flex().absolute().top_2().left_2().gap_1().child(
            Button::new("height-color")
                .xsmall()
                .when(height_on, |button| button.primary())
                .when(!height_on, |button| button.ghost())
                .icon(IconName::Palette)
                .label(rust_i18n::t!("viewport.height_color").to_string())
                .tooltip(rust_i18n::t!("viewport.height_color_tip").to_string())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.height_color = !this.height_color;
                    let mut config = trove_core::config::AppConfig::load();
                    config.height_color = Some(this.height_color);
                    let _ = config.save();
                    this.enhance_checked = None;
                    this.dirty = true;
                    this.pump(cx);
                    cx.notify();
                })),
        )
    }

    /// The pivot symbol: the ball-and-rings marker CloudCompare shows while
    /// the model is being turned, at the point the camera orbits around.
    ///
    /// Painted as a UI layer over the frame rather than inside it, for the same
    /// reasons CloudCompare clears the depth buffer before drawing its own: the
    /// symbol belongs to the viewport, not to the scene, so the model never
    /// hides the thing that says where it turns. Two things follow that matter
    /// here more than anywhere else — one implementation covers both renderers,
    /// and a turn is exactly when the viewport is drawing half-resolution draft
    /// frames (`INTERACTIVE_SCALE`), which this overlay therefore stays sharp
    /// through. Painted from `self.camera` directly, it also tracks the cursor
    /// instead of waiting for the frame behind it.
    ///
    /// Visibility is `pivot_shown`, which only a turn sets: CloudCompare's
    /// default `PIVOT_SHOW_ON_MOVE`, with its other two modes left out on
    /// purpose so there is nothing to configure and nothing to turn off by
    /// accident.
    fn pivot_symbol(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        use trove_core::media::render3d::AXIS_ORIGIN;
        if !self.pivot_shown {
            return div().into_any_element();
        }
        let (width, height) = self.logical;
        // The geometry is relative to the model's bounding sphere, so with no
        // bounds and no measured size there is nothing to place the symbol on.
        if width <= 0.0 || height <= 0.0 || self.scene_bounds.is_empty() {
            return div().into_any_element();
        }
        let framing = self
            .camera
            .framing(self.scene_bounds, width / height.max(1.0));
        let symbol = pivot_symbol_geometry(&framing, (width, height));

        div()
            .absolute()
            .inset_0()
            .child(gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    // Ball first, then the three rings over it: the order
                    // CloudCompare's `drawPivot` emits them in.
                    if let Some(path) = fill_path(&to_points(bounds.origin, &symbol.ball)) {
                        window.paint_path(path, axis_tint(AXIS_ORIGIN, 1.0));
                    }
                    for line in &symbol.lines {
                        let points = to_points(bounds.origin, &line.points);
                        if let Some(path) = stroke_path(&points, line.closed) {
                            window.paint_path(path, axis_tint(line.color, PIVOT_ALPHA));
                        }
                    }
                },
            ))
            .into_any_element()
    }

    /// A small panel in the canvas's top-right corner listing the keyboard
    /// shortcuts. The top-left belongs to the look switch, so the top-right
    /// corner is the free one.
    fn shortcuts_hint(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::base::v_flex;
        let line = |text: String| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        v_flex()
            .absolute()
            .top_2()
            .right_2()
            .gap_1()
            .p_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .opacity(0.8)
            .child(line(rust_i18n::t!("viewport.shortcuts").to_string()))
            .child(line(rust_i18n::t!("viewport.shortcut_rotate").to_string()))
            .child(line(
                rust_i18n::t!("viewport.shortcut_pan_drag").to_string(),
            ))
            .child(line(
                rust_i18n::t!("viewport.shortcut_pan_keys").to_string(),
            ))
            .child(line(rust_i18n::t!("viewport.shortcut_zoom").to_string()))
            .child(line(rust_i18n::t!("viewport.shortcut_reset").to_string()))
    }

    /// What the canvas shows before the first frame arrives.
    fn placeholder(&self, cx: &App) -> AnyElement {
        let message = match &self.backend {
            Backend::Loading => rust_i18n::t!("viewport.loading").to_string(),
            _ => match &self.error {
                Some(reason) => {
                    rust_i18n::t!("viewport.render_failed", reason = reason).to_string()
                }
                None => rust_i18n::t!("viewport.rendering").to_string(),
            },
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(message)
            .into_any_element()
    }

    /// The viewport's title-bar controls: the reset / close buttons.
    ///
    /// Rendered by the host panel's title bar while a model preview is open —
    /// see `WorkspacePanel::title_suffix` — so the canvas below is nothing but
    /// the picture. The title bar supplies the chrome, so this carries no
    /// padding or border of its own.
    ///
    /// Geometry stats, the frame time and the MSAA factor used to be listed
    /// here, and the backend before that. All gone — the row now stays free
    /// for the tools that will come after it.
    pub(crate) fn title_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Deliberately *not* `w_full`: this renders in the panel's title bar,
        // which is a row shared with the title and the window controls. Asking
        // for the full width there pushes the row past the panel's edge; the
        // content sizes itself instead, and the name lives in the tab.
        h_flex()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(
                Button::new("model-reset")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RotateCw)
                    .tooltip(rust_i18n::t!("viewport.reset").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.reset_camera(cx))),
            )
            .child(
                Button::new("model-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(rust_i18n::t!("viewport.close").to_string())
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Free the frame before the panel drops us.
                        this.release(window);
                        this.close(cx);
                    })),
            )
    }
}

/// The rendered frame, stretched over the whole canvas.
///
/// The explicit `size_full` is load-bearing. gpui's image element lays itself
/// out at the *image's own pixel size* whenever its style leaves the size
/// `Auto` — `absolute` and `inset_0` set the insets, not the size — so a frame
/// rendered at a different resolution would be drawn at that size in the
/// canvas's top-left corner instead of being stretched over it. That is
/// exactly what a half-resolution draft frame (see [`INTERACTIVE_SCALE`])
/// would do: the picture shrinks into the corner every time the camera moves
/// and snaps back when the settled frame arrives, since that one happens to be
/// canvas-sized.
fn frame_element(frame: Arc<RenderImage>) -> AnyElement {
    img(ImageSource::Render(frame))
        .absolute()
        .inset_0()
        .size_full()
        .object_fit(ObjectFit::Fill)
        .into_any_element()
}

// ============================================================================
// Pivot symbol geometry
// ============================================================================

/// Ring radius as a fraction of the viewport's shorter edge: CloudCompare's
/// `CC_DISPLAYED_PIVOT_RADIUS_PERCENT` (0.8) taken of *half* that edge, so the
/// rings span four fifths of it.
const PIVOT_RING_RADIUS: f32 = 0.4;
/// Points per ring — the same 64 `glDrawUnitCircle` steps with.
const PIVOT_RING_SEGMENTS: usize = 64;
/// Ring and diameter width in logical pixels, from `glLineWidth(2.0)`.
const PIVOT_LINE_WIDTH: f32 = 2.0;
/// Ring opacity: CloudCompare's `c_alpha = MAX * 0.6`.
const PIVOT_ALPHA: f32 = 0.6;
/// Radius of the centre ball in logical pixels. `ccSphere` is built with radius
/// `10 / symbolRadius` and the symbol is then scaled by `symbolRadius` pixels,
/// which is a 10 px ball at any zoom.
const PIVOT_BALL_RADIUS: f32 = 10.0;
/// Points around the ball's outline: the drawing precision `ccSphere` uses.
const PIVOT_BALL_SEGMENTS: usize = 24;

/// One stroked piece of the symbol, in canvas-local pixels.
#[derive(Clone)]
struct PivotLine {
    /// The polyline's vertices.
    points: Vec<[f32; 2]>,
    /// A ring closes back on its first point; the diameter through it does not.
    closed: bool,
    /// Which axis this piece belongs to. The colours come from the same
    /// `render3d` constants the scene gizmo uses, so the two cannot disagree
    /// about which axis is which.
    color: [f32; 3],
}

/// The symbol as one camera pose places it.
struct PivotSymbol {
    /// The three rings with their diameters, X then Y then Z.
    lines: Vec<PivotLine>,
    /// The centre ball, as a screen-space disc rather than a projected circle:
    /// CloudCompare's is a lit sphere, which reads as a dot from any angle.
    ball: Vec<[f32; 2]>,
}

/// CloudCompare's `drawPivot` arithmetic with the OpenGL taken out.
///
/// Its `glTranslated(pivotPoint)` followed by `glScaled(symbolRadius *
/// pixelSize)` says: put a unit circle on the rotation centre and give it a
/// radius of `symbolRadius` *pixels*. The two steps here are that the pivot is
/// already the origin of the unit space a [`Framing`] lives in, and that
/// `radius` below is a pixel count converted to that space — which is what
/// keeps the symbol the same size on screen at any zoom. The points then go
/// through the real projection, so a ring turns into an ellipse and, edge-on,
/// into a line, exactly as the 3D draw does.
fn pivot_symbol_geometry(
    framing: &trove_core::media::render3d::Framing,
    canvas: (f32, f32),
) -> PivotSymbol {
    use trove_core::media::render3d::{AXIS_X, AXIS_Y, AXIS_Z};

    let (width, height) = (canvas.0.max(1.0), canvas.1.max(1.0));
    // How much of the view one pixel covers at the pivot, in bounding-sphere
    // radii: the same quantity `Camera::pan_per_pixel` measures, with the
    // camera's own distance and field of view rather than the camera's zoom.
    let units_per_pixel = 2.0 * framing.distance * framing.tan_half / height;
    let radius = PIVOT_RING_RADIUS * width.min(height) * units_per_pixel;

    let dot3 = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    // A point measured from the pivot out to canvas pixels: through the
    // camera's basis, then the perspective divide `to_screen` does.
    let to_screen = |p: [f32; 3]| -> [f32; 2] {
        let d = [
            p[0] - framing.eye[0],
            p[1] - framing.eye[1],
            p[2] - framing.eye[2],
        ];
        let view = [
            dot3(d, framing.right),
            dot3(d, framing.up),
            dot3(d, framing.forward),
        ];
        framing.to_screen(view, width, height).0
    };
    let unit = |axis: usize| -> [f32; 3] {
        match axis {
            0 => [1.0, 0.0, 0.0],
            1 => [0.0, 1.0, 0.0],
            _ => [0.0, 0.0, 1.0],
        }
    };

    let mut lines = Vec::with_capacity(6);
    for (axis, color) in [AXIS_X, AXIS_Y, AXIS_Z].into_iter().enumerate() {
        // The ring spans the two axes *other* than the one that colours it, so
        // it is seen end-on down its own axis: `glDrawUnitCircle`'s
        // `dimX = dim + 1`, `dimY = dimX + 1`, both wrapping at three.
        let (u, v) = (unit((axis + 1) % 3), unit((axis + 2) % 3));
        let mut ring = Vec::with_capacity(PIVOT_RING_SEGMENTS);
        for step in 0..PIVOT_RING_SEGMENTS {
            let theta = std::f32::consts::TAU * step as f32 / PIVOT_RING_SEGMENTS as f32;
            let (cos, sin) = (theta.cos(), theta.sin());
            ring.push(to_screen([
                (u[0] * cos + v[0] * sin) * radius,
                (u[1] * cos + v[1] * sin) * radius,
                (u[2] * cos + v[2] * sin) * radius,
            ]));
        }
        lines.push(PivotLine {
            points: ring,
            closed: true,
            color,
        });
        // The axis itself, through the centre: the `GL_LINES` pair from -1 to
        // +1 in the same scaled space.
        let tip = unit(axis);
        lines.push(PivotLine {
            points: vec![
                to_screen(tip.map(|c| c * -radius)),
                to_screen(tip.map(|c| c * radius)),
            ],
            closed: false,
            color,
        });
    }

    let center = to_screen([0.0; 3]);
    let mut ball = Vec::with_capacity(PIVOT_BALL_SEGMENTS);
    for step in 0..PIVOT_BALL_SEGMENTS {
        let theta = std::f32::consts::TAU * step as f32 / PIVOT_BALL_SEGMENTS as f32;
        ball.push([
            center[0] + PIVOT_BALL_RADIUS * theta.cos(),
            center[1] + PIVOT_BALL_RADIUS * theta.sin(),
        ]);
    }
    PivotSymbol { lines, ball }
}

/// A stroked path tracing `points`, or `None` when the platform refused it and
/// the piece is skipped for the frame.
fn stroke_path(points: &[Point<Pixels>], closed: bool) -> Option<gpui::Path<Pixels>> {
    let mut builder = gpui::PathBuilder::stroke(px(PIVOT_LINE_WIDTH));
    builder.add_polygon(points, closed);
    builder.build().ok()
}

/// A filled path through a closed outline, for the centre ball.
fn fill_path(points: &[Point<Pixels>]) -> Option<gpui::Path<Pixels>> {
    let mut builder = gpui::PathBuilder::fill();
    builder.add_polygon(points, true);
    builder.build().ok()
}

/// Canvas-local points moved into window coordinates.
fn to_points(origin: Point<Pixels>, points: &[[f32; 2]]) -> Vec<Point<Pixels>> {
    points
        .iter()
        .map(|p| origin + gpui::point(px(p[0]), px(p[1])))
        .collect()
}

/// One `render3d` axis colour at `alpha`. `Rgba`'s components are the same 0..1
/// floats the constants hold, so the rings are the scene's own axis colours with
/// the transparency CloudCompare draws them at.
fn axis_tint(color: [f32; 3], alpha: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: color[0],
        g: color[1],
        b: color[2],
        a: alpha,
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags in a `test`
    // attribute macro from the gpui prelude (see `frame.rs`'s test module).
    use super::{PivotLine, pivot_symbol_geometry};
    use trove_core::media::formats::types::Bounds;
    use trove_core::media::render3d::{Camera, Framing};

    /// The canvas the tests place the symbol on: 900×600, so the shorter edge
    /// is 600 and a ring's radius is `0.4 * 600 = 240` px.
    const CANVAS: (f32, f32) = (900.0, 600.0);
    const CENTRE: [f32; 2] = [450.0, 300.0];
    /// Where the rings land on a canvas this size.
    const RING: f32 = 240.0;

    fn framing(camera: &Camera) -> Framing {
        camera.framing(
            Bounds {
                min: [-1.0; 3],
                max: [1.0; 3],
            },
            CANVAS.0 / CANVAS.1,
        )
    }

    fn lines() -> Vec<PivotLine> {
        pivot_symbol_geometry(&framing(&Camera::default()), CANVAS).lines
    }

    fn extent(points: &[[f32; 2]]) -> (f32, f32, f32, f32) {
        let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for point in points {
            min_x = min_x.min(point[0]);
            max_x = max_x.max(point[0]);
            min_y = min_y.min(point[1]);
            max_y = max_y.max(point[1]);
        }
        (min_x, max_x, min_y, max_y)
    }

    /// Rings and diameters alternate, X then Y then Z.
    fn ring(index: usize) -> PivotLine {
        lines()[index * 2].clone()
    }

    fn diameter(index: usize) -> PivotLine {
        lines()[index * 2 + 1].clone()
    }

    /// The camera always looks straight down at its own pivot, so the symbol
    /// is pinned to the middle of the viewport whatever the pose — which is
    /// what makes it read as "this is what I am turning around".
    #[test]
    fn the_pivot_sits_at_the_centre_of_the_viewport() {
        for camera in [
            Camera::default(),
            Camera {
                yaw: 2.4,
                pitch: -0.9,
                zoom: 0.6,
                pan: [0.3, -0.2],
            },
        ] {
            let symbol = pivot_symbol_geometry(&framing(&camera), CANVAS);
            let (min_x, max_x, min_y, max_y) = extent(&symbol.ball);
            let centre = [(min_x + max_x) * 0.5, (min_y + max_y) * 0.5];
            assert!(
                (centre[0] - CENTRE[0]).abs() < 0.01 && (centre[1] - CENTRE[1]).abs() < 0.01,
                "the ball landed at {centre:?}, not at the centre"
            );
        }
    }

    /// The symbol is sized in pixels, not in model units: zooming in moves the
    /// camera, and the rings follow it back out to the same size on screen.
    /// That is CloudCompare's `glScaled(symbolRadius * pixelSize)`.
    #[test]
    fn the_rings_keep_their_screen_size_at_any_zoom() {
        let widest = |zoom: f32| {
            let camera = Camera {
                zoom,
                ..Camera::default()
            };
            pivot_symbol_geometry(&framing(&camera), CANVAS)
                .lines
                .iter()
                .filter(|line| line.closed)
                .map(|line| {
                    let (min_x, max_x, min_y, max_y) = extent(&line.points);
                    (max_x - min_x).max(max_y - min_y)
                })
                .fold(0.0f32, f32::max)
        };
        let (near, far) = (widest(0.3), widest(4.0));
        assert!(
            (near - far).abs() < 1.0,
            "{near}px when zoomed in against {far}px when zoomed out"
        );
    }

    /// Looking straight down +Z (yaw and pitch both zero), which is the one
    /// pose where every claim can be checked exactly: the Z ring faces the
    /// viewer and is a true circle, the other two are edge-on and collapse
    /// onto an axis, and Z's own diameter collapses to a point.
    #[test]
    fn each_ring_is_seen_end_on_down_the_axis_that_colours_it() {
        let camera = Camera {
            yaw: 0.0,
            pitch: 0.0,
            ..Camera::default()
        };
        let symbol = pivot_symbol_geometry(&framing(&camera), CANVAS);
        let [x, y, z] = [0, 1, 2].map(|index| symbol.lines[index * 2].clone());

        // The ring spanning X and Y is square on the view: a circle of exactly
        // the symbol's radius, centred, in both directions.
        let (min_x, max_x, min_y, max_y) = extent(&z.points);
        assert!(
            (min_x - (CENTRE[0] - RING)).abs() < 0.01
                && (max_x - (CENTRE[0] + RING)).abs() < 0.01
                && (min_y - (CENTRE[1] - RING)).abs() < 0.01
                && (max_y - (CENTRE[1] + RING)).abs() < 0.01,
            "the Z ring spans {min_x}..{max_x} by {min_y}..{max_y}"
        );
        // The X ring spans Y and Z, and the eye sits on Z, so its plane holds
        // the line of sight: every point lands on the vertical centre line.
        assert!(
            x.points.iter().all(|p| (p[0] - CENTRE[0]).abs() < 0.01),
            "the edge-on X ring drifted off the centre line"
        );
        // Same argument for the Y ring, along the horizontal.
        assert!(
            y.points.iter().all(|p| (p[1] - CENTRE[1]).abs() < 0.01),
            "the edge-on Y ring drifted off the centre line"
        );

        // The diameters: X's is a full chord across the screen, Z's points at
        // the viewer and so has no length left.
        let (min_x, max_x, min_y, max_y) = extent(&symbol.lines[1].points);
        assert!(
            (min_x - (CENTRE[0] - RING)).abs() < 0.01
                && (max_x - (CENTRE[0] + RING)).abs() < 0.01
                && (min_y - CENTRE[1]).abs() < 0.01
                && (max_y - CENTRE[1]).abs() < 0.01,
            "the X diameter spans {min_x}..{max_x} by {min_y}..{max_y}"
        );
        let (min_x, max_x, min_y, max_y) = extent(&symbol.lines[5].points);
        assert!(
            max_x - min_x < 0.01 && max_y - min_y < 0.01,
            "the Z diameter should collapse to a point, spans {min_x}..{max_x} by {min_y}..{max_y}"
        );
    }

    /// An edge-on ring reaches *past* the symbol's radius, because the half of
    /// it that swings towards the camera is magnified on the way in. A flat
    /// orthographic squeeze — flattening the ring by hand instead of projecting
    /// it — would stop at exactly the radius, so this is the proof the ring
    /// vertices really go through the projection.
    #[test]
    fn an_edge_on_ring_is_magnified_by_the_camera() {
        let camera = Camera {
            yaw: 0.0,
            pitch: 0.0,
            ..Camera::default()
        };
        let symbol = pivot_symbol_geometry(&framing(&camera), CANVAS);
        // The X ring, a vertical line at this pose.
        let (_, _, min_y, max_y) = extent(&symbol.lines[0].points);
        let edge_on = (max_y - min_y) * 0.5;
        // The Z ring, square on the view and so at one depth throughout.
        let (_, _, min_y, max_y) = extent(&symbol.lines[4].points);
        let face_on = (max_y - min_y) * 0.5;
        assert!(
            edge_on > face_on * 1.02 && edge_on < face_on * 1.06,
            "{edge_on} against the {face_on} it would be without perspective"
        );
    }

    /// Panning slides the pivot and the eye together, so the symbol stays put
    /// on screen while the model moves under it — the rotation centre is where
    /// the camera is looking, which is the middle.
    #[test]
    fn a_pan_carries_the_pivot_with_the_camera() {
        let mut camera = Camera {
            yaw: 0.0,
            pitch: 0.0,
            ..Camera::default()
        };
        camera.pan_by([0.4, -0.25]);
        let symbol = pivot_symbol_geometry(&framing(&camera), CANVAS);
        let (min_x, max_x, min_y, max_y) = extent(&symbol.ball);
        assert!(
            (min_x + max_x - 2.0 * CENTRE[0]).abs() < 0.01
                && (min_y + max_y - 2.0 * CENTRE[1]).abs() < 0.01,
            "a panned pivot drifted off the centre"
        );
        // And the rings are undistorted by it: the Z ring is still circular.
        let (min_x, max_x, min_y, max_y) = extent(&symbol.lines[4].points);
        assert!(
            ((max_x - min_x) - (max_y - min_y)).abs() < 0.01,
            "the facing-the-viewer ring stopped being round: {min_x}..{max_x} by {min_y}..{max_y}"
        );
    }

    /// The colours come from the same constants the scene gizmo draws with, in
    /// X/Y/Z order, so the pivot cannot disagree about which axis is which.
    #[test]
    fn each_piece_wears_its_axis_colour() {
        use trove_core::media::render3d::{AXIS_X, AXIS_Y, AXIS_Z};
        for (index, color) in [AXIS_X, AXIS_Y, AXIS_Z].into_iter().enumerate() {
            assert_eq!(ring(index).color, color);
            assert_eq!(diameter(index).color, color);
        }
    }
}
