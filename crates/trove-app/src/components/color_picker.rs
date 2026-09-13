//! Color picker panel — Eagle-style compact picker: a saturation/value
//! square with a vertical hue strip on its right, two rows of preset
//! swatches led by a "no colour" swatch, and a hex input with a
//! colour-wheel confirm button (see the reference layout).
//!
//! The panel is a plain function over one [`ColorPickerState`] entity so it
//! can be embedded anywhere — the intended host is a `Popover` content
//! closure. Confirmed colours are persisted in the app config
//! (`color_history`, max 20) even though the compact panel shows no
//! history row.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt as _;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::PopoverState;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable as _};
use gpui_kit::*;
use rust_i18n::t;

/// Row 1 — neutrals, brown and pink, in the spirit of the reference layout.
/// The "no colour" swatch is prepended by the renderer.
const ROW_NEUTRAL: &[&str] = &[
    "#262626", "#595959", "#8c8c8c", "#bfbfbf", "#f5f5f5", "#8c6239", "#ffc1cc", "#ffd666",
];

/// Row 2 — the vivid, searchable primaries.
const ROW_VIVID: &[&str] = &[
    "#ff4d4f", "#fa8c16", "#fadb14", "#52c41a", "#13c2c2", "#1890ff", "#722ed1", "#eb2f96",
];

/// The rainbow stops of the hue strip, in degrees.
const HUE_STOPS: [f32; 7] = [0.0, 60.0, 120.0, 180.0, 240.0, 300.0, 360.0];

/// Panel geometry the mouse handlers do their math against.
const SV_W: f32 = 200.0;
const SV_H: f32 = 160.0;
/// Width of the vertical hue strip; its height matches [`SV_H`].
const HUE_W: f32 = 14.0;

// ---------------------------------------------------------------------------
// colour math (no deps: f32 0..1 channels, hue 0..360 degrees)
// ---------------------------------------------------------------------------

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let h = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (0.0, 0.0, c),
    };
    [r, g, b]
}

fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let v = max;
    let s = if max <= 0.0 { 0.0 } else { delta / max };
    let h = if delta <= 0.0 {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    (h, s, v)
}

fn rgb01_to_hex(rgb: [f32; 3]) -> String {
    let to8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", to8(rgb[0]), to8(rgb[1]), to8(rgb[2]))
}

fn hex_to_rgb01(hex: &str) -> Option<[f32; 3]> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let channel = |s: &str| u8::from_str_radix(s, 16).ok().map(|v| v as f32 / 255.0);
    Some([
        channel(&hex[0..2])?,
        channel(&hex[2..4])?,
        channel(&hex[4..6])?,
    ])
}

/// The pure colour of a hue — what the SV square fades from white to.
fn pure_hue(h: f32) -> Rgba {
    let [r, g, b] = hsv_to_rgb(h, 1.0, 1.0);
    rgba(rgb_u32(r, g, b))
}

fn rgb_u32(r: f32, g: f32, b: f32) -> u32 {
    let to8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u32;
    (to8(r) << 16) | (to8(g) << 8) | to8(b)
}

fn hex_bg(hex: &str) -> Rgba {
    let [r, g, b] = hex_to_rgb01(hex).unwrap_or([0.0, 0.0, 0.0]);
    rgba(rgb_u32(r, g, b))
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

/// Which picker area the mouse is captured over.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Sv,
    Hue,
}

pub struct ColorPickerState {
    /// The committed colour (lowercase `#rrggbb`).
    value: String,
    /// Live HSV editing state.
    h: f32,
    s: f32,
    v: f32,
    sv_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    hue_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    dragging: Option<Drag>,
    hex_field: Entity<InputState>,
    /// Set when the input no longer matches the live colour; the next panel
    /// render syncs it (set_value needs the window, handlers don't have it).
    inputs_dirty: bool,
    /// Guards against input → state → input set_value feedback loops.
    syncing: bool,
    /// Recently picked colours (mirrors the persisted history, loaded at
    /// construction and refreshed on every confirm).
    history: Vec<String>,
}

/// Emitted when the colour is confirmed (wheel button, or Enter in the hex
/// field): the committed colour, also pushed to the persisted history.
#[derive(Debug, Clone)]
pub struct ColorPicked(pub String);

