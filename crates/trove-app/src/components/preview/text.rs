//! The text viewer: a file's own characters, with line numbers — read-only
//! by default, editable where writing is honest.
//!
//! Serpent's text viewer is a `<textarea>` plus a `<pre>` column of numbers, and
//! it has no syntax highlighting and no virtualised lines. Trove has gpui-kit's
//! `Editor` — the code-editor element, with a gap-tree line layout, a real line
//! number gutter, selection and copy — so the viewer is that element in
//! read-only mode rather than a re-implementation of one.
//!
//! Its rules, each deliberate:
//!
//! * **The read is capped and the cap is announced.** One MiB, decided in
//!   `trove_core::media::text`. The notice says so rather than letting a long
//!   log look like a short file.
//! * **Writing is the exception, and it is fenced.** The edit toggle exists
//!   only where a save can be honest: a *linked* asset (the file is the
//!   user's, not a content-addressed blob) whose read was whole — a truncated
//!   buffer saved back over its source is data loss, which is a live bug in
//!   Serpent, where `save()` checks whether the asset is editable but not
//!   whether what is on screen is the whole file. The save itself is a
//!   compare-and-swap on the file's mtime and size as the read observed
//!   them (`Library::save_linked_text`), so a file an outside editor touched
//!   in the meantime is refused, not overwritten.
//! * **Stored assets stay read-only.** Their bytes live in a blob the
//!   record's content hash names; overwriting it in place would leave the
//!   record describing content the file no longer has.

use std::cell::Cell;
use std::rc::Rc;
use std::time::SystemTime;

