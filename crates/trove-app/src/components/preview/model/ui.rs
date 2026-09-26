//! The viewport's element tree: the canvas, the toolbar, and the two things
//! that stand in for a frame before one exists.

use std::sync::Arc;

use gpui_kit::base::{Disableable as _, ElementExt as _, h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use gpui_kit::base::{ColorPickerEvent, ColorPickerState};
use gpui_kit::component::color_picker::ColorPicker;
use gpui_kit::component::scroll::ScrollableElement as _;
use trove_core::media::height_color::{
    COLOR_SCALES, ColorScale, ColorStop, CustomScale, Field, HeightField, HeightLook, HeightMode,
    MAX_PERIOD, MIN_PERIOD, RAMP_STOPS, Ramp, Scale, nice_ticks, scale_by_id,
};

use super::{Backend, Drag, ModelViewport, ModelViewportEvent, drag_for};

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

/// How many swatches a colour strip is drawn in: enough that a scale reads as
/// continuous, few enough that a panel is not laying out a hundred divs.
const STRIP_SEGMENTS: usize = 32;

/// The height of the legend's bar. The labels ride beside it, so this is the
/// only place the two halves of the legend have to agree.
const LEGEND_HEIGHT: Pixels = px(96.);
/// Intermediate labels to aim for on a field that names none of its own. Three
/// inside the ends is as many as a bar this short carries without crowding.
const LEGEND_TICKS: usize = 3;
/// Half a line of `text_xs`, for centring a label on its own value.
const LEGEND_TEXT_HALF: f32 = 7.0;
/// Closer together than this and two labels are one label: the later is dropped.
const LEGEND_LINE: f32 = 14.0;
/// Slots the anchor editor cuts a scale into. Finer than anyone can aim at on a
/// bar this short, coarse enough that a scale's anchors land on positions the
/// strip can actually show.
const ANCHOR_CELLS: usize = 24;

/// The caption above a row of the height panel.
fn section_label(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A colour scale drawn as a horizontal strip, so the thing being chosen is the
/// thing being seen rather than a name that has to be imagined in colour.
fn scale_strip(scale: &Scale, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_shrink_0()
        .w_24()
        .h_4()
        .overflow_hidden()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .children(scale.gradient(STRIP_SEGMENTS).into_iter().map(|rgb| {
            div().flex_1().h_full().bg(tint_color([
                rgb[0] as f32 / 255.0,
                rgb[1] as f32 / 255.0,
                rgb[2] as f32 / 255.0,
            ]))
        }))
}

/// A scale stop, as a UI colour. Shared with the two picker states, which hold
/// their colours the way the theme does.
pub(super) fn rgb_hsla(rgb: [u8; 3]) -> Hsla {
    tint_color([
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
    ])
}

/// The other way round, for a colour the picker just committed. Alpha is
/// dropped: a scale stop is opaque, and the model behind it is not a layer.
pub(super) fn hsla_rgb(color: Hsla) -> [u8; 3] {
    let rgba = Rgba::from(color);
    [
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    ]
}

/// One of the colours a field look paints with — the same 0..=1 floats both
/// renderers agree on — as a UI colour.
fn tint_color(rgb: [f32; 3]) -> Hsla {
    Rgba {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
        a: 1.0,
    }
    .into()
}

/// The two ends of the bar, plus whatever the field calls a label.
///
/// Positions run from 0.0 at the bottom of the range to 1.0 at the top, because
/// that is the order the strip is painted in.
fn legend_values(field: &HeightField) -> Vec<(f32, f32)> {
    let (min, max) = field.range();
    let span = max - min;
    if span <= 0.0 {
        return vec![(min, 0.0)];
    }
    let position = |value: f32| (value - min) / span;
    // The field's own labels where it has them — CloudCompare stores them on the
    // scale — and round numbers over the model's range where it does not.
    let interior: Vec<f32> = match field.field().labels() {
        Some(values) => values
            .iter()
            .filter(|value| **value > min && **value < max)
            .copied()
            .collect(),
        // A class is a whole number, so a half-class step is not a label: a
        // two-class cloud's range is 0 to 2, and the natural tick falls at half.
        // What survives the filter is the whole part of it.
        None if field.field().is_integral() => nice_ticks(min, max, LEGEND_TICKS)
            .into_iter()
            .filter(|value| (value - value.round()).abs() < 1e-3)
            .collect(),
        None => nice_ticks(min, max, LEGEND_TICKS),
    };
    // The last class is one short of the palette's count, and no point is ever
    // painted past it, so that is the number the top of the bar carries.
    let top = if field.field().is_integral() {
        max - 1.0
    } else {
        max
    };
    let mut values = vec![(min, 0.0)];
    values.extend(interior.into_iter().map(|value| (value, position(value))));
    values.push((top, 1.0));
    values
}

/// What a field is called, in the user's language.
fn field_name(field: Field) -> String {
    match field {
        Field::Height => rust_i18n::t!("viewport.height_field_height").to_string(),
        Field::Slope => rust_i18n::t!("viewport.height_field_slope").to_string(),
        Field::Aspect => rust_i18n::t!("viewport.height_field_aspect").to_string(),
        Field::Intensity => rust_i18n::t!("viewport.height_field_intensity").to_string(),
        Field::Class => rust_i18n::t!("viewport.height_field_class").to_string(),
    }
}

/// The bar's caption: the field's name, with the axis appended for the one
/// field whose meaning depends on it.
fn legend_caption(field: &HeightField) -> String {
    let name = field_name(field.field());
    match field.field() {
        Field::Height => format!("{} · {}", name, ["X", "Y", "Z"][field.axis()]),
        _ => name,
    }
}

/// The labels beside the bar, each at the height of its own value.
///
/// Positioned by fraction rather than laid out, because a label belongs to a
/// value and not to a row: three labels in a range that only fills half the bar
/// sit in the bottom half, which is the whole point. Neighbours closer than a
/// line of `text_xs` would drop the later one rather than overlap it.
fn legend_labels(field: &HeightField, label: &dyn Fn(f32) -> String, cx: &App) -> AnyElement {
    let mut column = div().relative().w(px(52.)).h(LEGEND_HEIGHT);
    // Walking top down, so each label is measured against the last one kept.
    let mut last_top = f32::NEG_INFINITY;
    for (value, position) in legend_values(field).into_iter().rev() {
        let top = LEGEND_HEIGHT.as_f32() * (1.0 - position) - LEGEND_TEXT_HALF;
        if top - last_top < LEGEND_LINE {
            continue;
        }
        last_top = top;
        column = column.child(
            div()
                .absolute()
                .left_0()
                .top(px(top))
                .flex()
                .items_center()
                .gap_1()
                // The tick itself, so the number points at something.
                .child(div().w_1().h(px(7.)).flex_shrink_0().bg(cx.theme().border))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label(value)),
                ),
        );
    }
    column.into_any_element()
}

