//! The viewport's element tree: the canvas, the toolbar, and the two things
//! that stand in for a frame before one exists.

use std::sync::Arc;

use gpui_kit::base::{ElementExt as _, h_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Backend, Drag, ModelViewport, drag_for};

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
                this.camera.zoom_by((-lines * 0.12).exp());
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
                        this.camera.zoom_by(1.0 / zoom_factor);
                        this.dirty = true;
                    }
                    "q" => {
                        this.camera.zoom_by(zoom_factor);
                        this.dirty = true;
                    }
                    "=" | "+" => {
                        this.camera.zoom_by(1.0 / zoom_factor);
                        this.dirty = true;
                    }
                    "-" | "_" => {
                        this.camera.zoom_by(zoom_factor);
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
            .child(self.shortcuts_hint(cx))
    }

    /// A small panel in the top-right corner listing the keyboard shortcuts.
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

    /// The toolbar: what the model is, how it is being drawn, and the way out.
    pub(super) fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (primitives, vertices) = self.stats();
        let count_label = if self.mesh.is_point_cloud() {
            rust_i18n::t!("viewport.points", count = primitives)
        } else {
            rust_i18n::t!("viewport.triangles", count = primitives)
        };
        let backend = match &self.backend {
            Backend::Loading => rust_i18n::t!("viewport.backend_loading").to_string(),
            Backend::Starting => rust_i18n::t!("viewport.backend_starting").to_string(),
            Backend::Gpu(adapter) => {
                rust_i18n::t!("viewport.backend_gpu", adapter = adapter).to_string()
            }
            Backend::Cpu(reason) => {
                rust_i18n::t!("viewport.backend_cpu", reason = reason).to_string()
            }
            Backend::Streaming => {
                // Progress comes from the fields the background step updates:
                // the streamer itself is off-thread while a step is running.
                let loaded = self.stream_read;
                let total = self.stream_total;
                if total > 0 {
                    let pct = (loaded as f32 / total as f32 * 100.0) as u32;
                    // Progress is counted in points *read*: once the resident
                    // budget starts thinning the cloud, the kept count stops
                    // tracking the file.
                    rust_i18n::t!(
                        "viewport.backend_streaming",
                        percent = pct,
                        loaded = loaded,
                        total = total,
                        kept = self.stream_kept
                    )
                    .to_string()
                } else {
                    rust_i18n::t!("viewport.backend_streaming_starting").to_string()
                }
            }
            Backend::Indexed => rust_i18n::t!(
                "viewport.backend_indexed",
                chunks = self.index_chunks_read,
                total = self.index_chunks_total
            )
            .to_string(),
        };
        let frame_ms = self.frame_ms;

        h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(self.name.clone()),
            )
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
                div()
                    .flex_none()
                    .max_w(px(220.0))
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(backend),
            )
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
