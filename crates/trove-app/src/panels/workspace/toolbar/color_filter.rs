//! The workspace's colour filter.
//!
//! The trigger is a stock `component::button::Button` — the same one the
//! neighbouring kind / tag / shape / rating filters are built from — so the
//! icon size, the label metrics and the hover / selected states match by
//! construction instead of by imitation. (The framework's own colour picker
//! cannot do that: its trigger forwards the icon unsized, which is why the
//! hand-sized `Icon` this replaces existed.)
//!
//! The popover is assembled from `gpui-base`'s headless colour picker.
//! `ColorPickerState` owns the committed value, the transient preview, the hex
//! field and the four HSLA sliders; `ColorSwatch` carries the radio semantics
//! and the accessibility name; `ColorPicker` carries focus and Enter / Escape.
//! None of them draw anything an application did not ask for, which is the
//! whole reason that layer exists: `component`'s `ColorPicker` is one opaque
//! `IntoElement` with private panels and its own trigger.
//!
//! The shade ramp is generated here. The theme exposes two shades per family
//! (`red` / `red_light`), and the component library's full nine-family ramp is
//! `pub(crate)`, so a steady ramp is built from hue + lightness instead.

use gpui_kit::base::{ColorPicker as HeadlessColorPicker, ColorPickerState, ColorSwatch, h_flex};
use gpui_kit::component::Colorize as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, IconName, Selectable as _, Sizable as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The palette panel's index; every other index is the HSLA panel.
const PALETTE_TAB: usize = 0;

/// Shades drawn per family.
const SHADES: usize = 10;

/// The colour filter, sized and styled like the filters beside it.
pub(crate) fn color_filter(
    state: &Entity<ColorPickerState>,
    featured: Vec<Hsla>,
    cx: &App,
) -> impl IntoElement {
    let (open, selected) = {
        let state = state.read(cx);
        (state.is_open(), state.value().is_some())
    };
    let focus_handle = state.focus_handle(cx);
    let trigger_state = state.clone();
    let popover_state = state.clone();

    HeadlessColorPicker::new("color-filter")
        .open(open)
        .track_focus(&focus_handle)
        // Enter toggles and Escape dismisses, both handled by the base root.
        .on_open_change(move |open, _, cx| {
            trigger_state.update(cx, |state, cx| state.set_open(open, cx));
        })
        .child(
            Popover::new("color-filter-popover")
                .open(open)
                .w_72()
                .on_open_change(move |open: &bool, _, cx| {
                    popover_state.update(cx, |state, cx| state.set_open(*open, cx));
                })
                .trigger(
                    Button::new("filter-color")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Palette)
                        .label(rust_i18n::t!("workspace.color_filter").to_string())
                        .selected(selected),
                )
                .child(color_panel(state, featured, cx)),
        )
}

/// The popover body: the two panels, over a preview of whatever colour the
/// pointer or the text field is currently showing. Shared with the
/// smart-collection editor, which embeds it as its color column.
pub(crate) fn color_panel(
    state: &Entity<ColorPickerState>,
    featured: Vec<Hsla>,
    cx: &App,
) -> impl IntoElement {
    let active_tab = state.read(cx).active_tab();
    let displayed = state.read(cx).displayed_color();
    let hex_input = state.read(cx).hex_input().clone();
    let tab_state = state.clone();

    v_flex()
        .p_0p5()
        .gap_3()
        .child(
            TabBar::new("color-filter-mode")
                .segmented()
                .selected_index(active_tab)
                .on_click(move |ix: &usize, _, cx| {
                    tab_state.update(cx, |state, cx| state.set_active_tab(*ix, cx));
                })
                .child(
                    Tab::new()
                        .flex_1()
                        .label(rust_i18n::t!("color_picker.palette")),
                )
                .child(
                    Tab::new()
                        .flex_1()
                        .label(rust_i18n::t!("color_picker.hsla")),
                ),
        )
        .child(match active_tab {
            PALETTE_TAB => palette_panel(state, featured, cx).into_any_element(),
            _ => sliders_panel(state, cx).into_any_element(),
        })
        .when_some(displayed, |this, color| {
            this.child(Separator::horizontal()).child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .bg(color)
                            .flex_shrink_0()
                            .border_1()
                            .border_color(color.darken(0.2))
                            .size_5()
                            .rounded(cx.theme().radius),
                    )
                    .child(Input::new(&hex_input).small().px_2p5()),
            )
        })
}