/// A number in whatever units the field is measured in, with no more decimals
/// than it needs: the legend's two values and the banding period read this way.
fn format_units(value: f32) -> String {
    format!("{value:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
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
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .child(self.height_control(cx)),
            )
            .child(self.height_legend(cx))
            .child(self.shortcuts_hint(cx))
    }

    /// The one look switch, in the canvas's top-left corner: how — or whether
    /// — the model is painted by height.
    ///
    /// A popover, not a settings row, because the choice is made while looking
    /// at the model: CloudCompare asks the same four questions in its
    /// `ccColorGradientDlg` (direction, ramp, banding, frequency) and so does
    /// this panel, with the ramp previewed where its name would be. The panel
    /// itself only ever offers ways of painting; switching off is the × beside
    /// the button, which appears while the colouring is on — the same split as
    /// a chip that carries its own untag.
    ///
    /// On the canvas rather than in the toolbar because it changes what is
    /// drawn, not the panel's chrome. Every choice persists as it is made, so
    /// the next model opened looks the same way.
    fn height_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = self.height.mode != HeightMode::Off;
        h_flex()
            .gap_1()
            .items_center()
            .child(
                Popover::new("height-color-popover")
                    .trigger(
                        Button::new("height-color")
                            .xsmall()
                            .when(on, |button| button.primary())
                            .when(!on, |button| button.ghost())
                            .icon(IconName::Palette)
                            .label(rust_i18n::t!("viewport.height_color").to_string())
                            .tooltip(rust_i18n::t!("viewport.height_color_tip").to_string()),
                    )
                    .child(height_panel(self, cx.entity(), cx)),
            )
            .when(on, |row| {
                row.child(
                    Button::new("height-color-off")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(rust_i18n::t!("viewport.height_color_off").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.apply_height(
                                HeightLook {
                                    mode: HeightMode::Off,
                                    ..this.height.clone()
                                },
                                true,
                                cx,
                            );
                        })),
                )
            })
            .into_any_element()
    }

    /// The anchor picker moved: recolour the anchor it is editing, and leave the
    /// write to the close edge.
    pub(super) fn on_height_colour(
        &mut self,
        _: Entity<ColorPickerState>,
        event: &ColorPickerEvent,
        cx: &mut Context<Self>,
    ) {
        let ColorPickerEvent::Change(color) = event;
        let Some(color) = color else {
            return;
        };
        self.paint_anchor(hsla_rgb(*color), false, cx);
    }

    /// The selected anchor's colour, as a new look.
    pub(super) fn paint_anchor(&mut self, rgb: [u8; 3], persist: bool, cx: &mut Context<Self>) {
        let Some(mut stops) = self.custom_stops() else {
            return;
        };
        let index = self.height_anchor.min(stops.len().saturating_sub(1));
        stops[index].rgb = rgb;
        self.stow_scale(stops, persist, cx);
    }

    /// Click a cell of the editor strip: an anchor already there is selected,
    /// and an empty cell takes a new one, coloured from the ramp at that point
    /// so it starts invisible against its neighbours rather than as a hard step.
    pub(super) fn click_anchor_cell(&mut self, cell: usize, cx: &mut Context<Self>) {
        let Some(stops) = self.custom_stops() else {
            return;
        };
        let Some(live) = anchor_at(&stops, cell) else {
            if stops.len() >= RAMP_STOPS {
                return;
            }
            let at = cell_position(cell);
            let rgb = ramp_rgb(&stops, at);
            let mut stops = stops;
            stops.push(ColorStop::new(at, rgb));
            self.height_anchor = anchor_at(&stops_after_sort(&stops), cell).unwrap_or(0);
            self.stow_scale(stops, true, cx);
            return;
        };
        self.height_anchor = live;
        self.redraw(cx);
    }

    /// Slide the selected anchor one cell along the scale.
    pub(super) fn nudge_anchor(&mut self, cells: i32, cx: &mut Context<Self>) {
        let Some(stops) = self.custom_stops() else {
            return;
        };
        let Some(index) = stops.get(self.height_anchor).map(|_| self.height_anchor) else {
            return;
        };
        let at = (stops[index].at + cells as f32 / ANCHOR_CELLS as f32).clamp(0.0, 1.0);
        let mut stops = stops;
        stops[index].at = at;
        // The ends are the ends: moving an anchor off one would silently shorten
        // the scale's own range, which is what the sorted-then-pinned cleanup in
        // `Ramp::custom` exists to refuse.
        if at == 0.0 {
            self.height_anchor = 0;
        } else if at == 1.0 {
            self.height_anchor = stops.len() - 1;
        }
        self.stow_scale(stops, true, cx);
    }

    /// Drop the selected anchor, as long as two are left to interpolate with.
    pub(super) fn drop_anchor(&mut self, cx: &mut Context<Self>) {
        let Some(stops) = self.custom_stops() else {
            return;
        };
        if stops.len() <= 2 {
            return;
        }
        let mut stops = stops;
        stops.remove(self.height_anchor.min(stops.len() - 1));
        self.height_anchor = self.height_anchor.min(stops.len() - 1);
        self.stow_scale(stops, true, cx);
    }

    /// A new user scale, made current — which also switches the mode to the ramp
    /// that uses it, the way choosing a scale does.
    pub(super) fn add_custom_scale(&mut self, cx: &mut Context<Self>) {
        let mut config = trove_core::config::AppConfig::load();
        let id = config.add_custom_scale();
        let _ = config.save();
        self.height_scales = config.height_custom_scales;
        let ramp = self
            .height_scales
            .iter()
            .find(|custom| custom.id == id)
            .map(CustomScale::ramp)
            .unwrap_or_else(Ramp::defaults);
        self.height_anchor = 0;
        self.set_height(
            HeightLook {
                mode: HeightMode::Ramp,
                scale: Scale::Custom {
                    id,
                    ramp: Box::new(ramp),
                },
                ..self.height.clone()
            },
            cx,
        );
    }

    /// Forget the scale in use, and hand the model back the default. Its anchors
    /// may be named by another model's row; those read the fallback too, which is
    /// the trade the reference-and-nothing-else storage made on purpose.
    pub(super) fn delete_custom_scale(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.height.scale.custom_id().map(str::to_string) else {
            return;
        };
        let mut config = trove_core::config::AppConfig::load();
        config.remove_custom_scale(&id);
        let _ = config.save();
        self.height_scales = config.height_custom_scales.clone();
        self.set_height(
            HeightLook {
                scale: config.height_look().scale,
                ..self.height.clone()
            },
            cx,
        );
    }

    /// Choose one of the user's scales by id, from the list the viewport caches.
    ///
    /// By id rather than by value because the value lives in that list: picking
    /// a row picks what the config currently holds for it, which is what keeps
    /// the row's strip and the model's colours from disagreeing after an edit.
    fn use_custom_scale(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(custom) = self.height_scales.iter().find(|custom| custom.id == id) else {
            return;
        };
        let scale = Scale::custom(custom);
        self.height_anchor = 0;
        self.set_height(
            HeightLook {
                mode: HeightMode::Ramp,
                scale,
                ..self.height.clone()
            },
            cx,
        );
    }

    /// The anchors of the scale in use, when it is a user's own.
    pub(super) fn custom_stops(&self) -> Option<Vec<ColorStop>> {
        match &self.height.scale {
            Scale::Custom { ramp, .. } => Some(ramp.stops().to_vec()),
            Scale::Preset(_) => None,
        }
    }

    /// Rebuild the scale from an edited anchor list and apply it.
    ///
    /// `persist` is false only for a colour drag; every anchor the strip adds,
    /// moves or deletes is a finished decision and is written at once.
    pub(super) fn stow_scale(
        &mut self,
        stops: Vec<ColorStop>,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        let Scale::Custom { id, .. } = &self.height.scale else {
            return;
        };
        let id = id.clone();
        let ramp = Ramp::custom(&stops, false);
        self.apply_height(
            HeightLook {
                scale: Scale::Custom {
                    id,
                    ramp: Box::new(ramp),
                },
                ..self.height.clone()
            },
            persist,
            cx,
        );
    }

    /// Load the picker up with the selected anchor's colour, so opening it shows
    /// what clicking it will change.
    pub(super) fn sync_anchor_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rgb) = self
            .custom_stops()
            .and_then(|stops| stops.get(self.height_anchor).map(|stop| stop.rgb))
        else {
            return;
        };
        self.height_colour.update(cx, |state, cx| {
            state.set_value(rgb_hsla(rgb), window, cx);
        });
    }

    /// Write a look and persist it: a click that chose it is done choosing.
    pub(super) fn set_height(&mut self, look: HeightLook, cx: &mut Context<Self>) {
        self.apply_height(look, true, cx);
    }

    /// Draw `look`, and — when `persist` — write it to the config, to the asset's
    /// own row through the host, and to the cached scale list.
    ///
    /// The split exists because one control reports a value on every frame of a
    /// drag: the picker's own sliders. The model should follow the drag, but the
    /// library should be written once, when the pointer lets go — which is the
    /// same lesson the workspace's colour filter records.
    pub(super) fn apply_height(&mut self, look: HeightLook, persist: bool, cx: &mut Context<Self>) {
        self.height = look;
        if !persist {
            self.height_unsaved = true;
            self.redraw(cx);
            return;
        }
        self.write_height(cx);
        self.redraw(cx);
    }

    /// Persist the look that is on screen. Also where a staged colour stops
    /// being staged.
    pub(super) fn write_height(&mut self, cx: &mut Context<Self>) {
        self.height_unsaved = false;
        let mut config = trove_core::config::AppConfig::load();
        config.set_height_look(self.height.clone());
        let _ = config.save();
        // The panel's rows come from this list, so a scale just edited is read
        // back from exactly what was written.
        self.height_scales = config.height_custom_scales;
        // And this model now has a look of its own. The config write is the
        // default for models that do not; both happen, because a change here is
        // both a change to *this* model and what an untuned one starts from —
        // which is how CloudCompare's last-used parameters already behave.
        if self.asset.is_some() {
            cx.emit(ModelViewportEvent::LookChanged(self.height.stored()));
        }
        // Say the config poll ran *after* that save, so the frame loop picks up
        // what was just written rather than what was on disk when the model was
        // opened.
        self.enhance_checked = Some(std::time::Instant::now());
    }

    /// Draw the frame again after a look change, touching no stored value.
    pub(super) fn redraw(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.pump(cx);
        cx.notify();
    }

    /// The user's scales the panel lists.
    pub(super) fn height_scales(&self) -> &[CustomScale] {
        &self.height_scales
    }

    /// The colour scale's legend, in the canvas's bottom-left corner: what is
    /// painted, the strip it is painted with, and the values the strip runs
    /// between.
    ///
    /// CloudCompare's scalar-field colour bar, and the reason any of this is
    /// worth switching on: a ramp without its range is decoration. The labels
    /// are the bar's other half, so they come from the field itself — a dip
    /// reads 0 / 30 / 60 / 90 as CloudCompare's own `customLabels` say, and a
    /// height gets round numbers out of the model's range rather than its
    /// fourths.
    fn height_legend(&self, cx: &mut Context<Self>) -> AnyElement {
        let field = self.height_field();
        if field.mode() == HeightMode::Off || self.scene_bounds.is_empty() {
            return div().into_any_element();
        }
        // The legend says what the picture means, so it says it in the field's
        // own units: degrees for a dip, the file's units for a height.
        let unit = field.unit().unwrap_or("");
        let label = |value: f32| format!("{}{}", format_units(value), unit);
        h_flex()
            .absolute()
            .bottom_2()
            .left_2()
            .gap_1p5()
            .items_start()
            .child(
                v_flex()
                    .gap_1()
                    // Which field is being painted, and along which axis when
                    // that is what the field asks: with the panel closed, this
                    // is the only place either is visible.
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(legend_caption(&field)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_start()
                            // Sampled through the field itself, so what the bar
                            // shows is what the model was painted with —
                            // including the bands it landed on.
                            .child(
                                div()
                                    .w_3()
                                    .h(LEGEND_HEIGHT)
                                    .flex_shrink_0()
                                    .flex()
                                    .flex_col()
                                    .overflow_hidden()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .children(
                                        field
                                            .legend_steps(STRIP_SEGMENTS)
                                            .into_iter()
                                            .map(|rgb| div().flex_1().w_full().bg(tint_color(rgb))),
                                    ),
                            )
                            .child(legend_labels(&field, &label, cx)),
                    ),
            )
            .into_any_element()
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

/// The popover's body: the mode, then the knobs that mode reads.
///
/// Free functions rather than methods, because the elements they build must not
/// hold a borrow of the render context: the viewport is captured by handle and
/// updated when a choice is clicked. Reads run the other way — they come from
/// the `&ModelViewport` the caller already holds, never through the handle,
/// because render itself runs inside the entity's update lease and one
/// `entity.read(cx)` here double-leases the viewport and panics.
fn height_panel(viewport: &ModelViewport, entity: Entity<ModelViewport>, cx: &App) -> AnyElement {
    let look = &viewport.height;
    v_flex()
        .w(px(280.))
        .gap_3()
        .child(mode_row(look, entity.clone()))
        .when(look.mode != HeightMode::Off, |panel| {
            panel
                .child(field_row(look, viewport, entity.clone(), cx))
                .child(match look.mode {
                    HeightMode::Ramp => scale_list(look, viewport, entity.clone(), cx),
                    HeightMode::Bands => band_period_row(look, entity.clone(), cx),
                    HeightMode::Off => div().into_any_element(),
                })
                .when(
                    look.mode == HeightMode::Ramp && look.scale.custom_id().is_some(),
                    |panel| panel.child(anchor_editor(look, viewport, entity.clone(), cx)),
                )
                .child(Separator::horizontal())
                .child(axis_row(look, entity.clone(), cx))
        })
        .into_any_element()
}

/// 高度 / 坡度 / 坡向 / 强度 / 分类: which per-point value the colour runs along.
///
/// CloudCompare keeps this question in the scalar-field display rather than in
/// `ccColorGradientDlg`, where only the dimension is asked. The first three read
/// the geometry, so a model always has them; the last two read a channel the
/// file carried or did not, and a missing one is shown disabled rather than
/// hidden — a control that comes and going reads as a bug, and the reason is
/// worth the one line of copy.
fn field_row(
    look: &HeightLook,
    viewport: &ModelViewport,
    entity: Entity<ModelViewport>,
    cx: &App,
) -> AnyElement {
    // No caption for the row itself: the five names are the whole question, and
    // the segmented control already reads as a choice between them.
    let available = |field: Field| viewport.field_available(field);
    // In `Field::index` order, which is how the click reads the position back.
    let fields = [
        Field::Height,
        Field::Slope,
        Field::Aspect,
        Field::Intensity,
        Field::Class,
    ];
    let enabled: Vec<Field> = fields
        .iter()
        .copied()
        .filter(|field| available(*field))
        .collect();
    let mut row = TabBar::new("height-field").segmented().on_click({
        let entity = entity.clone();
        move |index: &usize, _, cx| {
            let field = Field::from_index(*index as u8);
            // A dimmed tab is still clickable in gpui-kit's segmented bar, so
            // the guard belongs here as much as in the styling.
            if !enabled.contains(&field) {
                return;
            }
            change_height(
                &entity,
                move |look| HeightLook {
                    field,
                    // A class is a name, so the class palette is what the field asks
                    // for the first time it is picked; a scale chosen on purpose —
                    // the user's own, or any other preset — stays where it was.
                    scale: if field == Field::Class
                        && matches!(&look.scale, Scale::Preset(preset) if preset.id == "bgyr")
                    {
                        Scale::Preset(scale_by_id("asprs"))
                    } else {
                        look.scale.clone()
                    },
                    ..look
                },
                cx,
            );
        }
    });
    for field in fields {
        let tab = Tab::new().flex_1().label(field_name(field));
        row = row.child(if available(field) {
            tab
        } else {
            tab.disabled(true)
        });
    }
    v_flex()
        .gap_1()
        .child(row.selected_index(look.field.index() as usize))
        .when(!available(look.field), |row| {
            row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        rust_i18n::t!(
                            "viewport.height_field_unavailable",
                            field = field_name(look.field)
                        )
                        .to_string(),
                    ),
            )
        })
        .into_any_element()
}

