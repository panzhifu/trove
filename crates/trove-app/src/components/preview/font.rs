//! Font preview: the sample text set in the font itself.
//!
//! The thumbnail is a 512×256 card — a fine way to tell two fonts apart in
//! the grid, but far too small to judge one by. The main-area preview draws
//! the sample in the font itself as **three rows of decreasing size** (the
//! one setting every type specimen has), in a preview text the user can
//! switch by language and replace with their own words. The stage's fit math
//! sees one block: [`specimen_metrics`] is its geometry, and the text size
//! always rides along proportionally as the stage zooms. The inspector keeps
//! the static card; a giant specimen has no room there.
//!
//! Returns `None` from [`specimen`] when the font file cannot be registered
//! with the text system, so the caller falls back to the thumbnail still.

use gpui_kit::base::h_flex;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _};
use gpui_kit::*;

use super::AssetPreviewData;
use crate::components::controls;
use crate::panels::common::{ensure_font_at_weight, ensure_font_registered};

/// Width the main area typically leaves for text, inside the stage's
/// padding.
const TEXT_WIDTH: f32 = 900.0;
/// Roughly how wide a glyph is relative to its size. Latin faces run nearer
/// 0.5 and CJK nearer 1.0, so this splits the difference and errs towards
/// not clipping.
const GLYPH_RATIO: f32 = 0.62;
/// One row's box height relative to its text size: line-height headroom, so
/// tall glyphs never touch the next row.
const ROW_LINE: f32 = 1.35;
/// Gap between rows.
const ROW_GAP: f32 = 12.0;
/// Sizes of the three specimen rows relative to the largest one — the one
/// proportion every type specimen ships with.
const ROW_RATIOS: [f32; 3] = [1.0, 0.6, 0.38];
/// How many characters a custom sample may carry. Longer texts stop being a
/// specimen and become a document; the input clamps as the user types.
const MAX_TEXT_CHARS: usize = 120;

/// The specimen's size range, in px of the largest row. Wide enough to read
/// a text face's color and tight enough that the block stays one screen.
const MIN_SPECIMEN_SIZE: f32 = 24.0;
const MAX_SPECIMEN_SIZE: f32 = 200.0;
/// px per wheel step.
const SIZE_STEP: f32 = 4.0;

/// The standard weight grid, as far down as `wght` naming goes.
const WEIGHT_GRID: [u16; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];

/// Languages the preview text can be written in. The *file's* declared
/// language only picks the initial value; the user can pick any of these, so
/// an accented Latin line (ÄÖÜ, Ç) is one click away even for a font that
/// declares nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontPreviewLanguage {
    En,
    ZhHans,
    ZhHant,
    Ja,
    Ko,
    De,
    Fr,
    Es,
    Pt,
    Ru,
}

pub(crate) use FontPreviewLanguage::*;

pub(crate) const FONT_PREVIEW_LANGUAGES: [FontPreviewLanguage; 10] =
    [En, ZhHans, ZhHant, Ja, Ko, De, Fr, Es, Pt, Ru];

impl FontPreviewLanguage {
    /// The language's own name, written in itself — language pickers label
    /// languages natively so every reader can find theirs.
    pub(crate) fn native_label(self) -> &'static str {
        match self {
            En => "English",
            ZhHans => "简体中文",
            ZhHant => "繁體中文",
            Ja => "日本語",
            Ko => "한국어",
            De => "Deutsch",
            Fr => "Français",
            Es => "Español",
            Pt => "Português",
            Ru => "Русский",
        }
    }

    /// The key custom texts are stored under — the same string the font
    /// facts use for a *declared* language, so a file saying `zh-Hans` opens
    /// with the sample written for it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            En => "en",
            ZhHans => "zh-Hans",
            ZhHant => "zh-Hant",
            Ja => "ja",
            Ko => "ko",
            De => "de",
            Fr => "fr",
            Es => "es",
            Pt => "pt",
            Ru => "ru",
        }
    }

    /// The preview language a font file's declared language opens with.
    /// `latin` (and anything unrecognised) opens as English: Latin letters
    /// and digits are the one script every font carries, so a wrong guess
    /// costs nothing.
    pub(crate) fn parse_declared(declared: &str) -> Option<Self> {
        match declared {
            "zh-Hans" => Some(ZhHans),
            "zh-Hant" => Some(ZhHant),
            "ja" => Some(Ja),
            "ko" => Some(Ko),
            _ => None,
        }
    }
}