/// The palette panel: the recently used colours, then the generated ramp.
fn palette_panel(
    state: &Entity<ColorPickerState>,
    featured: Vec<Hsla>,
    cx: &App,
) -> impl IntoElement {
    let featured = if featured.is_empty() {
        default_featured(cx)
    } else {
        featured
    };

    v_flex()
        .gap_3()
        .child(
            h_flex().gap_1().children(
                featured
                    .into_iter()
                    .enumerate()
                    // Featured slots may repeat a colour, so the slot index is
                    // part of the id: two swatches sharing an element id would
                    // share their hover state.
                    .map(|(ix, color)| swatch(swatch_id("featured", ix, 0), color, state, cx)),
            ),
        )
        .child(Separator::horizontal())
        .child(
            v_flex()
                .gap_1()
                .children(
                    color_families()
                        .into_iter()
                        .enumerate()
                        .map(|(family, shades)| {
                            h_flex()
                                .gap_1()
                                .children(shades.into_iter().enumerate().map(|(shade, color)| {
                                    swatch(swatch_id("ramp", family, shade), color, state, cx)
                                }))
                        }),
                ),
        )
}

/// A swatch's element id.
///
/// Built as one string rather than a tuple: `ElementId` implements `From` for
/// pairs, not triples, and the palette row and its shade both have to be in
/// the id for two swatches never to collide.
fn swatch_id(row: &str, first: usize, second: usize) -> SharedString {
    SharedString::from(format!("color-{row}-{first}-{second}"))
}

/// One selectable colour: hovering previews it, clicking commits it.
///
/// The ring is a filled box behind the colour rather than a `border`, the same
/// way `panels::common::color_swatch` draws its chips: gpui paints an element's
/// background and its border with two separate quads, each anti-aliasing its
/// own rounded outline, and at this size the two edges blend into a frayed
/// ring. Two filled boxes give each edge its own colour to blend into.
fn swatch(
    id: impl Into<ElementId>,
    color: Hsla,
    state: &Entity<ColorPickerState>,
    cx: &App,
) -> impl IntoElement {
    let selected = state.read(cx).value() == Some(color);
    let hover_state = state.clone();
    let click_state = state.clone();
    // The ring is the colour's own shade, so it reads as part of the swatch
    // rather than as a theme-neutral hairline.
    let (frame, ring) = match selected {
        true => (cx.theme().foreground, px(2.)),
        false => (color.darken(0.15), px(1.)),
    };
    let inner_radius = (cx.theme().radius - ring).max(px(0.));

    div()
        .size_5()
        .flex_shrink_0()
        .rounded(cx.theme().radius)
        .p(ring)
        .bg(frame)
        .hover(|this| this.bg(color.darken(0.35)))
        .child(
            ColorSwatch::new(id, color)
                .selected(selected)
                .size_full()
                .rounded(inner_radius)
                .bg(color)
                .on_hover(move |color, entered, window, cx| {
                    hover_state.update(cx, |state, cx| match entered {
                        true => state.preview_color(color, window, cx),
                        // Only the hex field follows the preview; the committed
                        // value is untouched by a hover, so leaving restores it.
                        false => state.clear_preview(window, cx),
                    });
                })
                .on_click(move |color, _, window, cx| {
                    click_state.update(cx, |state, cx| state.select_color(color, window, cx));
                }),
        )
}

/// The HSLA panel: four labelled sliders, each over a track that shows what
/// dragging it would do.
fn sliders_panel(state: &Entity<ColorPickerState>, cx: &App) -> impl IntoElement {
    let (slider_color, sliders) = {
        let state = state.read(cx);
        (
            state
                .displayed_color()
                .unwrap_or_else(|| hsla(0., 0., 0., 1.)),
            state.sliders().clone(),
        )
    };

    // 96 stops is where a hue strip stops banding at this height.
    let steps = 96usize;
    let hue_stops = (0..steps)
        .map(|ix| {
            let h = ix as f32 / (steps - 1) as f32;
            hsla(h, 1.0, 0.5, 1.0)
        })
        .collect::<Vec<_>>();
    let lightness_stops = (0..steps)
        .map(|ix| {
            let l = ix as f32 / (steps - 1) as f32;
            hsla(slider_color.h, 1.0, l, 1.0)
        })
        .collect::<Vec<_>>();

    v_flex()
        .gap_2()
        .child(slider_row(
            "color-hue",
            rust_i18n::t!("color_picker.hue"),
            &sliders.hue().clone(),
            segment_track(hue_stops),
            format!("{:.0}", slider_color.h * 360.),
            cx,
        ))
        .child(slider_row(
            "color-saturation",
            rust_i18n::t!("color_picker.saturation"),
            &sliders.saturation().clone(),
            gradient_track(
                hsla(slider_color.h, 0.0, slider_color.l, 1.0),
                hsla(slider_color.h, 1.0, slider_color.l, 1.0),
            ),
            format!("{:.0}", slider_color.s * 100.),
            cx,
        ))
        .child(slider_row(
            "color-lightness",
            rust_i18n::t!("color_picker.lightness"),
            &sliders.lightness().clone(),
            segment_track(lightness_stops),
            format!("{:.0}", slider_color.l * 100.),
            cx,
        ))
        .child(slider_row(
            "color-alpha",
            rust_i18n::t!("color_picker.alpha"),
            &sliders.alpha().clone(),
            gradient_track(
                hsla(slider_color.h, slider_color.s, slider_color.l, 0.0),
                hsla(slider_color.h, slider_color.s, slider_color.l, 1.0),
            ),
            format!("{:.0}", slider_color.a * 100.),
            cx,
        ))
}