/// Write a new look through the viewport, the one way every row changes it.
fn change_height(
    entity: &Entity<ModelViewport>,
    change: impl FnOnce(HeightLook) -> HeightLook,
    cx: &mut App,
) {
    entity.update(cx, |this, cx| {
        let look = change(this.height.clone());
        this.set_height(look, cx);
    });
}

/// 渐变 / 分带: how the scale is laid over the field.
///
/// CloudCompare asks the same question with radio buttons in
/// `ccColorGradientDlg`. Its fourth button — off — is not a way of painting,
/// so it is not offered here either: turning the colouring off is the × beside
/// the palette button, and while the model is uncoloured the panel opens on
/// the two real choices with neither one lit.
fn mode_row(look: &HeightLook, entity: Entity<ModelViewport>) -> AnyElement {
    // Panel indices, not [`HeightMode::index`] — that one puts `Off` first,
    // and `Off` is no longer a choice the panel lists.
    let panel_index = |mode: HeightMode| match mode {
        HeightMode::Bands => 1,
        _ => 0,
    };
    let bar = TabBar::new("height-mode")
        .segmented()
        .on_click(move |index: &usize, _, cx| {
            let mode = match *index {
                1 => HeightMode::Bands,
                _ => HeightMode::Ramp,
            };
            change_height(&entity, move |look| HeightLook { mode, ..look }, cx);
        })
        .child(
            Tab::new()
                .flex_1()
                .label(rust_i18n::t!("viewport.height_ramp")),
        )
        .child(
            Tab::new()
                .flex_1()
                .label(rust_i18n::t!("viewport.height_bands")),
        );
    let mut bar = bar;
    if look.mode != HeightMode::Off {
        bar = bar.selected_index(panel_index(look.mode));
    }
    bar.into_any_element()
}