/// The font viewer's session state, attached to [`AssetPreviewData`] by the
/// preview panel the same way the exposure render is: resolved once per
/// mutation, read as a clone on the paint path. `text` is already resolved —
/// the user's words when there are any, the built-in sample otherwise — so
/// nothing downstream re-reads the config.
#[derive(Clone)]
pub(crate) struct FontPreviewState {
    pub(crate) language: FontPreviewLanguage,
    pub(crate) text: String,
    /// The weight the specimen renders at. For a variable font this moves
    /// along the file's own `wght` axis; a static face stays at its declared
    /// weight and the picker stays hidden.
    pub(crate) weight: u16,
    /// The largest row's size in px. The wheel steps it (the preview's zoom
    /// gesture *is* the font size here); it is session state, so the next
    /// font opens at the fit default again.
    pub(crate) size: f32,
}

impl FontPreviewState {
    /// The state a fresh preview opens with: what the font file declares,
    /// else English; the file's `wght` axis bounds the starting weight; the
    /// size is the fit default the stage has always opened at.
    pub(crate) fn initial(
        declared: Option<FontPreviewLanguage>,
        axis: Option<(u16, u16)>,
        base_weight: Option<u16>,
    ) -> Self {
        let language = declared.unwrap_or(En);
        Self {
            language,
            text: initial_text(language),
            weight: starting_weight(axis, base_weight),
            size: default_specimen_size(),
        }
    }

    /// The same state, switched to `language` — the text re-resolves, so the
    /// specimen and the input both follow. Weight and size are viewer state,
    /// not language state, and survive the switch.
    pub(crate) fn switched(self, language: FontPreviewLanguage) -> Self {
        Self {
            language,
            text: initial_text(language),
            ..self
        }
    }
}

/// The weight a fresh preview starts at: Regular, pulled inside the file's
/// own axis — a 150–250 text axis starts at 250, not at a weight it cannot
/// render.
fn starting_weight(axis: Option<(u16, u16)>, base_weight: Option<u16>) -> u16 {
    let (min, max) = axis.unwrap_or((400, 400));
    let preferred = base_weight.unwrap_or(400).clamp(min, max);
    // Nearest grid weight inside the axis, the way variable-font UIs pick.
    WEIGHT_GRID
        .iter()
        .copied()
        .filter(|w| (min..=max).contains(w))
        .min_by_key(|w| w.abs_diff(preferred))
        .unwrap_or(preferred)
}

/// The weights a variable font can render, from its `wght` axis: the
/// standard grid inside the axis, and — when the axis sits between grid
/// points entirely — its own two ends.
pub(crate) fn weight_options((min, max): (u16, u16)) -> Vec<u16> {
    let mut options: Vec<u16> = WEIGHT_GRID
        .iter()
        .copied()
        .filter(|w| (min..=max).contains(w))
        .collect();
    if options.is_empty() {
        options = if min == max {
            vec![min]
        } else {
            vec![min, max]
        };
    }
    options
}

/// The `wght` axis the font facts record, `"100–900"` shaped (the mined
/// en-dash form), as bounds. A single-number axis (the mined form for
/// `min >= max`) reads as `min == max`.
pub(crate) fn parse_variable_weight(recorded: &str) -> Option<(u16, u16)> {
    let (min, max) = match recorded.split_once('–') {
        Some((min, max)) => {
            let min = min.trim().parse::<u16>().ok()?;
            let max = max.trim().parse::<u16>().ok()?;
            (min, max)
        }
        None => {
            let value = recorded.trim().parse::<u16>().ok()?;
            (value, value)
        }
    };
    (min <= max).then_some((min, max))
}

/// The size one wheel gesture lands on: notches (or pixels over 40) become
/// 1–5 steps of [`SIZE_STEP`], wheel-up grows. Clamped to the specimen's
/// range.
pub(crate) fn stepped_size(current: f32, lines: f32) -> f32 {
    if !lines.is_finite() || lines == 0.0 {
        return current.clamp(MIN_SPECIMEN_SIZE, MAX_SPECIMEN_SIZE);
    }
    let direction = if lines < 0.0 { 1.0 } else { -1.0 };
    let steps = (lines.abs().round() as i32).clamp(1, 5) as f32;
    (current + direction * steps * SIZE_STEP).clamp(MIN_SPECIMEN_SIZE, MAX_SPECIMEN_SIZE)
}