impl EventEmitter<ColorPicked> for ColorPickerState {}

impl ColorPickerState {
    pub fn new(initial: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial = initial.unwrap_or("#4f46e5");
        let (h, s, v) = hex_to_rgb01(initial)
            .map(|rgb| rgb_to_hsv(rgb[0], rgb[1], rgb[2]))
            .unwrap_or((220.0, 0.6, 0.55));
        let hex_field = cx.new(|cx| InputState::new(window, cx).placeholder("#FF0000".to_string()));
        let this = Self {
            value: initial.to_lowercase(),
            h,
            s,
            v,
            sv_bounds: Rc::new(Cell::new(None)),
            hue_bounds: Rc::new(Cell::new(None)),
            dragging: None,
            hex_field,
            inputs_dirty: true,
            syncing: false,
            history: trove_core::history::AppHistory::load().colors().to_vec(),
        };
        // Typing applies the colour live; Enter applies and confirms.
        cx.subscribe(
            &this.hex_field,
            |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => this.on_field_changed(cx),
                InputEvent::PressEnter { .. } => {
                    this.on_field_changed(cx);
                    if hex_to_rgb01(&this.hex_field.read(cx).value()).is_some() {
                        this.confirm(cx);
                    }
                }
                _ => {}
            },
        )
        .detach();
        this
    }

    /// Apply an HSV triple: update the committed value, flag the input.
    fn set_hsv(&mut self, h: f32, s: f32, v: f32, cx: &mut Context<Self>) {
        self.h = h.clamp(0.0, 360.0);
        self.s = s.clamp(0.0, 1.0);
        self.v = v.clamp(0.0, 1.0);
        self.value = rgb01_to_hex(hsv_to_rgb(self.h, self.s, self.v));
        self.inputs_dirty = true;
        cx.notify();
    }

    /// Apply a hex string; invalid input is ignored.
    fn set_hex(&mut self, hex: &str, cx: &mut Context<Self>) {
        if let Some([r, g, b]) = hex_to_rgb01(hex) {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            self.set_hsv(h, s, v, cx);
        }
    }

    /// Handle an edit coming from the hex field.
    fn on_field_changed(&mut self, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let hex = self.hex_field.read(cx).value().to_string();
        self.set_hex(&hex, cx);
    }

    /// Confirm: remember the colour in the persisted history and announce it.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let value = self.value.clone();
        let mut history = trove_core::history::AppHistory::load();
        let _ = history.push_color(&value);
        self.history = history.colors().to_vec();
        cx.emit(ColorPicked(value));
        cx.notify();
    }

    /// "No colour" swatch: undo live edits back to the last confirmed colour.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.set_hex(&self.value.clone(), cx);
    }
}

// ---------------------------------------------------------------------------
// panel rendering
// ---------------------------------------------------------------------------