/// The scale list: every built-in strip, then the custom pair. Each row is the
/// strip first and its name second, because the strip is what the choice is.
///
/// The list scrolls rather than the scales being cut: sixteen of them will not
/// fit above a preview the height of a laptop window, and a scale whose colours
/// you cannot see is not a choice. CloudCompare answers this with a dropdown
/// selector; here the strips are the point, so they stay on screen.
fn scale_list(
    look: &HeightLook,
    viewport: &ModelViewport,
    entity: Entity<ModelViewport>,
    cx: &App,
) -> AnyElement {
    let mut rows = COLOR_SCALES
        .iter()
        .map(|scale| scale_row(scale, scale.id == look.scale.key(), entity.clone(), cx))
        .collect::<Vec<_>>();
    for (number, custom) in viewport.height_scales().iter().enumerate() {
        rows.push(custom_row(custom, number + 1, look, entity.clone(), cx));
    }
    rows.push(new_scale_button(entity.clone(), cx));
    v_flex()
        .gap_1()
        .child(section_label(rust_i18n::t!("viewport.height_scale"), cx))
        .child(
            v_flex()
                .gap_1()
                .max_h(px(220.))
                .overflow_y_scrollbar()
                .pr_1()
                .children(rows),
        )
        .into_any_element()
}

