//! Shared helpers for the dock panels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use gpui_kit::base::h_flex;
use gpui_kit::component::{ActiveTheme, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use trove_core::model::{Asset, AssetKind, AssetQuery};

use crate::library::LibraryController;

/// Distinct icon per asset kind (image cells only fall back to this when no
/// thumbnail was generated). Icon names resolve to the gpui-kit asset set.
pub(crate) fn kind_icon(kind: AssetKind) -> IconName {
    match kind {
        AssetKind::Image => IconName::Frame,
        AssetKind::Video => IconName::Play,
        AssetKind::Audio => IconName::Pause,
        AssetKind::Document => IconName::FileText,
        AssetKind::Archive => IconName::File,
        AssetKind::Font => IconName::CaseSensitive,
        AssetKind::Model => IconName::Building2,
        AssetKind::Other => IconName::File,
    }
}

pub(crate) fn display_name(asset: &Asset) -> String {
    asset
        .title
        .clone()
        .unwrap_or_else(|| asset.file_name.clone())
}

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub(crate) fn observe_controller<V: Render + 'static>(
    cx: &mut Context<V>,
    controller: &Entity<LibraryController>,
) {
    cx.observe(controller, |_, _, cx| cx.notify()).detach();
}

pub(crate) fn separator_label(cx: &Context<impl Render>, text: impl Into<String>) -> Div {
    h_flex().px_1().pt_1().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(text.into()),
    )
}

pub(crate) fn live_count(controller: &LibraryController) -> u64 {
    controller
        .library
        .query_assets(&AssetQuery::live())
        .map(|page| page.total)
        .unwrap_or(0)
}

pub(crate) fn trash_count(controller: &LibraryController) -> u64 {
    controller
        .library
        .query_assets(&AssetQuery::trashed())
        .map(|page| page.total)
        .unwrap_or(0)
}

/// Parse a `#rrggbb` hex (leading `#` optional, case-insensitive) into an
/// opaque `u32` value usable with `gpui::rgb(0xRRGGBB)`. `None` if malformed.
pub(crate) fn hex_to_rgb(s: &str) -> Option<u32> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// A square color chip in the shared palette style: hairline frame, a stronger
/// one when `selected`, hover feedback. Used by the smart-collection palette
/// and the Inspector's mined-color swatches.
///
/// Square rather than round so the chips match the swatches in the toolbar
/// colour picker's palette, which are square.
///
/// The frame is painted as *padding* rather than as an outline. An element's
/// `bg` and its `border` are two separate `paint_quad` calls inside gpui
/// (`style.rs:730` and `:748`), each anti-aliasing its own rounded outline;
/// at this size — 24 px across a 6 px radius — both anti-aliased edges land on
/// the same ring of pixels, and the frame colour bleeds into the fill, which
/// reads as a frayed edge. Painting the frame as a filled rounded box behind
/// an inset rounded box gives each edge its own background to blend into: the
/// outer one against the panel, the inner one against the frame.
///
/// The inner box therefore carries its own, smaller radius. `overflow_hidden`
/// cannot supply it: gpui's content mask is a plain rectangle
/// (`window.rs:2119` — `ContentMask` has `bounds` and nothing else), so a
/// square inner box would fill the outer corners instead of being cut by them.
pub(crate) fn color_swatch(
    cx: &App,
    id: String,
    hex: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let rgb = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    // The ring is doubled when this chip is the chosen one.
    let (frame, ring) = match selected {
        true => (cx.theme().foreground, px(2.)),
        false => (cx.theme().border, px(1.)),
    };
    // Concentric radii: pulling the inner radius in by the ring width keeps the
    // two curves parallel, which is what a real border does.
    let inner_radius = (cx.theme().radius - ring).max(px(0.));

    div()
        .id(id)
        .cursor_pointer()
        .size(px(19.2))
        .flex_shrink_0()
        .rounded(cx.theme().radius)
        .p(ring)
        .bg(frame)
        .when(!selected, |this| {
            this.hover(|this| this.bg(cx.theme().muted_foreground))
        })
        .child(
            div()
                .size_full()
                .rounded(inner_radius)
                .bg(gpui_kit::rgb(rgb)),
        )
        .on_click(on_click)
}

/// A passive square colour chip in the shared palette style: the
/// [`color_swatch`] look without the interaction, for rows whose click
/// belongs to something larger — the tag colour menu, where the row picks
/// the colour and the chip only shows it. Same frame-as-padding trick and
/// concentric radii; see [`color_swatch`] for why the frame is not a
/// `border`.
pub(crate) fn color_chip(hex: &str, cx: &App) -> Div {
    let rgb = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    let ring = px(1.);
    let inner_radius = (cx.theme().radius - ring).max(px(0.));
    div()
        .size(px(16.))
        .flex_shrink_0()
        .rounded(cx.theme().radius)
        .p(ring)
        .bg(cx.theme().border)
        .child(
            div()
                .size_full()
                .rounded(inner_radius)
                .bg(gpui_kit::rgb(rgb)),
        )
}