/// Build the picker panel for `state` (embed inside a `Popover` content
/// closure, or anywhere holding an `&mut App`). `popover` is the hosting
/// popover: confirming a colour dismisses it, like Eagle's picker.
pub fn picker_panel(
    popover: &Entity<PopoverState>,
    state: &Entity<ColorPickerState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    // Flush pending input syncs (set_value needs the window we have here).
    if state.read(cx).inputs_dirty {
        state.update(cx, |this, cx| {
            this.syncing = true;
            this.hex_field
                .update(cx, |f, cx| f.set_value(this.value.clone(), window, cx));
            this.syncing = false;
            this.inputs_dirty = false;
        });
    }

    let (h, s, v, value) = {
        let st = state.read(cx);
        (st.h, st.s, st.v, st.value.clone())
    };

    // --- SV square: white→hue base, transparent→black overlay --------------
    let sv_bounds = state.read(cx).sv_bounds.clone();
    let sv_square = div()
        .id("cp-sv")
        .relative()
        .w(px(SV_W))
        .h(px(SV_H))
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .bg(linear_gradient(
            90.0,
            linear_color_stop(white(), 0.0),
            linear_color_stop(pure_hue(h), 1.0),
        ))
        .on_prepaint({
            let cell = sv_bounds.clone();
            move |bounds: Bounds<Pixels>, _, _| cell.set(Some(bounds))
        })
        .on_mouse_down(MouseButton::Left, {
            let state = state.clone();
            let sv_bounds = sv_bounds.clone();
            move |event: &MouseDownEvent, _, cx| {
                state.update(cx, |this, cx| {
                    this.dragging = Some(Drag::Sv);
                    apply_sv(&sv_bounds, event.position, this, cx);
                });
            }
        })
        .on_mouse_move({
            let state = state.clone();
            let sv_bounds = sv_bounds.clone();
            move |event: &MouseMoveEvent, _, cx| {
                state.update(cx, |this, cx| {
                    if this.dragging == Some(Drag::Sv) {
                        apply_sv(&sv_bounds, event.position, this, cx);
                    }
                });
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let state = state.clone();
            move |_: &MouseUpEvent, _, cx| {
                state.update(cx, |this, cx| {
                    this.dragging = None;
                    cx.notify();
                });
            }
        })
        .child(div().absolute().inset_0().bg(linear_gradient(
            180.0,
            linear_color_stop(hsla(0.0, 0.0, 0.0, 0.0), 0.0),
            linear_color_stop(black(), 1.0),
        )))
        .child(
            div()
                .absolute()
                .size(px(12.0))
                .rounded_full()
                .border_2()
                .border_color(white())
                .left(px(s * SV_W - 6.0))
                .top(px((1.0 - v) * SV_H - 6.0)),
        );

    // --- hue strip: vertical rainbow at the square's right -----------------
    let hue_bounds = state.read(cx).hue_bounds.clone();
    let mut rainbow = v_flex().w_full().h_full();
    for pair in HUE_STOPS.windows(2) {
        rainbow = rainbow.child(div().flex_1().w_full().bg(linear_gradient(
            180.0,
            linear_color_stop(pure_hue(pair[0]), 0.0),
            linear_color_stop(pure_hue(pair[1]), 1.0),
        )));
    }
    let hue_strip = div()
        .id("cp-hue")
        .relative()
        .w(px(HUE_W))
        .h(px(SV_H))
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .on_prepaint({
            let cell = hue_bounds.clone();
            move |bounds: Bounds<Pixels>, _, _| cell.set(Some(bounds))
        })
        .on_mouse_down(MouseButton::Left, {
            let state = state.clone();
            let hue_bounds = hue_bounds.clone();
            move |event: &MouseDownEvent, _, cx| {
                state.update(cx, |this, cx| {
                    this.dragging = Some(Drag::Hue);
                    apply_hue(&hue_bounds, event.position, this, cx);
                });
            }
        })
        .on_mouse_move({
            let state = state.clone();
            let hue_bounds = hue_bounds.clone();
            move |event: &MouseMoveEvent, _, cx| {
                state.update(cx, |this, cx| {
                    if this.dragging == Some(Drag::Hue) {
                        apply_hue(&hue_bounds, event.position, this, cx);
                    }
                });
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let state = state.clone();
            move |_: &MouseUpEvent, _, cx| {
                state.update(cx, |this, cx| {
                    this.dragging = None;
                    cx.notify();
                });
            }
        })
        .child(rainbow)
        .child(
            div()
                .absolute()
                .left_0()
                .w_full()
                .h(px(3.0))
                .rounded_sm()
                .bg(white())
                .border_1()
                .border_color(black())
                .top(px((h / 360.0) * SV_H - 1.5)),
        );

    // --- swatch rows: "no colour" + neutrals, then the vivid primaries -----
    let preset_swatch = |ix: usize, hex: &str| -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("cp-sw-{ix}")))
            .size_5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .bg(hex_bg(hex))
            .cursor_pointer()
            .hover(|this| this.opacity(0.8))
            .on_click({
                let state = state.clone();
                let hex = hex.to_string();
                move |_, _, cx| {
                    state.update(cx, |this, cx| this.set_hex(&hex, cx));
                }
            })
    };
    // The slash swatch: back to the last confirmed colour.
    let none_swatch = div()
        .id("cp-sw-none")
        .size_5()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().border)
        .bg(white())
        .cursor_pointer()
        .hover(|this| this.opacity(0.8))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(SharedString::from(
                t!("color_picker.none").to_string(),
            ))
            .build(window, cx)
        })
        .on_click({
            let state = state.clone();
            move |_, _, cx| {
                state.update(cx, |this, cx| this.reset(cx));
            }
        })
        .child(
            Icon::new(IconName::Minus)
                .size_3()
                .rotate(radians(std::f32::consts::FRAC_PI_4))
                .text_color(gpui_kit::black()),
        );

    let mut row1 = h_flex().gap_1().child(none_swatch);
    for (ix, hex) in ROW_NEUTRAL.iter().enumerate() {
        row1 = row1.child(preset_swatch(ix, hex));
    }
    let mut row2 = h_flex().gap_1();
    for (ix, hex) in ROW_VIVID.iter().enumerate() {
        row2 = row2.child(preset_swatch(ROW_NEUTRAL.len() + ix, hex));
    }

    // --- bottom: current-colour swatch + hex input + the confirm wheel -----
    let bottom = h_flex()
        .gap_2()
        .items_center()
        .child(
            h_flex()
                .flex_1()
                .h(px(28.0))
                .items_center()
                .gap_1p5()
                .px_1p5()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .child(
                    div()
                        .size(px(14.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(hex_bg(&value)),
                )
                .child(
                    Input::new(&state.read(cx).hex_field)
                        .small()
                        .appearance(false),
                ),
        )
        .child(
            div()
                .id("cp-confirm")
                .size(px(26.0))
                .rounded_full()
                .cursor_pointer()
                .border_1()
                .border_color(cx.theme().border)
                .bg(linear_gradient(
                    135.0,
                    linear_color_stop(pure_hue(0.0), 0.0),
                    linear_color_stop(pure_hue(280.0), 1.0),
                ))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(SharedString::from(
                        t!("color_picker.confirm").to_string(),
                    ))
                    .build(window, cx)
                })
                .on_click({
                    let state = state.clone();
                    let popover = popover.clone();
                    move |_, window, cx| {
                        state.update(cx, |this, cx| this.confirm(cx));
                        popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                    }
                }),
        );

    // The recent-colours history row, rebuilt on every open from the
    // persisted record; picking one applies and confirms immediately.
    let history = state.read(cx).history.clone();
    let history_row = if history.is_empty() {
        None
    } else {
        let mut row = h_flex().gap_1();
        for (ix, hex) in history.iter().enumerate() {
            let hex = hex.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("cp-hist-{ix}")))
                    .size_5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(hex_bg(&hex))
                    .cursor_pointer()
                    .hover(|this| this.opacity(0.8))
                    .on_click({
                        let state = state.clone();
                        let popover = popover.clone();
                        move |_, window, cx| {
                            state.update(cx, |this, cx| {
                                this.set_hex(&hex, cx);
                                this.confirm(cx);
                            });
                            popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                        }
                    }),
            );
        }
        Some(row)
    };

    v_flex()
        .w(px(SV_W + HUE_W + 8.0))
        .gap_2()
        .child(h_flex().gap_2().child(sv_square).child(hue_strip))
        .child(row1)
        .child(row2)
        .children(history_row)
        .child(bottom)
        .into_any_element()
}