/// One row of the scale list: the strip, then its name.
fn scale_row(
    preset: &'static ColorScale,
    selected: bool,
    entity: Entity<ModelViewport>,
    cx: &App,
) -> AnyElement {
    let scale = Scale::Preset(preset);
    div()
        .id(SharedString::from(preset.id))
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .py_0p5()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .when(selected, |row| row.bg(cx.theme().accent))
        .child(scale_strip(&scale, cx))
        .child(scale_name(
            rust_i18n::t!(format!("viewport.height_scale_{}", preset.name_key)).into(),
            selected,
            cx,
        ))
        .on_click(move |_, _, cx| {
            change_height(
                &entity,
                move |look| HeightLook {
                    mode: HeightMode::Ramp,
                    // Rebuilt here rather than captured: a `Scale` carries an
                    // id once it is a user's own, so it is not `Copy`.
                    scale: Scale::Preset(preset),
                    ..look
                },
                cx,
            );
        })
        .into_any_element()
}

/// One of the user's scales in the list: the strip it paints, its number, and
/// the anchor markers along it — so the row says at a glance how many anchors
/// this one has, which is the only difference a list of strips otherwise hides.
fn custom_row(
    custom: &CustomScale,
    number: usize,
    look: &HeightLook,
    entity: Entity<ModelViewport>,
    cx: &App,
) -> AnyElement {
    let scale = Scale::custom(custom);
    let selected = scale.key() == look.scale.key();
    let id = custom.id.clone();
    div()
        .id(SharedString::from(format!("height-scale-{}", custom.id)))
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .py_0p5()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .when(selected, |row| row.bg(cx.theme().accent))
        .child(scale_strip(&scale, cx))
        .child(scale_name(
            format!(
                "{} {}",
                rust_i18n::t!("viewport.height_scale_custom"),
                number
            )
            .into(),
            selected,
            cx,
        ))
        .on_click(tap(&entity, move |this, cx| {
            this.use_custom_scale(&id, cx);
        }))
        .into_any_element()
}

