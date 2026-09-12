//! Color picker panel — a classic desktop picker (Windows-style): a
//! saturation/value square, a hue bar, 基本/常用/历史 swatch rows, RGB and
//! hex inputs, and clear/confirm buttons (see the reference layout).
//!
//! The panel is a plain function over one [`ColorPickerState`] entity so it
//! can be embedded anywhere — the intended host is a `Popover` content
//! closure. History (max 20 colours) is persisted in the app config and
//! shared with every open picker.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt as _;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::*;
use rust_i18n::t;

use trove_core::config::AppConfig;

/// How many recently-picked colours are remembered.
const HISTORY_CAP: usize = 20;

/// 基本 — the eight primaries from the classic picker.
const BASIC_COLORS: &[&str] = &[
    "#ff0000", "#ffff00", "#00ff00", "#00ffff", "#0000ff", "#ff00ff", "#ffffff", "#000000",
];

/// 常用 — a practical palette (red→amber→green→cyan→blue→magenta→greys).
const COMMON_COLORS: &[&str] = &[
    "#ef4444", "#f97316", "#f59e0b", "#facc15", "#84cc16", "#22c55e", "#10b981", "#14b8a6",
    "#06b6d4", "#0ea5e9", "#3b82f6", "#6366f1", "#8b5cf6", "#a855f7", "#d946ef", "#ec4899",
    "#f43f5e", "#e11d48", "#7c2d12", "#a16207", "#4d7c0f", "#166534", "#155e75", "#1e40af",
    "#4c1d95", "#701a75", "#525252", "#a3a3a3",
];

/// The rainbow stops of the hue bar, in degrees.
const HUE_STOPS: [f32; 7] = [0.0, 60.0, 120.0, 180.0, 240.0, 300.0, 360.0];

/// Panel geometry the mouse handlers do their math against.
const SV_W: f32 = 248.0;
const SV_H: f32 = 160.0;
const HUE_W: f32 = 248.0;

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
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max <= 0.0 { 0.0 } else { d / max }, max)
}

fn rgb01_to_hex(rgb: [f32; 3]) -> String {
    let to8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", to8(rgb[0]), to8(rgb[1]), to8(rgb[2]))
}

fn hex_to_rgb01(hex: &str) -> Option<[f32; 3]> {
    let s = hex.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some([
        ch(0)? as f32 / 255.0,
        ch(2)? as f32 / 255.0,
        ch(4)? as f32 / 255.0,
    ])
}

/// The pure colour of a hue — what the SV square fades from white to.
fn pure_hue(h: f32) -> Rgba {
    let [r, g, b] = hsv_to_rgb(h, 1.0, 1.0);
    rgb(rgb_u32(r, g, b))
}

fn rgb_u32(r: f32, g: f32, b: f32) -> u32 {
    let to8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    u32::from_be_bytes([0, to8(r), to8(g), to8(b)])
}

/// `.bg()` helper for a `#rrggbb` string.
fn hex_bg(hex: &str) -> Rgba {
    rgb(u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0))
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
    /// Recently confirmed colours, newest first (shared with the panel).
    history: Rc<std::cell::RefCell<Vec<String>>>,
    r_field: Entity<InputState>,
    g_field: Entity<InputState>,
    b_field: Entity<InputState>,
    hex_field: Entity<InputState>,
    /// Set when the inputs no longer match the live colour; the next panel
    /// render syncs them (set_value needs the window, handlers don't have it).
    inputs_dirty: bool,
    /// Guards against input → state → input set_value feedback loops.
    syncing: bool,
}

/// Emitted when 确定 is pressed: the committed colour, also pushed to the
/// persisted history.
#[derive(Debug, Clone)]
pub struct ColorPicked(pub String);

impl EventEmitter<ColorPicked> for ColorPickerState {}