/// The built-in sample per language: the language's own words first — they
/// are what shows off a CJK or accented face — then digits and the
/// punctuation that separates a text face from a display one. Probing a
/// German face with `ÄÖÜ ß` is the point of having a language picker at all.
pub(crate) fn default_preview_text(language: FontPreviewLanguage) -> &'static str {
    match language {
        En => "AaBbGg 0123 The quick brown fox !?.;:@&%",
        ZhHans => "字体预览 永国爱 0123 ！？；：",
        ZhHant => "字型預覽 永國愛 0123 ！？；：",
        Ja => "フォントプレビュー 永字 0123 ！？「」",
        Ko => "폰트 미리보기 각인 0123 ！？；：",
        De => "Schriftprobe ÄÖÜ ß 0123 !?.;:@&%",
        Fr => "Aperçu ÀÉÇ ù 0123 !?.;:@&%",
        Es => "Muestra ÑÁÉ ¿? 0123 !?.;:@&%",
        Pt => "Prévia ÃÇÕ 0123 !?.;:@&%",
        Ru => "Пример ЖЩЪ 0123 !?.;:@&%",
    }
}

/// Cut a custom sample to the specimen's budget.
pub(crate) fn clamp_preview_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

/// The three specimen sizes for a base: the ratios, kept *distinct* while
/// there is room to distinguish them — a tiny base floors every row at one
/// pixel rather than going negative.
pub(crate) fn specimen_sizes(base: f32) -> [f32; 3] {
    let mut sizes: Vec<f32> = ROW_RATIOS
        .map(|ratio| (base * ratio).round().max(1.0))
        .to_vec();
    for i in 1..sizes.len() {
        while sizes[i] >= sizes[i - 1] && sizes[i] > 1.0 {
            sizes[i] -= 1.0;
        }
    }
    sizes.try_into().expect("three rows")
}

/// The size a fresh preview opens at, in px of the largest row — the value
/// the fit-based preview has always landed at for the built-in sample, kept
/// as the explicit default now that the size is viewer state.
pub(super) fn default_specimen_size() -> f32 {
    let glyphs = trove_core::media::thumb::DEFAULT_FONT_SAMPLE
        .chars()
        .count()
        .max(1) as f32;
    (TEXT_WIDTH / (glyphs * GLYPH_RATIO)).clamp(32.0, 160.0)
}

/// The block geometry for one size: width, height, and that size back as
/// the largest row's text size.
fn metrics_at(size: f32) -> (f32, f32, f32) {
    let height: f32 = ROW_RATIOS.iter().map(|r| size * r * ROW_LINE).sum::<f32>() + ROW_GAP * 2.0;
    (TEXT_WIDTH, height, size)
}

/// The geometry this preview shows right now — the viewer's own size, not a
/// viewport fit. The wheel drives the size directly, so an oversized block
/// pans instead of shrinking back.
pub(super) fn metrics_for(data: &AssetPreviewData) -> (f32, f32, f32) {
    metrics_at(
        data.font_preview
            .as_ref()
            .map(|state| state.size)
            .unwrap_or_else(default_specimen_size),
    )
}

/// One wheel gesture over a live specimen: step the font size. `true` when
/// the size moved and the stage should repaint.
pub(super) fn step_size(data: &mut AssetPreviewData, event: &ScrollWheelEvent) -> bool {
    let Some(state) = data.font_preview.as_mut() else {
        return false;
    };
    let lines = match event.delta {
        ScrollDelta::Lines(delta) => delta.y,
        ScrollDelta::Pixels(delta) => delta.y.as_f32() / 40.0,
    };
    let next = stepped_size(state.size, lines);
    if next == state.size {
        return false;
    }
    state.size = next;
    true
}

/// The language and text this preview shows. The panel attaches a
/// [`FontPreviewState`] the moment the live specimen exists and re-resolves
/// it on every mutation, so rendering is a clone — no config file I/O on the
/// paint path (a pan drag repaints every frame). Before a state is attached
/// (or for a preview that never attaches one), the declared language and the
/// built-in sample stand in.
fn resolved(data: &AssetPreviewData) -> (FontPreviewLanguage, u16, String) {
    if let Some(state) = &data.font_preview {
        return (state.language, state.weight, state.text.clone());
    }
    let language = data.font_declared_language.unwrap_or(En);
    let weight = data.font_base_weight.unwrap_or(400);
    (language, weight, default_preview_text(language).to_string())
}