/// A labelled slider over its own track.
fn slider_row(
    id: &'static str,
    label: impl Into<SharedString>,
    slider: &Entity<SliderState>,
    track: AnyElement,
    value: String,
    cx: &App,
) -> impl IntoElement {
    let label_color = cx.theme().foreground.opacity(0.7);

    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .min_w_16()
                .text_xs()
                .text_color(label_color)
                .child(label.into()),
        )
        .child(
            div()
                .relative()
                .flex()
                .items_center()
                .flex_1()
                .h_8()
                .child(track)
                // The slider paints no track of its own, so the one above
                // shows through and the handle sits on it.
                .child(Slider::new(slider).flex_1().bg(cx.theme().transparent)),
        )
        .child(
            div()
                .id(id)
                .w_10()
                .text_xs()
                .text_color(label_color)
                .text_align(TextAlign::Right)
                .child(value),
        )
}

/// A track drawn as discrete colour segments, for the hue and lightness axes,
/// which a two-stop gradient cannot express.
fn segment_track(colors: Vec<Hsla>) -> AnyElement {
    h_flex()
        .absolute()
        .left_0()
        .right_0()
        .h_2_5()
        .overflow_hidden()
        .children(
            colors
                .into_iter()
                .map(|color| div().flex_1().h_full().bg(color)),
        )
        .into_any_element()
}

/// A track drawn as a gradient, for the saturation and alpha axes.
fn gradient_track(start: Hsla, end: Hsla) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .h_2_5()
        .overflow_hidden()
        .bg(linear_gradient(
            90.,
            linear_color_stop(start, 0.),
            linear_color_stop(end, 1.),
        ))
        .into_any_element()
}

/// The colours the feature row falls back to: one shade per family, for a
/// library whose history has no colours yet. Ten slots: at 20px + gap that is
/// the widest row the `w_72` popover fits without spilling.
fn default_featured(cx: &App) -> Vec<Hsla> {
    let theme = cx.theme();
    vec![
        theme.red,
        theme.red_light,
        theme.yellow,
        theme.yellow_light,
        theme.green,
        theme.green_light,
        theme.cyan,
        theme.cyan_light,
        theme.blue,
        theme.blue_light,
    ]
}

/// Nine families of shades, light to dark.
///
/// Each family is one hue; the saturation eases off towards both ends of the
/// ramp, the way a designed palette does — a single hue at constant saturation
/// reads as a neon column rather than as shades of one colour.
fn color_families() -> Vec<Vec<Hsla>> {
    /// `(hue in degrees, saturation at mid-ramp)` — stone is the near-grey.
    const FAMILIES: [(f32, f32); 9] = [
        (30., 0.05),
        (0., 0.72),
        (28., 0.86),
        (45., 0.82),
        (142., 0.58),
        (189., 0.70),
        (221., 0.78),
        (271., 0.58),
        (330., 0.70),
    ];

    FAMILIES
        .iter()
        .map(|(hue, saturation)| {
            (0..SHADES)
                .map(|ix| {
                    let t = ix as f32 / (SHADES - 1) as f32;
                    let lightness = 0.94 - 0.74 * t;
                    let saturation = saturation * (1.0 - (t - 0.5).abs() * 0.8);
                    hsla(hue / 360.0, saturation, lightness, 1.0)
                })
                .collect()
        })
        .collect()
}