impl ColorPickerState {
    pub fn new(initial: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial = initial.unwrap_or("#4f46e5");
        let (h, s, v) = hex_to_rgb01(initial)
            .map(|rgb| rgb_to_hsv(rgb[0], rgb[1], rgb[2]))
            .unwrap_or((220.0, 0.6, 0.55));
        let history = Rc::new(std::cell::RefCell::new(
            AppConfig::load().color_history.clone(),
        ));
        let r_field = cx.new(|cx| InputState::new(window, cx));
        let g_field = cx.new(|cx| InputState::new(window, cx));
        let b_field = cx.new(|cx| InputState::new(window, cx));
        let hex_field = cx.new(|cx| InputState::new(window, cx).placeholder("#19be6b".to_string()));
        let this = Self {
            value: initial.to_lowercase(),
            h,
            s,
            v,
            sv_bounds: Rc::new(Cell::new(None)),
            hue_bounds: Rc::new(Cell::new(None)),
            dragging: None,
            history,
            r_field,
            g_field,
            b_field,
            hex_field,
            inputs_dirty: true,
            syncing: false,
        };
        // Field edits (typing or Enter) flow back into the colour.
        for input in [&this.r_field, &this.g_field, &this.b_field, &this.hex_field] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change | InputEvent::PressEnter { .. } => {
                    this.on_field_changed(cx);
                }
                _ => {}
            })
            .detach();
        }
        this
    }

    /// Apply an HSV triple: update the committed value, flag the inputs.
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

    /// Handle an edit coming from an input field.
    fn on_field_changed(&mut self, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let hex = self.hex_field.read(cx).value().to_string();
        if hex_to_rgb01(&hex).is_some() {
            self.set_hex(&hex, cx);
            return;
        }
        let parse = |e: &Entity<InputState>| e.read(cx).value().trim().parse::<u8>().ok();
        let (Some(r), Some(g), Some(b)) = (
            parse(&self.r_field),
            parse(&self.g_field),
            parse(&self.b_field),
        ) else {
            return;
        };
        let (h, s, v) = rgb_to_hsv(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        self.set_hsv(h, s, v, cx);
    }

    /// 确定: remember the colour in the persisted history and announce it.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let value = self.value.clone();
        {
            let mut history = self.history.borrow_mut();
            history.retain(|c| c != &value);
            history.insert(0, value.clone());
            history.truncate(HISTORY_CAP);
        }
        let mut config = AppConfig::load();
        config.color_history = self.history.borrow().clone();
        let _ = config.save();
        cx.emit(ColorPicked(value));
        cx.notify();
    }

    /// 清空: undo live edits back to the last confirmed colour.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.set_hex(&self.value.clone(), cx);
    }
}

// ---------------------------------------------------------------------------
// panel rendering
// ---------------------------------------------------------------------------