/// The text a fresh state opens with: the user's own words for `language`
/// when there are any, the built-in sample otherwise.
pub(super) fn initial_text(language: FontPreviewLanguage) -> String {
    trove_core::config::AppConfig::load()
        .font_preview
        .custom_text(language.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| default_preview_text(language).to_string())
}

/// Whether this font previews as live text (its file registered with the
/// text system) rather than as the thumbnail still.
pub(super) fn specimen_available(data: &AssetPreviewData, cx: &mut App) -> bool {
    data.font_family
        .as_ref()
        .is_some_and(|family| ensure_font_registered(family, data.original.as_deref(), cx))
}

/// One specimen row: the text in the font itself, centered, clipped rather
/// than wrapped — a specimen line that runs off the block reads as "long
/// text", not as a broken preview.
fn specimen_row(family: &str, text: &str, size: f32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .overflow_hidden()
        .text_size(px(size))
        .line_height(px(size * ROW_LINE))
        // The card ink, not the theme's: the block wears the fixed paper the
        // baked cards are rasterized on (see `specimen_scaled`), and ink that
        // followed the theme would vanish on it in the dark theme.
        .text_color(crate::panels::common::card_ink())
        .font_family(family.to_string())
        .whitespace_nowrap()
        .child(text.to_string())
}

/// The large specimen block, or `None` when the font cannot be registered.
pub(super) fn specimen(data: &AssetPreviewData, cx: &mut App) -> Option<AnyElement> {
    let (width, height, size) = metrics_for(data);
    specimen_scaled(data, width, height, size, cx)
}

/// The specimen block at an explicit geometry: the stage picks the size, the
/// font fills it. `text_size` is the *largest* row; the other two are its
/// fixed fractions, so the whole block scales as one.
///
/// The block wears the same fixed paper the baked specimen cards are
/// rasterized on. This is what keeps the fullscreen stage readable: the
/// stage paints black behind whatever it holds, and this panel draws no
/// surface of its own — theme ink centered on that read as nothing at all.
/// On the card paper the ink is fixed too, so the block reads identically
/// in the panel, on the stage and in either theme.
pub(super) fn specimen_scaled(
    data: &AssetPreviewData,
    width: f32,
    height: f32,
    text_size: f32,
    cx: &mut App,
) -> Option<AnyElement> {
    let family = data.font_family.as_ref()?;
    let (_, weight, text) = resolved(data);
    // A variable font renders the picked weight through a patched face
    // (see `ensure_font_at_weight`); a static one renders as registered.
    let registered = match data.font_variable_weight {
        Some((min, max)) => {
            ensure_font_at_weight(family, data.original.as_deref(), weight.clamp(min, max), cx)
        }
        None => ensure_font_registered(family, data.original.as_deref(), cx),
    };
    if !registered {
        return None;
    }
    let rows = specimen_sizes(text_size)
        .into_iter()
        .map(|size| specimen_row(family, &text, size).font_weight(FontWeight(weight as f32)))
        .collect::<Vec<_>>();
    Some(
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(ROW_GAP))
            .w(px(width))
            .h(px(height))
            .bg(crate::panels::common::card_paper())
            .rounded(cx.theme().radius)
            .children(rows)
            .into_any_element(),
    )
}

/// What the title bar's font tools render from, copied out of the preview:
/// the title bar reads the preview and then mutates `cx` to build the
/// controls, and a shared read of the entity must not stay open across
/// those calls.
pub(crate) struct FontToolState {
    language: FontPreviewLanguage,
    weight: u16,
    text: String,
    size: f32,
    axis: Option<(u16, u16)>,
}

/// The font-tool state of a preview — `Some` only while a live specimen is
/// up, the same gate that used to show the in-preview strip.
pub(crate) fn tool_state(preview: &super::AssetPreviewPanel) -> Option<FontToolState> {
    if !preview.font_live {
        return None;
    }
    let data = &preview.data;
    let (language, weight, text) = resolved(data);
    let size = data
        .font_preview
        .as_ref()
        .map(|state| state.size)
        .unwrap_or_else(default_specimen_size);
    Some(FontToolState {
        language,
        weight,
        text,
        size,
        axis: data.font_variable_weight,
    })
}