fn apply_sv(
    bounds_cell: &Rc<Cell<Option<Bounds<Pixels>>>>,
    position: Point<Pixels>,
    this: &mut ColorPickerState,
    cx: &mut Context<ColorPickerState>,
) {
    if let Some(bounds) = bounds_cell.get() {
        let dx = f32::from((position.x - bounds.origin.x).clamp(px(0.0), bounds.size.width));
        let dy = f32::from((position.y - bounds.origin.y).clamp(px(0.0), bounds.size.height));
        let s = dx / f32::from(bounds.size.width).max(1.0);
        let v = 1.0 - dy / f32::from(bounds.size.height).max(1.0);
        this.set_hsv(this.h, s, v, cx);
    }
}

/// Vertical strip: hue runs top (0°) to bottom (360°).
fn apply_hue(
    bounds_cell: &Rc<Cell<Option<Bounds<Pixels>>>>,
    position: Point<Pixels>,
    this: &mut ColorPickerState,
    cx: &mut Context<ColorPickerState>,
) {
    if let Some(bounds) = bounds_cell.get() {
        let dy = f32::from((position.y - bounds.origin.y).clamp(px(0.0), bounds.size.height));
        let h = dy / f32::from(bounds.size.height).max(1.0) * 360.0;
        this.set_hsv(h, this.s, this.v, cx);
    }
}