/// Build the picker panel for `state` (embed inside a `Popover` content
/// closure, or anywhere holding an `&mut App`).
pub fn picker_panel(
    state: &Entity<ColorPickerState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    // Flush pending input syncs (set_value needs the window we have here).
    if state.read(cx).inputs_dirty {
        state.update(cx, |this, cx| {
            this.syncing = true;
            let [r, g, b] = hsv_to_rgb(this.h, this.s, this.v);
            let to8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            this.r_field
                .update(cx, |f, cx| f.set_value(to8(r).to_string(), window, cx));
            this.g_field
                .update(cx, |f, cx| f.set_value(to8(g).to_string(), window, cx));
            this.b_field
                .update(cx, |f, cx| f.set_value(to8(b).to_string(), window, cx));
            this.hex_field
                .update(cx, |f, cx| f.set_value(this.value.clone(), window, cx));
            this.syncing = false;
            this.inputs_dirty = false;
        });
    }

    let (h, s, v) = {
        let st = state.read(cx);
        (st.h, st.s, st.v)
    };
    let history = state.read(cx).history.borrow().clone();

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

    // --- hue bar: seven 2-stop segments seam into a rainbow ---------------
    let hue_bounds = state.read(cx).hue_bounds.clone();
    let mut rainbow = h_flex().w_full().h_full();
    for pair in HUE_STOPS.windows(2) {
        rainbow = rainbow.child(div().flex_1().h_full().bg(linear_gradient(
            90.0,
            linear_color_stop(pure_hue(pair[0]), 0.0),
            linear_color_stop(pure_hue(pair[1]), 1.0),
        )));
    }
    let hue_bar = div()
        .id("cp-hue")
        .relative()
        .w(px(HUE_W))
        .h(px(14.0))
        .mt_2()
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
                .top_0()
                .h_full()
                .w(px(4.0))
                .rounded_sm()
                .bg(white())
                .border_1()
                .border_color(black())
                .left(px((h / 360.0) * HUE_W - 2.0)),
        );

    // --- swatch rows -------------------------------------------------------
    let swatches = |label: String, colors: &[&str]| -> AnyElement {
        let label_row_id = label.clone();
        let colors = colors.to_vec();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                h_flex().flex_wrap().gap_1().children(
                    colors
                        .iter()
                        .enumerate()
                        .map(|(ix, hex)| {
                            let hex = hex.to_string();
                            div()
                                .id(SharedString::from(format!("cp-sw-{label_row_id}-{ix}")))
                                .size_5()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(hex_bg(&hex))
                                .on_click({
                                    let state = state.clone();
                                    let hex = hex.clone();
                                    move |_, _, cx| {
                                        state.update(cx, |this, cx| this.set_hex(&hex, cx));
                                    }
                                })
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .into_any_element()
    };

    v_flex()
        .w(px(280.0))
        .gap_2()
        .child(sv_square)
        .child(hue_bar)
        .child(swatches(t!("color_picker.basic").to_string(), BASIC_COLORS))
        .child(swatches(
            t!("color_picker.common").to_string(),
            COMMON_COLORS,
        ))
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} (≤ {})",
                            t!("color_picker.history"),
                            HISTORY_CAP
                        )),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(history.iter().enumerate().map(|(ix, hex)| {
                            let hex = hex.clone();
                            div()
                                .id(SharedString::from(format!("cp-hist-{ix}")))
                                .size_5()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(hex_bg(&hex))
                                .on_click({
                                    let state = state.clone();
                                    let hex = hex.clone();
                                    move |_, _, cx| {
                                        state.update(cx, |this, cx| this.set_hex(&hex, cx));
                                    }
                                })
                        })),
                ),
        )
        .child(
            h_flex()
                .items_end()
                .gap_2()
                .child(
                    h_flex()
                        .items_end()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("R"),
                        )
                        .child(Input::new(&state.read(cx).r_field).small().w(px(52.0))),
                )
                .child(
                    h_flex()
                        .items_end()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("G"),
                        )
                        .child(Input::new(&state.read(cx).g_field).small().w(px(52.0))),
                )
                .child(
                    h_flex()
                        .items_end()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("B"),
                        )
                        .child(Input::new(&state.read(cx).b_field).small().w(px(52.0))),
                )
                .child(Input::new(&state.read(cx).hex_field).small().w(px(96.0))),
        )
        .child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("cp-reset")
                        .ghost()
                        .small()
                        .label(t!("color_picker.reset").to_string())
                        .on_click({
                            let state = state.clone();
                            move |_, _, cx| {
                                state.update(cx, |this, cx| this.reset(cx));
                            }
                        }),
                )
                .child(
                    Button::new("cp-confirm")
                        .primary()
                        .small()
                        .label(t!("color_picker.confirm").to_string())
                        .on_click({
                            let state = state.clone();
                            move |_, _, cx| {
                                state.update(cx, |this, cx| this.confirm(cx));
                            }
                        }),
                ),
        )
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

fn apply_hue(
    bounds_cell: &Rc<Cell<Option<Bounds<Pixels>>>>,
    position: Point<Pixels>,
    this: &mut ColorPickerState,
    cx: &mut Context<ColorPickerState>,
) {
    if let Some(bounds) = bounds_cell.get() {
        let dx = f32::from((position.x - bounds.origin.x).clamp(px(0.0), bounds.size.width));
        let h = dx / f32::from(bounds.size.width).max(1.0) * 360.0;
        this.set_hsv(h, this.s, this.v, cx);
    }
}