/// The font specimen's tools in the panel title bar, where every other
/// preview keeps its tools: the sample text (editable, stored per
/// language), a reset for it, and the language and weight pickers. Built
/// per render because the input re-syncs to the current language's text —
/// the same contract the settings page's model field runs on. The state
/// arrives as a snapshot because the caller's read of the preview must be
/// closed by the time this runs; the panel handle is all the callbacks
/// need, and they fire outside the title bar's own update.
pub(crate) fn toolbar(
    panel: Entity<super::AssetPreviewPanel>,
    state: FontToolState,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let FontToolState {
        language,
        weight,
        text,
        size,
        axis,
    } = state;

    struct State {
        input: Entity<InputState>,
        _subscription: gpui::Subscription,
    }
    let state_entity = window.use_keyed_state(SharedString::from("font-preview-input"), cx, {
        let panel = panel.clone();
        let text = text.clone();
        move |window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(text.clone())
                    .placeholder(rust_i18n::t!("preview.font_text").to_string())
            });
            let subscription = cx.subscribe(&input, {
                move |_, input, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let value = clamp_preview_text(input.read(cx).value().to_string().as_str());
                        panel.update(cx, |panel, cx| panel.set_font_custom_text(value, cx));
                    }
                }
            });
            State {
                input,
                _subscription: subscription,
            }
        }
    });

    // A language switch (or a reset) reaches the input on the next repaint —
    // the text it holds belongs to the previous language otherwise.
    state_entity.update(cx, |state, cx| {
        if state.input.read(cx).value() != text {
            state.input.update(cx, |input, cx| {
                input.set_value(text.clone(), window, cx);
            });
        }
    });
    let state = state_entity.read(cx);

    let language_dropdown = controls::dropdown_button(
        "font-preview-language",
        language.native_label().to_string(),
        FONT_PREVIEW_LANGUAGES
            .iter()
            .map(|l| (*l, l.native_label().to_string()))
            .collect(),
        language,
        {
            let panel = panel.clone();
            move |picked: FontPreviewLanguage, cx: &mut App| {
                panel.update(cx, |panel, cx| panel.set_font_language(picked, cx));
            }
        },
        130.0,
        Anchor::TopLeft,
    );

    let reset = {
        let panel = panel.clone();
        controls::icon_button(
            "font-preview-text-reset",
            IconName::RotateCw,
            rust_i18n::t!("preview.font_text_reset").to_string(),
        )
        // The sample reads as built-in exactly when the text equals it — the
        // same test the state resolves with, so the button never claims a
        // reset that would change nothing (and no config read on the paint
        // path).
        .disabled(text == default_preview_text(language))
        .on_click(move |_, _, cx| {
            panel.update(cx, |panel, cx| panel.reset_font_custom_text(cx));
        })
    };

    // The weight picker exists only for a font that can actually change
    // weight — a static face at one weight would make it decoration.
    let weight_dropdown = axis.map(|axis| {
        controls::dropdown_button(
            "font-preview-weight",
            format!("{} {weight}", rust_i18n::t!("preview.font_weight")),
            weight_options(axis)
                .into_iter()
                .map(|w| (w, w.to_string()))
                .collect(),
            weight,
            {
                let panel = panel.clone();
                move |picked: u16, cx: &mut App| {
                    panel.update(cx, |panel, cx| panel.set_font_weight(picked, cx));
                }
            },
            90.0,
            Anchor::TopLeft,
        )
    });

    h_flex()
        .items_center()
        .gap_1()
        .child(
            Input::new(&state.input)
                .small()
                .appearance(true)
                .w(px(280.)),
        )
        .child(reset)
        .child(language_dropdown)
        .children(weight_dropdown)
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{} px", size.round() as u32)),
        )
}

#[cfg(test)]
mod tests {
    use super::FONT_PREVIEW_LANGUAGES;
    use super::FontPreviewLanguage;
    use super::MAX_TEXT_CHARS;
    use super::clamp_preview_text;
    use super::default_preview_text;
    use super::parse_variable_weight;
    use super::specimen_sizes;
    use super::starting_weight;
    use super::stepped_size;
    use super::weight_options;
    use FontPreviewLanguage::{Ja, Ko, ZhHans, ZhHant};

    #[test]
    fn every_language_has_a_built_in_sample_and_a_native_label() {
        for language in FONT_PREVIEW_LANGUAGES {
            assert!(
                !default_preview_text(language).trim().is_empty(),
                "{:?} has no sample text",
                language
            );
            assert!(
                !language.native_label().trim().is_empty(),
                "{:?} has no label",
                language
            );
            assert!(!language.as_str().is_empty());
        }
    }