use gpui_kit::assets::IconName as MediaIcon;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::{ActiveTheme, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trove_core::library::TextSaveOutcome;
use trove_core::media::text::{self, TextContent};

use super::AssetPreviewData;
use uuid::Uuid;

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
/// encoding guess are not a frame's worth of work. The library handle is
/// `None` for previews with no record behind them (a virtual system font),
/// and those viewers never write.
pub(super) fn spawn_viewer(
    data: &AssetPreviewData,
    library: Option<trove_core::library::Library>,
    cx: &mut App,
) -> Option<Entity<TextViewer>> {
    let path = data.original.clone()?;
    if !path.is_file() {
        return None;
    }
    let viewer = cx.new(|_| TextViewer {
        loaded: Loaded::Pending,
        editor: None,
        wrapped: Rc::new(Cell::new(true)),
        editing: false,
        save_note: None,
        save_target: None,
    });
    let entity = viewer.clone();
    // A linked asset the panel says is writable gets a save target: the
    // compare-and-swap anchor is the stat taken *before* the read, so the
    // save checks the file against the same moment the text describes. The
    // library handle clones out here — it is a cheap `Arc`, and the save
    // task must not borrow the controller across an await.
    let writable = data.write_back && data.asset_id.is_some() && library.is_some();
    let asset_id = data.asset_id;
    cx.spawn(async move |cx| {
        let anchor = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok().map(|mtime| (mtime, m.len())));
        let read_path = path.clone();
        let content = cx
            .background_executor()
            .spawn(async move { text::read_for_viewer(&read_path) })
            .await;
        entity.update(cx, |this, cx| {
            this.loaded = match content {
                None => Loaded::Missing,
                Some(content) if content.binary => Loaded::Binary,
                Some(content) => Loaded::Ready(content),
            };
            if writable
                && let (Some(asset_id), Some((mtime, size))) = (asset_id, anchor)
                && let Loaded::Ready(content) = &this.loaded
                && !content.truncated
                && let Some(library) = library
            {
                this.save_target = Some(SaveTarget {
                    library,
                    asset_id,
                    path,
                    encoding: content.encoding,
                    bom: content.bom,
                    mtime,
                    size,
                });
            }
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

/// Everything a save needs, gathered where it could be verified: the
/// library handle (a cheap `Arc`), the record to write against, the file
/// itself, the encoding the read detected, and the mtime-and-size pair the
/// save swaps against. Built only for a whole-file read of a linked asset;
/// `None` is the read-only story.
#[derive(Clone)]
struct SaveTarget {
    library: trove_core::library::Library,
    asset_id: Uuid,
    path: std::path::PathBuf,
    encoding: &'static str,
    bom: bool,
    mtime: SystemTime,
    size: u64,
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
    /// Edit mode: the editor accepts keystrokes and the save button shows.
    /// Entered only from the pencil, which exists only where a save could
    /// actually run.
    editing: bool,
    /// What the last save reported, for the header line — "saved", "nothing
    /// to save", the conflict's reload advice, or a failure. Session-only.
    save_note: Option<String>,
    /// Where a save would go, when it could be established at all.
    save_target: Option<SaveTarget>,
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

    /// Enter or leave edit mode. Leaving after content changed would strand
    /// the typing in a read-only editor — so leaving is the save's job to
    /// allow (the pencil only toggles *in*; the save button or a failed CAS
    /// decide what happens to the buffer).
    fn set_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
        self.editing = editing;
        self.save_note = None;
        cx.notify();
    }

    /// Save the buffer over the linked source. Synchronous on purpose:
    /// the whole call is bounded by the viewer's own one-megabyte read cap
    /// (encode, one file write, one hash, one eleven-line card), the same
    /// budget a settings write pays — and the library handle is not `Send`
    /// (the store connection is `Rc`), so the background-executor route is
    /// structurally closed. The outcome lands in the header line, and a
    /// successful write re-anchors the CAS to the file's new mtime so a
    /// second save does not refuse itself.
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.save_target.clone() else {
            return;
        };
        let Some(editor) = &self.editor else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let outcome = target.library.save_linked_text(
            target.asset_id,
            &text,
            target.encoding,
            target.bom,
            target.mtime,
            target.size,
        );
        // The editor keeps the typed buffer; nothing here rebuilds it.
        self.editing = false;
        match &outcome {
            Ok(outcome) => {
                // A write moved the file: the CAS anchor is stale the
                // moment it lands, so re-stat and re-arm it.
                self.save_note = Some(match outcome {
                    TextSaveOutcome::Written | TextSaveOutcome::Unchanged => {
                        rust_i18n::t!("text.saved").to_string()
                    }
                    TextSaveOutcome::Conflict => rust_i18n::t!("text.save_conflict").to_string(),
                    TextSaveOutcome::NotWritable => rust_i18n::t!("text.save_refused").to_string(),
                });
                if *outcome == TextSaveOutcome::Written
                    && let Ok(stat) = std::fs::metadata(&target.path)
                    && let Ok(mtime) = stat.modified()
                {
                    self.save_target = Some(SaveTarget {
                        mtime,
                        size: stat.len(),
                        ..target
                    });
                }
            }
            Err(error) => {
                self.save_note =
                    Some(rust_i18n::t!("text.save_failed", error = error.to_string()).to_string());
            }
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
                let mut parts = vec![stats];
                if let Some(cut) = truncated {
                    parts.push(cut);
                }
                if let Some(note) = self.save_note.clone() {
                    parts.push(note);
                }
                Some(parts.join(" · "))
            }
        };
        // The pencil exists only where a save could actually run — a whole
        // read of a linked asset with the record reachable. When the read
        // was truncated the button shows disabled with the reason, rather
        // than vanishing: a silently missing control reads as a bug.
        let truncated = matches!(&self.loaded, Loaded::Ready(c) if c.truncated);
        let can_write = self.save_target.is_some();
        let controls = ready.then(|| {
            let host = cx.entity();
            let mut row = Vec::new();
            row.push(
                Button::new("text-toggle-wrap")
                    .ghost()
                    .xsmall()
                    .icon(MediaIcon::TextWrap)
                    .toggled(wrapped)
                    .tooltip(rust_i18n::t!("text.wrap").to_string())
                    .on_click({
                        let host = host.clone();
                        move |_, window, cx| {
                            host.update(cx, |this, cx| this.toggle_wrap(window, cx));
                        }
                    })
                    .into_any_element(),
            );
            if can_write {
                row.push(
                    Button::new("text-toggle-edit")
                        .ghost()
                        .xsmall()
                        .icon(MediaIcon::Pencil)
                        .toggled(self.editing)
                        .tooltip(rust_i18n::t!("text.edit").to_string())
                        .on_click({
                            let host = host.clone();
                            move |_, _, cx| {
                                host.update(cx, |this, cx| this.set_editing(!this.editing, cx));
                            }
                        })
                        .into_any_element(),
                );
            } else if truncated {
                row.push(
                    Button::new("text-toggle-edit")
                        .ghost()
                        .xsmall()
                        .icon(MediaIcon::Pencil)
                        .disabled(true)
                        .tooltip(rust_i18n::t!("text.edit_truncated").to_string())
                        .into_any_element(),
                );
            }
            if self.editing {
                row.push(
                    Button::new("text-save")
                        .ghost()
                        .xsmall()
                        .icon(MediaIcon::Save)
                        .tooltip(rust_i18n::t!("text.save").to_string())
                        .on_click(move |_, _, cx| {
                            host.update(cx, |this, cx| this.save(cx));
                        })
                        .into_any_element(),
                );
            }
            row
        });
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
                controls.map(|row| h_flex().gap_1().children(row).into_any_element()),
            ))
            .when(ready, |stage| {
                let Some(editor) = self.editor.clone() else {
                    return stage;
                };
                stage.child(
                    Editor::new(&editor)
                        .readonly(!self.editing)
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