/// Payload for internal drag & drop of one or many selected assets.
#[derive(Debug, Clone)]
pub struct AssetsDrag(pub Vec<Uuid>);

/// Payload for dragging a collection row (reparent / reorder in the tree).
#[derive(Debug, Clone)]
pub struct CollectionDrag(pub Uuid);

/// Payload for dragging a smart-collection row onto another row (nest it) or
/// onto the section header (back to the top level).
#[derive(Debug, Clone)]
pub struct SmartDrag(pub Uuid);

/// Reveal a file or directory in the platform file manager. Where the
/// platform supports it the file is selected (Windows/macOS); on Linux the
/// containing directory opens instead.
pub(crate) fn reveal_path(path: &std::path::Path) {
    let is_file = path.is_file();
    let _ = if cfg!(target_os = "windows") {
        if is_file {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(path)
                .spawn()
        } else {
            std::process::Command::new("explorer").arg(path).spawn()
        }
    } else if cfg!(target_os = "macos") {
        if is_file {
            std::process::Command::new("open")
                .arg("-R")
                .arg(path)
                .spawn()
        } else {
            std::process::Command::new("open").arg(path).spawn()
        }
    } else {
        let dir = if is_file {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
}

// ---------------------------------------------------------------------------
// Live font previews (grid cells / list rows / inspector)
// ---------------------------------------------------------------------------

/// Families already registered with the process text system. Registration
/// is global and permanent for the session, so one shared set serves the
/// inspector and every grid cell.
static REGISTERED_FONTS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

/// Register the font file behind `family` with the process text system so
/// `.font_family(family)` resolves to it. Best-effort: returns `false` when
/// the path is missing or unparseable, and callers fall back to the static
/// specimen card.
pub(crate) fn ensure_font_registered(
    family: &str,
    blob: Option<&std::path::Path>,
    cx: &mut App,
) -> bool {
    let set = REGISTERED_FONTS.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    if trove_core::sync::lock(set).contains(family) {
        return true;
    }
    let Some(path) = blob else {
        return false;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Owned(bytes)])
        .is_ok()
    {
        trove_core::sync::lock(set).insert(family.to_string());
        true
    } else {
        false
    }
}

/// (family, weight) faces already registered through
/// [`ensure_font_at_weight`]. Separate from [`REGISTERED_FONTS`]: a patched
/// face is one more candidate under the same family, and the weight picker
/// may register several over a session — bounded by the 100–900 grid.
static REGISTERED_FONT_WEIGHTS: OnceLock<Mutex<std::collections::HashSet<(String, u16)>>> =
    OnceLock::new();

/// Register `family` so that requesting `.font_weight(weight)` really
/// renders that weight of a **variable** font.
///
/// The text system's face matcher scores candidates by their *static*
/// weight and then renders at the face's own weight — the requested one
/// never reaches the renderer. The way through is the variable axis
/// itself: cosmic-text instantiates the requested weight along `wght`
/// (clamped to the axis), so a face whose `OS/2.usWeightClass` says
/// `weight` both wins the match and renders there. This registers a patched
/// copy of the file with exactly that header field rewritten — two bytes in
/// a scratch copy, the original on disk untouched.
pub(crate) fn ensure_font_at_weight(
    family: &str,
    blob: Option<&std::path::Path>,
    weight: u16,
    cx: &mut App,
) -> bool {
    let set = REGISTERED_FONT_WEIGHTS.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    if trove_core::sync::lock(set).contains(&(family.to_string(), weight)) {
        return true;
    }
    let Some(path) = blob else {
        return false;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Some(patched) = patch_os2_weight(&bytes, weight) else {
        return false;
    };
    if cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Owned(patched)])
        .is_ok()
    {
        trove_core::sync::lock(set).insert((family.to_string(), weight));
        true
    } else {
        false
    }
}

