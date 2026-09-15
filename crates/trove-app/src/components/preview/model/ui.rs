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
                let nudge = |this: &mut Self, dx: f32, dy: f32| {
                    if pan {
                        // Panning pushes the *view*: the model slides the other
                        // way, which is what an arrow key does in every viewer.
                        this.camera.pan_by([dx * panstep, dy * panstep]);
                    } else {
                        // Turning follows the drag's convention instead, so an
                        // arrow turns the model the way it points.
                        this.camera.orbit(-dx * keystep, dy * keystep);
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
            // The corner trihedron is a coverage layer over the picture, so it
            // is painted after the frame and before the buttons — the controls
            // stay clickable where they overlap it.
            .child(self.corner_axis(cx))
            .child(self.axis_controls(cx))
            .child(self.shortcuts_hint(cx))
    }

    /// The axis switches, in the canvas's top-left corner: height colouring,
    /// the scene's X/Y/Z axes, and the corner trihedron.
    ///
    /// On the canvas rather than in the toolbar because each one changes what
    /// is drawn, not the panel's chrome. Each is a persisted toggle: flipping
    /// it redraws, and the frame loop's periodic config re-read agrees with
    /// what is on screen instead of flicking it back. The scene axes used to
    /// follow the height switch — that pairing is gone; the two now answer
    /// different questions (what the colours mean, and where the axes are).
    fn axis_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (height_on, scene_on, corner_on) = (
            self.height_color,
            self.show_scene_axes,
            self.show_corner_axis,
        );
        h_flex()
            .absolute()
            .top_2()
            .left_2()
            .gap_1()
            .child(
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
            .child(
                Button::new("scene-axes")
                    .xsmall()
                    .when(scene_on, |button| button.primary())
                    .when(!scene_on, |button| button.ghost())
                    .icon(gpui_kit::assets::IconName::Axis3d)
                    .label(rust_i18n::t!("viewport.scene_axes").to_string())
                    .tooltip(rust_i18n::t!("viewport.scene_axes_tip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_scene_axes = !this.show_scene_axes;
                        let mut config = trove_core::config::AppConfig::load();
                        config.scene_axes = Some(this.show_scene_axes);
                        let _ = config.save();
                        this.enhance_checked = None;
                        this.dirty = true;
                        this.pump(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("corner-axis")
                    .xsmall()
                    .when(corner_on, |button| button.primary())
                    .when(!corner_on, |button| button.ghost())
                    .icon(gpui_kit::assets::IconName::LocateFixed)
                    .label(rust_i18n::t!("viewport.corner_axis").to_string())
                    .tooltip(rust_i18n::t!("viewport.corner_axis_tip").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_corner_axis = !this.show_corner_axis;
                        let mut config = trove_core::config::AppConfig::load();
                        config.corner_axis = Some(this.show_corner_axis);
                        let _ = config.save();
                        // A pure UI-layer overlay: no frame to redraw, the
                        // render below is untouched.
                        cx.notify();
                    })),
            )
    }

    /// The corner trihedron: a small X/Y/Z axis indicator pinned to the
    /// viewport's bottom-right corner, turning with the camera —
    /// CloudCompare's `drawTrihedron`, minus the OpenGL.
    ///
    /// Painted as a UI layer over the rendered frame rather than inside it.
    /// One implementation then covers both renderers (GPU and CPU fallback),
    /// and the labels come out as real text at the window's own resolution
    /// instead of being baked into whatever resolution the frame happened to
    /// be drawn at. It also stays a pure overlay: the picture below is
    /// untouched, which is exactly the contract CloudCompare gets from
    /// clearing the depth buffer before it draws its own.
    fn corner_axis(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        if !self.show_corner_axis {
            return div().into_any_element();
        }
        let (width, height) = self.logical;
        // Not on screen yet, or nothing to relate the axes to: the geometry
        // below needs a framing, and a framing needs real bounds.
        if width <= 0.0 || height <= 0.0 || self.scene_bounds.is_empty() {
            return div().into_any_element();
        }
        let framing = self
            .camera
            .framing(self.scene_bounds, width / height.max(1.0));
        let (rods, labels) = corner_axis_geometry(&framing, (width, height));

        div()
            .absolute()
            .inset_0()
            .child(gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    // The rods go down in back-to-front order, which is what
                    // gives three flat quads the depth relationship a real
                    // 3D draw would get from its depth test.
                    for rod in &rods {
                        if let Some(path) = quad_path(bounds.origin, &rod.quad) {
                            window.paint_path(path, gpui::rgb(rod.color));
                        }
                    }
                },
            ))
            .children(labels.into_iter().map(|label| {
                // Half a `text_xs` letter each way: a fixed nudge standing in
                // for the font metrics CloudCompare measures, because a
                // single bold letter is close enough at this size.
                div()
                    .absolute()
                    .left(px(label.pos[0] - 4.0))
                    .top(px(label.pos[1] - 6.0))
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(gpui::rgb(label.color))
                    .child(label.letter)
            }))
            .into_any_element()
    }

    /// A small panel in the canvas's bottom-left corner listing the keyboard
    /// shortcuts. Bottom-left rather than the top corners, which belong to
    /// the axis switches and the trihedron.
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
            .bottom_2()
            .left_2()
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

    /// The toolbar: what the model is, how it is being drawn, and the way out.
    /// The viewport's title-bar controls: name, geometry stats, and the
    /// reset / close buttons.
    ///
    /// Rendered by the host panel's title bar while a model preview is open —
    /// see `WorkspacePanel::title_suffix` — so the canvas below is nothing but
    /// the picture. The title bar supplies the chrome, so this carries no
    /// padding or border of its own.
    ///
    /// The backend used to be listed here too. It now lives in the status bar
    /// (see `ModelViewport::backend_text`), which left this row with room for
    /// the tools that will come after it.
    pub(crate) fn title_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (primitives, vertices) = self.stats();
        let count_label = if self.mesh.is_point_cloud() {
            rust_i18n::t!("viewport.points", count = primitives)
        } else {
            rust_i18n::t!("viewport.triangles", count = primitives)
        };
        let frame_ms = self.frame_ms;

        // Deliberately *not* `w_full`: this renders in the panel's title bar,
        // which is a row shared with the title and the window controls. Asking
        // for the full width there pushes the row past the panel's edge; the
        // content sizes itself instead, and the name lives in the tab.
        h_flex()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} · {}",
                        count_label,
                        rust_i18n::t!("viewport.vertices", count = vertices)
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .when(frame_ms > 0.0, |element| {
                        // Two decimals: at 60 fps the number is small and the
                        // difference between 1.2 and 1.9 ms is what the user
                        // is watching.
                        element.child(format!("{frame_ms:.2} ms"))
                    }),
            )
            .when(self.samples() > 1, |element| {
                element.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{}×", self.samples())),
                )
            })
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
// Corner trihedron geometry
// ============================================================================

