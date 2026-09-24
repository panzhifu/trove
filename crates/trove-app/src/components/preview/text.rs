//! The text viewer: a file's own characters, read-only, with line numbers.
//!
//! Serpent's text viewer is a `<textarea>` plus a `<pre>` column of numbers, and
//! it has no syntax highlighting and no virtualised lines. Trove has gpui-kit's
//! `Editor` — the code-editor element, with a gap-tree line layout, a real line
//! number gutter, selection and copy — so the viewer is that element in
//! read-only mode rather than a re-implementation of one.
//!
//! Two of its rules are deliberate:
//!
//! * **The read is capped and the cap is announced.** One MiB, decided in
//!   `trove_core::media::text`. The notice says so rather than letting a long
//!   log look like a short file.
//! * **Nothing here writes.** The editor is read-only, so a truncated buffer can
//!   never be saved back over its source — which is a live bug in Serpent, where
//!   `save()` checks whether the asset is editable but not whether what is on
//!   screen is the whole file.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName as MediaIcon;
use gpui_kit::base::v_flex;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trove_core::media::text::{self, TextContent};

use super::AssetPreviewData;

/// Whether this preview should open the text viewer rather than a still.
///
/// Keyed on the extension, not on `AssetKind`: the text family spans `Document`
/// (`.txt`, `.md`, `.csv`) and `Other` (`.rs`, `.json`, `.html`) alike, and a
/// kind would have to be added and back-filled to express "has characters to
/// read".
pub(super) fn is_text(data: &AssetPreviewData) -> bool {
    let ext = data
        .original
        .as_ref()
        .and_then(|path| path.extension())
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    text::is_text_ext(&ext)
}

/// Open the viewer for `data`, or `None` when there is no file to read. The
/// content itself arrives on a background task: a megabyte of text and an
/// encoding guess are not a frame's worth of work.
pub(super) fn spawn_viewer(data: &AssetPreviewData, cx: &mut App) -> Option<Entity<TextViewer>> {
    let path = data.original.clone()?;
    if !path.is_file() {
        return None;
    }
    let viewer = cx.new(|_| TextViewer {
        loaded: Loaded::Pending,
        editor: None,
        wrapped: Rc::new(Cell::new(true)),
    });
    let entity = viewer.clone();
    cx.spawn(async move |cx| {
        let content = cx
            .background_executor()
            .spawn(async move { text::read_for_viewer(&path) })
            .await;
        entity.update(cx, |this, cx| {
            this.loaded = match content {
                None => Loaded::Missing,
                Some(content) if content.binary => Loaded::Binary,
                Some(content) => Loaded::Ready(content),
            };
            cx.notify();
        });
    })
    .detach();
    Some(viewer)
}

/// What the viewer knows about the file so far.
enum Loaded {
    Pending,
    /// No file behind the asset, or it vanished after the preview opened.
    Missing,
    /// Readable bytes, but not characters: NULs where text should be.
    Binary,
    Ready(TextContent),
}

/// The live text view: the loaded content, the editor that shows it, and the
/// session's wrap preference.
pub(crate) struct TextViewer {
    loaded: Loaded,
    /// Built once, on the first frame that has content to put in it: the editor
    /// state wants a `Window`, which a spawn does not have.
    editor: Option<Entity<EditorState>>,
    /// Soft wrap, on by default. Session-only on purpose — the same answer
    /// Serpent gives, and the honest one for a viewer that is not a setting.
    wrapped: Rc<Cell<bool>>,
}

impl TextViewer {
    /// Flip wrap and tell the editor, which owns the line layout.
    fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wrapped = !self.wrapped.get();
        self.wrapped.set(wrapped);
        if let Some(editor) = &self.editor {
            editor.update(cx, |state, cx| state.set_soft_wrap(wrapped, window, cx));
        }
        cx.notify();
    }
}

impl Render for TextViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let wrapped = self.wrapped.get();
        let ready = matches!(self.loaded, Loaded::Ready(_));
        // The state is built here rather than at spawn because it needs the
        // window, and the text is handed to it exactly once.
        if ready && self.editor.is_none() {
            let content = match &self.loaded {
                Loaded::Ready(content) => content.text.clone(),
                _ => String::new(),
            };
            let wrap = wrapped;
            let entity = cx.new(move |cx| {
                EditorState::new(window, cx)
                    .line_number(true)
                    .soft_wrap(wrap)
                    .default_value(content)
            });
            self.editor = Some(entity);
        }
        let notice = match &self.loaded {
            Loaded::Pending => Some(rust_i18n::t!("text.loading").to_string()),
            Loaded::Missing => Some(rust_i18n::t!("text.missing").to_string()),
            Loaded::Binary => Some(rust_i18n::t!("text.binary").to_string()),
            Loaded::Ready(content) => {
                // Encoding and line count belong in the header, not the
                // inspector: they are properties of *this* read, and the
                // inspector is a different panel that may not even be open.
                let stats = rust_i18n::t!(
                    "text.stats",
                    encoding = content.encoding,
                    lines = content.line_count
                )
                .to_string();
                let truncated = content.truncated.then(|| {
                    rust_i18n::t!(
                        "text.truncated",
                        shown = format_bytes(content.bytes_read),
                        total = format_bytes(content.total_bytes as usize)
                    )
                    .to_string()
                });
                Some(match truncated {
                    Some(cut) => format!("{stats} · {cut}"),
                    None => stats,
                })
            }
        };
        v_flex()
            .size_full()
            .min_w_0()
            .gap_1()
            .p_2()
            .child(h_row(
                div()
                    .text_xs()
                    .text_color(muted)
                    .text_ellipsis()
                    .child(notice.unwrap_or_default())
                    .into_any_element(),
                ready.then(|| {
                    let host = cx.entity();
                    Button::new("text-toggle-wrap")
                        .ghost()
                        .xsmall()
                        .icon(MediaIcon::TextWrap)
                        .toggled(wrapped)
                        .tooltip(rust_i18n::t!("text.wrap").to_string())
                        .on_click(move |_, window, cx| {
                            host.update(cx, |this, cx| this.toggle_wrap(window, cx));
                        })
                        .into_any_element()
                }),
            ))
            .when(ready, |stage| {
                let Some(editor) = self.editor.clone() else {
                    return stage;
                };
                stage.child(
                    Editor::new(&editor)
                        .readonly(true)
                        .appearance(false)
                        .bordered(false)
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                )
            })
            .into_any_element()
    }
}

/// A byte count in the unit a person reads it in.
fn format_bytes(bytes: usize) -> String {
    const KB: usize = 1024;
    if bytes < KB {
        return format!("{bytes} B");
    }
    if bytes < KB * KB {
        return format!("{:.0} KiB", bytes as f64 / KB as f64);
    }
    format!("{:.1} MiB", bytes as f64 / (KB * KB) as f64)
}

/// A header row that right-aligns its optional control without the label
/// stealing the space the button needs.
fn h_row(label: AnyElement, control: Option<AnyElement>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .w_full()
        .child(label)
        .children(control)
        .into_any_element()
}