/// A copy of `bytes` whose `OS/2.usWeightClass` reads `weight`, or `None`
/// when the input is not a parseable sfnt with an OS/2 table. Everything
/// else — including the checksums, which nothing downstream validates — is
/// byte-for-byte the input.
pub(crate) fn patch_os2_weight(bytes: &[u8], weight: u16) -> Option<Vec<u8>> {
    // sfnt header: version (4) + numTables (2) + search fields (6), then
    // 16-byte table records of tag, checksum, offset, length.
    if bytes.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    let mut patched = bytes.to_vec();
    for entry in 0..num_tables {
        let record = 12 + entry * 16;
        let end = record + 16;
        if end > patched.len() {
            return None;
        }
        if &patched[record..record + 4] == b"OS/2" {
            // usWeightClass sits after the version and xAvgCharWidth fields.
            let offset = u32::from_be_bytes([
                patched[record + 8],
                patched[record + 9],
                patched[record + 10],
                patched[record + 11],
            ]) as usize
                + 4;
            let field = offset..offset + 2;
            if field.end > patched.len() {
                return None;
            }
            patched[field].copy_from_slice(&weight.to_be_bytes());
            return Some(patched);
        }
    }
    None
}

/// One live specimen line for a registered font: the built-in sample text
/// rendered in the font itself, centered on a soft card background, single
/// row. The caller sizes it (grid cells stretch, list leads get fixed dims).
pub(crate) fn font_live_preview(family: &str, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(cx.theme().secondary)
        .child(
            div()
                .font_family(family.to_string())
                .whitespace_nowrap()
                .text_color(cx.theme().foreground)
                .child(trove_core::media::thumb::DEFAULT_FONT_SAMPLE),
        )
}

/// A grid font cell, fontmatrix style: the built-in specimen rendered in the
/// font itself as three stacked rows — Latin on top, CJK in the middle,
/// digits at the bottom, the same rows the rasterized font card stacks —
/// with a small UI-font family label pinned to the top-left corner (the
/// "subtitled preview" mode), so every specimen stays attributable.
pub(crate) fn font_specimen_card(family: &str, cx: &App) -> Div {
    div()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .bg(cx.theme().secondary)
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .w_full()
                .h_full()
                .children(
                    trove_core::media::thumb::default_specimen_rows().map(|line| {
                        div()
                            .font_family(family.to_string())
                            .whitespace_nowrap()
                            .truncate()
                            .max_w_full()
                            .text_color(cx.theme().foreground)
                            .child(line)
                    }),
                ),
        )
        .child(
            div()
                .absolute()
                .top(px(3.))
                .left(px(7.))
                .right(px(7.))
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(family.to_string()),
        )
}

// ---------------------------------------------------------------------------
// Animated image playback (GIF / animated WebP / APNG)
// ---------------------------------------------------------------------------

/// Decoded APNG previews, keyed by source path. Both hits and misses are
/// cached so repeated renders (and re-opens of the preview dialog) never
/// re-decode; the map is capped and wholesale-cleared when it overflows.
static APNG_CACHE: std::sync::OnceLock<
    Mutex<HashMap<PathBuf, Option<Arc<gpui_kit::RenderImage>>>>,
> = std::sync::OnceLock::new();

/// Pick the image source for a preview of a *potentially animated* image:
///
/// - GIF / animated WebP — hand the original file to gpui, which decodes
///   all frames through the asset system and plays them natively.
/// - APNG — gpui only renders the first PNG frame, so decode the animation
///   ourselves into a multi-frame [`gpui_kit::RenderImage`] (cached).
///
/// `mime` is the asset's MIME type, `original` the full-size file (blob or
/// linked source path). Returns `None` when the file is not animated (or
/// missing), so callers fall back to the static thumbnail.
pub(crate) fn animated_preview_source(
    mime: Option<&str>,
    original: Option<&std::path::Path>,
) -> Option<gpui_kit::ImageSource> {
    use gpui_kit::ImageSource;

    let path = original?;
    if !path.is_file() {
        return None;
    }
    match mime {
        // gpui plays these natively from a file source.
        Some("image/gif") | Some("image/webp") => Some(ImageSource::from(path.to_path_buf())),
        // APNG needs manual frame extraction (mime for PNG is image/png).
        Some("image/png") => {
            let cache = APNG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
            let mut cache = trove_core::sync::lock(cache);
            if let Some(hit) = cache.get(path) {
                return hit.clone().map(ImageSource::Render);
            }
            if cache.len() >= 16 {
                cache.clear();
            }
            let decoded = decode_apng(path);
            cache.insert(path.to_path_buf(), decoded.clone());
            decoded.map(ImageSource::Render)
        }
        _ => None,
    }
}