/// Length of one corner-axis rod, in logical pixels. Close to CloudCompare's
/// 25 px, so the proportion reads the same.
const CORNER_AXIS_LEN: f32 = 26.0;
/// Thickness of one rod, in logical pixels.
const CORNER_AXIS_WIDTH: f32 = 2.0;
/// Distance from the viewport's bottom-right corner to the trihedron's
/// origin, along both edges.
const CORNER_AXIS_MARGIN: f32 = 14.0;
/// How far past a rod's tip its letter sits.
const CORNER_AXIS_LABEL_GAP: f32 = 8.0;
/// A rod this short on screen is pointing at the viewer; its letter would
/// pile up on the trihedron's origin, so it is skipped.
const CORNER_AXIS_MIN_PROJECTION: f32 = 2.0;

/// One screen-space rod of the corner trihedron, ready to paint.
struct CornerRod {
    /// The rod as a closed quad — origin and tip, widened to
    /// [`CORNER_AXIS_WIDTH`] — in canvas-local pixels.
    quad: [[f32; 2]; 4],
    /// View-space depth: larger means closer to the eye. Used only to sort.
    depth: f32,
    /// Fill colour, packed `0xRRGGBB`.
    color: u32,
}

/// One X/Y/Z letter of the corner trihedron, positioned past its rod's tip.
struct CornerLabel {
    /// The letter itself.
    letter: &'static str,
    /// Centre of the letter, in canvas-local pixels.
    pos: [f32; 2],
    /// Letter colour, packed `0xRRGGBB`.
    color: u32,
}

