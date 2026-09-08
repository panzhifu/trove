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
use trove_core::store::assets;

use crate::library::LibraryController;

/// Per-asset color-label palette (name ↔ swatch hex); the names match
/// `trove_core::model::COLOR_LABELS`.
pub(crate) const COLOR_LABEL_SWATCHES: &[(&str, &str)] = &[
    ("red", "#ef4444"),
    ("orange", "#f97316"),
    ("yellow", "#eab308"),
    ("green", "#22c55e"),
    ("blue", "#3b82f6"),
    ("purple", "#a855f7"),
];

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
    let conn = controller.library.store().conn();
    assets::query(conn, &AssetQuery::default())
        .map(|(total, _)| total)
        .unwrap_or(0)
}

pub(crate) fn trash_count(controller: &LibraryController) -> u64 {
    let conn = controller.library.store().conn();
    assets::query(
        conn,
        &AssetQuery {
            is_trashed: true,
            ..Default::default()
        },
    )
    .map(|(total, _)| total)
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

/// A round color chip in the shared palette style: hairline border, a
/// stronger ring when `selected`, hover feedback. Used by the smart-collection
/// palette and the Inspector's mined-color swatches.
pub(crate) fn color_swatch(
    cx: &App,
    id: String,
    hex: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let rgb = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    // Larger swatch for smoother edges (less visible aliasing) and a more
    // touch-friendly target.
    div()
        .id(id)
        .cursor_pointer()
        .size_6()
        .flex_shrink_0()
        .rounded_full()
        .bg(gpui_kit::rgb(rgb))
        .border_1()
        .border_color(cx.theme().border)
        .hover(|this| this.border_color(cx.theme().muted_foreground))
        .when(selected, |this| {
            this.border_2().border_color(cx.theme().foreground)
        })
        .on_click(on_click)
}

/// Payload for internal drag & drop of one or many selected assets.
#[derive(Debug, Clone)]
pub struct AssetsDrag(pub Vec<Uuid>);

/// Payload for dragging a collection row (reparent / reorder in the tree).
#[derive(Debug, Clone)]
pub struct CollectionDrag(pub Uuid);

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
    if set.lock().unwrap().contains(family) {
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
        set.lock().unwrap().insert(family.to_string());
        true
    } else {
        false
    }
}

/// Cached copy of the user's font sample text (config reads are file I/O,
/// and every visible font cell asks for it on every render). TTL keeps the
/// settings change visible without wiring invalidation.
fn cached_font_sample() -> String {
    static CACHE: OnceLock<Mutex<(std::time::Instant, String)>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| {
            Mutex::new((
                std::time::Instant::now() - std::time::Duration::from_secs(10),
                String::new(),
            ))
        })
        .lock()
        .unwrap();
    if cache.0.elapsed() > std::time::Duration::from_secs(2) {
        cache.1 = trove_core::config::AppConfig::load().font_sample_text();
        cache.0 = std::time::Instant::now();
    }
    cache.1.clone()
}

/// One live specimen line for a registered font: the sample text rendered
/// in the font itself, centered on a soft card background, single row. The
/// caller sizes it (grid cells stretch, list leads get fixed dims).
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
                .child(cached_font_sample()),
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
            let mut cache = cache.lock().unwrap();
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