/// The row that ends the list: start a scale from nothing.
fn new_scale_button(entity: Entity<ModelViewport>, cx: &App) -> AnyElement {
    div()
        .id("height-scale-new")
        .w_full()
        .px_1()
        .py_0p5()
        .rounded(cx.theme().radius)
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .child(rust_i18n::t!("viewport.height_scale_new").to_string())
        .on_click(tap(&entity, |this, cx| this.add_custom_scale(cx)))
        .into_any_element()
}

/// The anchor editor, under the row of the scale being edited.
///
/// Every control here is a click rather than a drag, and that is what keeps the
/// library quiet: a dragged handle would want a write per frame, and a
/// `SliderState` would want one more entity to keep in step with the config
/// poll. So the strip is cut into `ANCHOR_CELLS` slots — click an empty one to
/// drop an anchor there, click a marked one to select it, and step it along with
/// the arrows. CloudCompare drags its anchors; this picks them, at a resolution
/// finer than anyone reads off a 96-pixel bar.
fn anchor_editor(
    look: &HeightLook,
    viewport: &ModelViewport,
    entity: Entity<ModelViewport>,
    cx: &App,
) -> AnyElement {
    let stops = look.scale.stops();
    let selected = viewport.height_anchor_index();
    let anchor = selected.min(stops.len().saturating_sub(1));
    let cell = stops
        .get(anchor)
        .map(|stop| cell_index(stop.at))
        .unwrap_or(0);
    v_flex()
        .gap_1()
        .child(section_label(rust_i18n::t!("viewport.height_anchors"), cx))
        .child(h_flex().w_full().children((0..ANCHOR_CELLS).map(|index| {
            let here = anchor_at(&stops, index);
            let rgb = ramp_rgb(&stops, cell_position(index));
            div()
                .id(SharedString::from(format!("height-anchor-{index}")))
                .flex_1()
                .h(px(22.))
                .bg(tint_color([
                    rgb[0] as f32 / 255.0,
                    rgb[1] as f32 / 255.0,
                    rgb[2] as f32 / 255.0,
                ]))
                .cursor_pointer()
                // The marker is a dot rather than an outline: a cell is
                // a few pixels wide, and a border there reads as part of
                // the colour it is meant to point at.
                .when(here.is_some(), |slot| {
                    slot.flex().items_center().justify_center().child(
                        div()
                            .size_2()
                            .rounded_full()
                            .flex_shrink_0()
                            .border_2()
                            .border_color(if here == Some(anchor) {
                                cx.theme().accent
                            } else {
                                cx.theme().muted_foreground
                            })
                            .bg(tint_color([
                                rgb[0] as f32 / 255.0,
                                rgb[1] as f32 / 255.0,
                                rgb[2] as f32 / 255.0,
                            ])),
                    )
                })
                .on_click(tap(&entity, move |this, cx| {
                    this.click_anchor_cell(index, cx);
                }))
        })))
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    ColorPicker::new(&viewport.height_colour_picker())
                        .xsmall()
                        .label(rust_i18n::t!("viewport.height_anchor_colour")),
                )
                .child(
                    Button::new("height-anchor-left")
                        .xsmall()
                        .ghost()
                        .label("◀")
                        .disabled(cell == 0)
                        .on_click(tap(&entity, |this, cx| this.nudge_anchor(-1, cx))),
                )
                .child(
                    Button::new("height-anchor-right")
                        .xsmall()
                        .ghost()
                        .label("▶")
                        .disabled(cell == ANCHOR_CELLS - 1)
                        .on_click(tap(&entity, |this, cx| this.nudge_anchor(1, cx))),
                )
                .child(
                    Button::new("height-anchor-drop")
                        .xsmall()
                        .ghost()
                        .label(rust_i18n::t!("viewport.height_anchor_remove"))
                        .disabled(stops.len() <= 2)
                        .on_click(tap(&entity, |this, cx| this.drop_anchor(cx))),
                )
                .child(div().flex_1())
                .child(
                    Button::new("height-scale-delete")
                        .xsmall()
                        .ghost()
                        .label(rust_i18n::t!("viewport.height_scale_delete"))
                        .on_click(tap(&entity, |this, cx| this.delete_custom_scale(cx))),
                ),
        )
        .into_any_element()
}

/// A click handler that runs one edit through the viewport.
///
/// Every control in the panel needs the same three steps — take the handle,
/// update the entity, notify — and a `tap` per button keeps the `entity.clone()`
/// that each `move` closure needs out of the layout code.
/// A click handler aimed at one edit of the viewport.
type PanelClick = dyn Fn(&ClickEvent, &mut Window, &mut App);