/// The corner trihedron's geometry for one camera pose, anchored at the
/// viewport's bottom-right corner.
///
/// This is CloudCompare's `drawTrihedron` arithmetic with the OpenGL taken
/// out. Each world axis is projected onto the screen plane through the
/// camera's `right`/`up` basis — the same thing multiplying a world direction
/// by the view matrix's rotation does before an orthographic projection — so
/// a rod pointing at the viewer shortens towards a dot exactly as an
/// orthographic view of it would, and the trihedron reads as 3D without any
/// perspective maths. The view-space depth of each axis (its dot with the
/// `forward` basis) sorts the rods back to front, which is what stands in for
/// the depth buffer CloudCompare clears and tests: the rod in front covers
/// the ones behind it.
fn corner_axis_geometry(
    framing: &trove_core::media::render3d::Framing,
    canvas: (f32, f32),
) -> (Vec<CornerRod>, Vec<CornerLabel>) {
    use trove_core::media::render3d::{AXIS_X, AXIS_Y, AXIS_Z};

    let dot3 = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    // The origin sits a margin plus one rod's length inside the corner, so a
    // rod pointing straight down or right still ends on screen.
    let origin = [
        canvas.0 - CORNER_AXIS_MARGIN - CORNER_AXIS_LEN,
        canvas.1 - CORNER_AXIS_MARGIN - CORNER_AXIS_LEN,
    ];
    let mut rods = Vec::with_capacity(3);
    let mut labels = Vec::with_capacity(3);
    for (axis, color, letter) in [
        ([1.0, 0.0, 0.0], AXIS_X, "X"),
        ([0.0, 1.0, 0.0], AXIS_Y, "Y"),
        ([0.0, 0.0, 1.0], AXIS_Z, "Z"),
    ] {
        // Screen direction of the world axis. y is flipped because pixel y
        // grows downwards while view-space y grows upwards.
        let dir = [dot3(axis, framing.right), -dot3(axis, framing.up)];
        let length = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
        if length < CORNER_AXIS_MIN_PROJECTION {
            continue;
        }
        let tip = [
            origin[0] + dir[0] * CORNER_AXIS_LEN,
            origin[1] + dir[1] * CORNER_AXIS_LEN,
        ];
        // View-space z of the rod's tip relative to the trihedron's origin:
        // smaller is closer to the eye, which is the sort key.
        let depth = dot3(axis, framing.forward);
        // A unit normal across the rod, so it is a `CORNER_AXIS_WIDTH`-wide
        // quad instead of a hairline.
        let normal = [-dir[1] / length, dir[0] / length];
        let half = CORNER_AXIS_WIDTH * 0.5;
        rods.push(CornerRod {
            quad: [
                [origin[0] + normal[0] * half, origin[1] + normal[1] * half],
                [origin[0] - normal[0] * half, origin[1] - normal[1] * half],
                [tip[0] - normal[0] * half, tip[1] - normal[1] * half],
                [tip[0] + normal[0] * half, tip[1] + normal[1] * half],
            ],
            depth,
            color: axis_color(color),
        });
        let unit = [dir[0] / length, dir[1] / length];
        labels.push(CornerLabel {
            letter,
            pos: [
                tip[0] + unit[0] * CORNER_AXIS_LABEL_GAP,
                tip[1] + unit[1] * CORNER_AXIS_LABEL_GAP,
            ],
            color: axis_color(color),
        });
    }
    // Far first: painting near over far gives three flat quads the same
    // front-to-back look a depth-tested 3D draw has.
    rods.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    (rods, labels)
}

/// Pack a linear `render3d` axis colour as `0xRRGGBB`. The trihedron reads
/// its colours from the same constants the scene gizmo uses, so the two
/// cannot disagree about which axis is which.
fn axis_color(color: [f32; 3]) -> u32 {
    let to8 = |value: f32| (value * 255.0).round().clamp(0.0, 255.0) as u32;
    (to8(color[0]) << 16) | (to8(color[1]) << 8) | to8(color[2])
}

/// A closed quad through the four corners, offset into window coordinates.
/// `paint_path` fills it; `build` returns `None` when the platform refused
/// the path, in which case the rod is skipped for the frame.
fn quad_path(origin: Point<Pixels>, quad: &[[f32; 2]; 4]) -> Option<gpui::Path<Pixels>> {
    let mut builder = gpui::PathBuilder::fill();
    builder.move_to(origin + gpui::point(px(quad[0][0]), px(quad[0][1])));
    for corner in quad.iter().skip(1) {
        builder.line_to(origin + gpui::point(px(corner[0]), px(corner[1])));
    }
    builder.build().ok()
}