/// Decode every APNG frame into BGRA [`image::Frame`]s. Gives up (returns
/// `None`, i.e. "show static") on any decode error, single-frame files, or
/// when the animation would exceed a 256 MB RGBA budget.
fn decode_apng(path: &std::path::Path) -> Option<Arc<gpui_kit::RenderImage>> {
    use image::AnimationDecoder as _;
    use image::ImageDecoder as _;

    const FRAME_BUDGET: u64 = 256 * 1024 * 1024 / 4; // 256 MB worth of RGBA

    let file = std::fs::File::open(path).ok()?;
    let decoder = image::codecs::png::PngDecoder::new(std::io::BufReader::new(file)).ok()?;
    if !decoder.is_apng().ok()? {
        return None;
    }
    let (w, h) = decoder.dimensions();
    let frame_px = u64::from(w) * u64::from(h);
    let mut frames = smallvec::SmallVec::new();
    let mut total = 0u64;
    for frame in decoder.apng().ok()?.into_frames() {
        let mut frame = frame.ok()?;
        total += frame_px;
        if total > FRAME_BUDGET {
            return None;
        }
        // RGBA -> BGRA, the layout gpui's renderer expects.
        for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        frames.push(frame);
    }
    if frames.len() <= 1 {
        return None;
    }
    Some(Arc::new(gpui_kit::RenderImage::new(frames)))
}

#[cfg(test)]
mod tests {
    use super::patch_os2_weight;

    /// A minimal sfnt: header + one table record naming `OS/2`, plus a
    /// six-byte OS/2 table whose usWeightClass (offset +4) is the field the
    /// patch rewrites. Everything else must survive untouched.
    fn sfnt_with_os2(weight: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // sfntVersion
        bytes.extend_from_slice(&1u16.to_be_bytes()); // numTables
        bytes.extend_from_slice(&0u16.to_be_bytes()); // searchRange
        bytes.extend_from_slice(&0u16.to_be_bytes()); // entrySelector
        bytes.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
        bytes.extend_from_slice(b"OS/2");
        bytes.extend_from_slice(&0u32.to_be_bytes()); // checksum
        bytes.extend_from_slice(&28u32.to_be_bytes()); // offset
        bytes.extend_from_slice(&6u32.to_be_bytes()); // length
        bytes.extend_from_slice(&1u16.to_be_bytes()); // OS/2 version
        bytes.extend_from_slice(&0i16.to_be_bytes()); // xAvgCharWidth
        bytes.extend_from_slice(&weight.to_be_bytes()); // usWeightClass
        bytes
    }

    #[test]
    fn the_weight_field_is_rewritten_and_nothing_else_is() {
        let bytes = sfnt_with_os2(400);
        let patched = patch_os2_weight(&bytes, 700).expect("a parseable sfnt");
        assert_eq!(patched.len(), bytes.len(), "same size");
        assert_eq!(&patched[..28], &bytes[..28], "header and record untouched");
        assert_eq!(
            u16::from_be_bytes([patched[32], patched[33]]),
            700,
            "usWeightClass reads back the requested weight"
        );
        assert_eq!(patched[28], bytes[28], "OS/2 version byte untouched");
        assert_eq!(patched[30], bytes[30], "xAvgCharWidth byte untouched");
        // The input is never touched.
        assert_eq!(u16::from_be_bytes([bytes[32], bytes[33]]), 400);
    }

    /// The offset math against a real font in the tree: the patched file
    /// must still parse, and its `OS/2` weight must read back as requested.
    /// (ttf-parser is a trove-core dependency; here it only re-reads two
    /// bytes the patch moved.)
    #[test]
    fn a_real_font_survives_the_patch() {
        let Some(path) = std::fs::read_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../reference/Serpent/resources/fonts"),
        )
        .ok()
        .and_then(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "ttf"))
        }) else {
            eprintln!("no reference fonts on this tree; skipping");
            return;
        };
        let bytes = std::fs::read(&path).expect("reference font readable");
        let Some(patched) = patch_os2_weight(&bytes, 650) else {
            panic!("a real ttf must patch");
        };
        assert_ne!(&patched[..], &bytes[..], "the weight byte moved");
        let read_weight = |data: &[u8]| {
            let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
            for entry in 0..num_tables {
                let record = 12 + entry * 16;
                if &data[record..record + 4] == b"OS/2" {
                    let offset = u32::from_be_bytes([
                        data[record + 8],
                        data[record + 9],
                        data[record + 10],
                        data[record + 11],
                    ]) as usize;
                    return u16::from_be_bytes([data[offset + 4], data[offset + 5]]);
                }
            }
            panic!("no OS/2 in the real font");
        };
        assert_eq!(read_weight(&patched), 650);
        assert_ne!(read_weight(&bytes), 650, "the original reads differently");
    }

    #[test]
    fn a_font_without_an_os2_table_is_not_patched() {
        let mut bytes = sfnt_with_os2(400);
        // Retag the record: the table is there, `OS/2` is not.
        bytes[12..16].copy_from_slice(b"head");
        assert_eq!(patch_os2_weight(&bytes, 700), None);
        assert_eq!(patch_os2_weight(&bytes[..8], 700), None, "truncated");
    }
}