fn tap(
    entity: &Entity<ModelViewport>,
    edit: impl Fn(&mut ModelViewport, &mut Context<ModelViewport>) + 'static,
) -> Box<PanelClick> {
    let entity = entity.clone();
    // `update` wants a `FnOnce`; `edit` is a plain `Fn`, so the wrapper calls
    // through it rather than moving it in.
    Box::new(move |_, _, cx| {
        let edit = &edit;
        entity.update(cx, |this, cx| edit(this, cx))
    })
}

/// Which anchor, if any, sits in this slot.
fn anchor_at(stops: &[ColorStop], cell: usize) -> Option<usize> {
    stops.iter().position(|stop| cell_index(stop.at) == cell)
}

/// The slot a position falls in, with the ends folding into the outer ones.
fn cell_index(at: f32) -> usize {
    ((at.clamp(0.0, 1.0) * ANCHOR_CELLS as f32) as usize).min(ANCHOR_CELLS - 1)
}

/// The position a slot stands for: its middle.
fn cell_position(cell: usize) -> f32 {
    (cell as f32 + 0.5) / ANCHOR_CELLS as f32
}

/// The colour a new anchor takes when it is dropped into a scale: what the scale
/// already says at that point, so adding one does not change the picture until
/// its colour is moved too.
fn ramp_rgb(stops: &[ColorStop], at: f32) -> [u8; 3] {
    let rgb = Ramp::custom(stops, false).color_at(at);
    [
        (rgb[0] * 255.0).round() as u8,
        (rgb[1] * 255.0).round() as u8,
        (rgb[2] * 255.0).round() as u8,
    ]
}

/// The anchors in the order `Ramp::custom` will put them, for the caller that
/// has just appended one and needs to know where it landed.
fn stops_after_sort(stops: &[ColorStop]) -> Vec<ColorStop> {
    Ramp::custom(stops, false).stops().to_vec()
}

/// The label of a scale row: the strip is the choice, the name only a caption.
fn scale_name(name: SharedString, selected: bool, cx: &App) -> impl IntoElement {
    div()
        .text_sm()
        .text_color(if selected {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        })
        .child(name)
}

/// Which axis the field is read against. Three models in one library can
/// disagree about it — a scanner writes Z up, a game engine Y — so it is asked,
/// not assumed.
fn axis_row(look: &HeightLook, entity: Entity<ModelViewport>, cx: &App) -> AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        // The same control answers a different question per field: where the
        // elevations sit, versus what counts as up.
        .child(section_label(
            rust_i18n::t!(if look.field == Field::Height {
                "viewport.height_axis"
            } else {
                "viewport.reference_axis"
            }),
            cx,
        ))
        .child(
            h_flex()
                .gap_1()
                .flex_1()
                .justify_end()
                .child(axis_button(0, "X", look, entity.clone()))
                .child(axis_button(1, "Y", look, entity.clone()))
                .child(axis_button(2, "Z", look, entity)),
        )
        .into_any_element()
}

fn axis_button(
    axis: usize,
    name: &'static str,
    look: &HeightLook,
    entity: Entity<ModelViewport>,
) -> AnyElement {
    Button::new(SharedString::from(format!("height-axis-{name}")))
        .xsmall()
        .w(px(28.))
        .justify_center()
        .when(axis == look.axis, |button| button.primary())
        .when(axis != look.axis, |button| button.ghost())
        .label(name.to_string())
        .on_click(move |_, _, cx| {
            change_height(&entity, move |look| HeightLook { axis, ..look }, cx);
        })
        .into_any_element()
}