    /// The declared-language mapping is the one contract the font facts
    /// rely on: CJK scripts open with their own sample, `latin` degrades to
    /// English (every font carries it), and an unknown string must not
    /// pretend to be a language it is not.
    #[test]
    fn declared_languages_parse_and_latin_degrades_to_english() {
        assert_eq!(FontPreviewLanguage::parse_declared("zh-Hans"), Some(ZhHans));
        assert_eq!(FontPreviewLanguage::parse_declared("zh-Hant"), Some(ZhHant));
        assert_eq!(FontPreviewLanguage::parse_declared("ja"), Some(Ja));
        assert_eq!(FontPreviewLanguage::parse_declared("ko"), Some(Ko));
        assert_eq!(FontPreviewLanguage::parse_declared("latin"), None);
        assert_eq!(FontPreviewLanguage::parse_declared("klingon"), None);
    }

    #[test]
    fn custom_text_is_clamped_to_the_specimen_budget() {
        let long = "x".repeat(MAX_TEXT_CHARS + 50);
        assert_eq!(clamp_preview_text(&long).chars().count(), MAX_TEXT_CHARS);
    }

    /// The three rows stay three *distinct* sizes at every base a stage can
    /// actually hand over; at absurd bases every row floors at one pixel
    /// rather than going negative.
    #[test]
    fn specimen_sizes_are_three_distinct_values_largest_first() {
        for base in [160.0_f32, 64.0, 32.0, 10.0] {
            let sizes = specimen_sizes(base);
            assert_eq!(sizes.len(), 3);
            assert!(sizes[0] > sizes[1] && sizes[1] > sizes[2], "{sizes:?}");
        }
        // The ratios hold at a normal base: 1 / 0.6 / 0.38.
        let sizes = specimen_sizes(100.0);
        assert_eq!(sizes, [100.0, 60.0, 38.0]);
        // Below the floor, nothing goes negative.
        let sizes = specimen_sizes(1.0);
        assert!(sizes.iter().all(|size| *size >= 1.0), "{sizes:?}");
    }

    /// The axis the miner records — `"100–900"` for a span, a single number
    /// for a degenerate one — parses into bounds, and garbage never does.
    #[test]
    fn the_recorded_wght_axis_parses_into_bounds() {
        assert_eq!(parse_variable_weight("100–900"), Some((100, 900)));
        assert_eq!(parse_variable_weight("250"), Some((250, 250)));
        assert_eq!(parse_variable_weight("700–300"), None);
        assert_eq!(parse_variable_weight("abc–900"), None);
        assert_eq!(parse_variable_weight(""), None);
    }

    /// The picker offers the standard grid inside the axis; an axis that
    /// sits between grid points falls back to its own ends, so every option
    /// it offers is one the file can render.
    #[test]
    fn weight_options_follow_the_axis() {
        assert_eq!(
            weight_options((100, 900)),
            vec![100, 200, 300, 400, 500, 600, 700, 800, 900]
        );
        assert_eq!(weight_options((350, 650)), vec![400, 500, 600]);
        assert_eq!(weight_options((250, 270)), vec![250, 270]);
        assert_eq!(weight_options((250, 250)), vec![250]);
    }

    /// The opening weight is Regular pulled inside the axis, snapped to the
    /// nearest *grid* point the file can render — the grid has no 250, so a
    /// narrow axis around it still lands on 200.
    #[test]
    fn the_starting_weight_is_the_nearest_grid_weight_inside_the_axis() {
        assert_eq!(starting_weight(Some((100, 900)), Some(400)), 400);
        assert_eq!(starting_weight(Some((100, 900)), None), 400);
        assert_eq!(starting_weight(Some((150, 250)), Some(400)), 200);
        assert_eq!(starting_weight(Some((150, 260)), None), 200);
        assert_eq!(starting_weight(None, Some(700)), 400);
    }

    /// Wheel up grows, wheel down shrinks, both clamp — and an idle wheel
    /// changes nothing.
    #[test]
    fn the_wheel_steps_the_size_within_bounds() {
        assert_eq!(stepped_size(76.0, -1.0), 80.0);
        assert_eq!(stepped_size(76.0, 1.0), 72.0);
        assert_eq!(stepped_size(199.0, -1.0), 200.0);
        assert_eq!(stepped_size(200.0, -3.0), 200.0);
        assert_eq!(stepped_size(24.0, 2.0), 24.0);
        assert_eq!(stepped_size(100.0, 0.0), 100.0);
    }
}