/// The banding period. Measured, not counted, because that is what the stripes
/// are for: a cycle of a known number of model units is a ruler on the surface,
/// and CloudCompare keeps its last period across objects the same way.
///
/// Stepped by doubling rather than by a fixed increment, since one file's useful
/// period is another's noise by three orders of magnitude.
fn band_period_row(look: &HeightLook, entity: Entity<ModelViewport>, cx: &App) -> AnyElement {
    let period = look.period;
    v_flex()
        .gap_1()
        .child(section_label(
            rust_i18n::t!("viewport.height_band_period"),
            cx,
        ))
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child({
                    let entity = entity.clone();
                    Button::new("height-period-less")
                        .xsmall()
                        .ghost()
                        .label("\u{00d7} 1/2")
                        .disabled(period <= MIN_PERIOD * 2.0)
                        .on_click(move |_, _, cx| {
                            change_height(
                                &entity,
                                move |look| HeightLook {
                                    mode: HeightMode::Bands,
                                    period: (look.period / 2.0).clamp(MIN_PERIOD, MAX_PERIOD),
                                    ..look
                                },
                                cx,
                            );
                        })
                })
                .child(
                    div()
                        .flex_1()
                        .text_center()
                        .text_sm()
                        .child(format_units(period)),
                )
                .child({
                    let entity = entity.clone();
                    Button::new("height-period-more")
                        .xsmall()
                        .ghost()
                        .label("\u{00d7} 2")
                        .disabled(period >= MAX_PERIOD / 2.0)
                        .on_click(move |_, _, cx| {
                            change_height(
                                &entity,
                                move |look| HeightLook {
                                    mode: HeightMode::Bands,
                                    period: (look.period * 2.0).clamp(MIN_PERIOD, MAX_PERIOD),
                                    ..look
                                },
                                cx,
                            );
                        })
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: the glob drags in a `test`
    // attribute macro from the gpui prelude (see `frame.rs`'s test module).
    use super::{
        ANCHOR_CELLS, PivotLine, anchor_at, cell_index, cell_position, legend_values,
        pivot_symbol_geometry, ramp_rgb, stops_after_sort,
    };
    use trove_core::media::formats::types::Bounds;
    use trove_core::media::height_color::{
        ColorStop, Field, FieldData, HeightLook, HeightMode, Scale, scale_by_id,
    };
    use trove_core::media::render3d::{Camera, Framing};

    fn painted(mode: HeightMode, field: Field, box_bounds: Bounds) -> Vec<(f32, f32)> {
        legend_values(
            &HeightLook {
                mode,
                field,
                ..Default::default()
            }
            .resolve(&FieldData::geometry(&box_bounds)),
        )
    }

    /// A class bar labels whole classes — `7.5` is not a class — and stops at
    /// the last one the palette paints, because 23 is how many there are rather
    /// than a number a point carries.
    #[test]
    fn a_class_bar_labels_whole_classes() {
        let box_bounds = Bounds {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 10.0, 1.0],
        };
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Class,
            scale: Scale::Preset(scale_by_id("asprs")),
            ..Default::default()
        };
        for classes in [23, 7, 2, 1] {
            let values = legend_values(&look.resolve(&FieldData {
                bounds: &box_bounds,
                intensities: None,
                classes: Some(classes),
            }));
            assert!(
                values
                    .iter()
                    .all(|(value, _)| (value - value.round()).abs() < 1e-3),
                "{classes} classes: {values:?}"
            );
            assert_eq!(values.first().unwrap().0, 0.0);
            assert_eq!(values.last().unwrap().0, 22.0, "{classes} classes");
        }
        // A continuous scale reads the cloud's own classes instead, so the top
        // of the bar is the highest one present.
        let ramp = legend_values(
            &HeightLook {
                mode: HeightMode::Ramp,
                field: Field::Class,
                ..Default::default()
            }
            .resolve(&FieldData {
                bounds: &box_bounds,
                intensities: None,
                classes: Some(7),
            }),
        );
        assert_eq!(ramp.last().unwrap().0, 6.0);
        assert!(
            ramp.iter()
                .all(|(value, _)| (value - value.round()).abs() < 1e-3),
            "{ramp:?}"
        );
    }

    #[test]
    fn a_legends_labels_run_from_the_floor_to_the_ceiling() {
        let values = painted(
            HeightMode::Ramp,
            Field::Height,
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 100.0, 1.0],
            },
        );
        assert_eq!(values.first().unwrap(), &(0.0, 0.0));
        assert_eq!(values.last().unwrap(), &(100.0, 1.0));
        // Positions climb with their values, because the bar is painted bottom
        // up and a label at 0.3 has to sit at 0.3 of it.
        assert!(
            values
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1)
        );
        // A model with no span has one label, not a divide by nothing.
        let flat = painted(
            HeightMode::Ramp,
            Field::Height,
            Bounds {
                min: [0.0, 7.5, 0.0],
                max: [1.0, 7.5, 1.0],
            },
        );
        assert_eq!(flat, vec![(7.5, 0.0)]);
    }

    #[test]
    fn an_absolute_legends_labels_are_the_ones_the_field_names() {
        // A dip bar reads 0 / 30 / 60 / 90 wherever the model's bounds are,
        // because those are the numbers CloudCompare prints on it.
        let slope = painted(
            HeightMode::Bands,
            Field::Slope,
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 4_000.0, 1.0],
            },
        );
        let values: Vec<f32> = slope.into_iter().map(|(value, _)| value).collect();
        assert_eq!(values, vec![0.0, 30.0, 60.0, 90.0]);
    }

    #[test]
    fn the_anchor_strip_and_the_scale_agree_on_positions() {
        // The ends fold into the outer slots — an anchor at 1.0 is still the
        // last slot, not one past it.
        assert_eq!(cell_index(0.0), 0);
        assert_eq!(cell_index(1.0), ANCHOR_CELLS - 1);
        assert_eq!(cell_index(0.5), ANCHOR_CELLS / 2);
        // A slot stands for its own middle, which is where its colour is read.
        assert!((cell_position(0) - 0.5 / ANCHOR_CELLS as f32).abs() < 1e-6);
        for cell in 0..ANCHOR_CELLS {
            let stops = [ColorStop::new(cell_position(cell), [1, 2, 3])];
            assert_eq!(anchor_at(&stops, cell), Some(0), "slot {cell}");
        }
        // An anchor between two slots marks neither of them.
        assert_eq!(anchor_at(&[ColorStop::new(0.0, [0, 0, 0])], 5), None);
    }

    #[test]
    fn a_dropped_anchor_starts_as_the_colour_already_there() {
        // So that adding one is not itself an edit: the strip is unchanged until
        // the new anchor's colour is moved.
        let stops = vec![
            ColorStop::new(0.0, [0, 0, 0]),
            ColorStop::new(1.0, [255, 255, 255]),
        ];
        assert_eq!(ramp_rgb(&stops, 0.5), [128, 128, 128]);
        assert_eq!(ramp_rgb(&stops, 0.0), [0, 0, 0]);
        assert_eq!(ramp_rgb(&stops, 1.0), [255, 255, 255]);
        // A caller that has just appended one needs to know where it landed,
        // because the scale sorts them.
        let after = stops_after_sort(&[
            ColorStop::new(0.25, [9, 9, 9]),
            ColorStop::new(1.0, [255, 255, 255]),
            ColorStop::new(0.0, [0, 0, 0]),
        ]);
        assert_eq!(
            after.iter().map(|stop| stop.at).collect::<Vec<_>>(),
            vec![0.0, 0.25, 1.0]
        );
        assert_eq!(anchor_at(&after, cell_index(0.25)), Some(1));
    }

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
