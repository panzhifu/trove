//! The subtitle view: an asset's `.srt` shown as cues in the main area, with
//! copy, edit and save.
//!
//! The sidecar is the artifact the user keeps — the transcript column behind
//! it is only what search reads — so this is where subtitles are looked at and
//! changed. It follows the preview contract the workspace already speaks: a
//! self-contained entity the host hands its whole content area to, opened by
//! Enter and left by Escape. Its tools (copy, edit, save, close) live in the
//! host's title bar, exactly as every other preview's do.
//!
//! Two modes, one document. The cue list reads the `SRT`; the edit mode is a
//! plain text editor over the same document, because a subtitle's timeline
//! and its words are both text, and the raw file is the honest thing to edit
//! when either can change. Saving writes the buffer back verbatim through the
//! core's atomic sidecar write, refreshes the sidecar's library record, and
//! re-parses the cue list so the two views never disagree.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::*;
use trove_core::media::subtitles::{self, Cue};
use uuid::Uuid;

use crate::components::scrollbar;
use crate::library::LibraryController;

/// What the editor tells its host, the workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubtitleEvent {
    /// The user left the subtitle view.
    Closed,
}

/// The main-area subtitle host. Its controls live in the host's title bar
/// (see `WorkspacePanel::title_suffix`), so the content area is nothing but
/// the cues — the same contract the asset preview offers.
pub(crate) struct SubtitleEditor {
    controller: Entity<LibraryController>,
    title: String,
    srt_path: std::path::PathBuf,
    /// The parsed cues behind the list view.
    cues: Vec<Cue>,
    /// The document as last loaded or saved; the edit buffer starts here.
    raw: String,
    /// Whether a `.srt` exists on disk (as opposed to an estimate from the
    /// transcript that has not been written yet).
    has_file: bool,
    /// Built once, on the first frame that can give it a window.
    editor: Option<Entity<EditorState>>,
    /// Edit mode: the editor accepts keystrokes and the save button shows.
    editing: bool,
    /// The status line — "saved", a failure. Session-only.
    note: Option<String>,
}

impl EventEmitter<SubtitleEvent> for SubtitleEditor {}

impl SubtitleEditor {
    /// Open the view for `asset_id`, or `None` when the asset or its file is
    /// gone. Reads the sidecar when it exists, and falls back to the
    /// transcript's estimated cues when it does not — the just-transcribed
    /// case, before the auto-save has landed, and the legacy case alike.
    pub(crate) fn spawn(
        controller: &Entity<LibraryController>,
        asset_id: Uuid,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        let (title, srt_path, raw, cues, has_file) = {
            let ctl = controller.read(cx);
            let asset = ctl.library.asset(asset_id).ok().flatten()?;
            let disk_path = ctl.library.asset_file(asset_id)?;
            let srt_path = disk_path.with_extension("srt");
            let has_file = srt_path.is_file();
            let (raw, cues) = if has_file {
                match std::fs::read_to_string(&srt_path) {
                    Ok(text) => {
                        let cues = subtitles::parse(&text);
                        (text, cues)
                    }
                    Err(_) => (String::new(), Vec::new()),
                }
            } else {
                let transcript = ctl
                    .library
                    .transcript(asset_id)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let cues = subtitles::cues(&transcript, asset.duration_ms);
                (subtitles::to_srt(&cues), cues)
            };
            let title = asset.title.clone().unwrap_or_else(|| asset.file_stem());
            (title, srt_path, raw, cues, has_file)
        };
        Some(cx.new(|_| Self {
            controller: controller.clone(),
            title,
            srt_path,
            cues,
            raw,
            has_file,
            editor: None,
            editing: false,
            note: None,
        }))
    }

    /// The asset name the host's title bar shows.
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    /// Whether the view is in edit mode (the host draws the edit button's
    /// toggled state and the save button from this).
    pub(crate) fn editing(&self) -> bool {
        self.editing
    }

    /// What the copy button hands over: the live buffer while editing
    /// (unsaved work included), the saved document otherwise.
    pub(crate) fn copy_text(&self, cx: &App) -> String {
        if self.editing {
            self.editor
                .as_ref()
                .map(|editor| editor.read(cx).value().to_string())
                .unwrap_or_else(|| self.raw.clone())
        } else {
            self.raw.clone()
        }
    }

    /// Announce that the view should close; the host drops it in response.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(SubtitleEvent::Closed);
    }

    /// Enter or leave edit mode.
    pub(crate) fn toggle_editing(&mut self, cx: &mut Context<Self>) {
        self.editing = !self.editing;
        self.note = None;
        cx.notify();
    }

    /// Write the buffer back over the sidecar. The parsed cues are refreshed
    /// from what was written, so a malformed edit that happens to parse
    /// differently is reflected rather than silently kept. The sidecar's
    /// library record is refreshed too, so an asset already in the grid keeps
    /// pointing at the current text.
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        let cues = subtitles::parse(&text);
        if cues.is_empty() && !text.trim().is_empty() {
            self.note = Some(rust_i18n::t!("subtitle.unparsable").to_string());
            cx.notify();
            return;
        }
        match subtitles::write_document(&self.srt_path, &text) {
            Ok(()) => {
                self.cues = cues;
                self.raw = text;
                self.has_file = true;
                self.editing = false;
                self.note = Some(rust_i18n::t!("subtitle.saved").to_string());
                crate::library::jobs::ensure_subtitle_asset(&self.controller, &self.srt_path, cx);
            }
            Err(error) => {
                self.note = Some(
                    rust_i18n::t!("subtitle.save_failed", error = error.to_string()).to_string(),
                );
            }
        }
        cx.notify();
    }
}

impl Render for SubtitleEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // The editor state wants a `Window`, which a spawn does not have, so
        // it is built here on the first frame and fed the document once. A
        // view that opened with no text builds it when editing starts, so a
        // blank document can still be written from scratch.
        if self.editor.is_none() && (!self.raw.is_empty() || self.editing) {
            let raw = self.raw.clone();
            let entity = cx.new(|cx| {
                EditorState::new(window, cx)
                    .line_number(false)
                    .default_value(raw)
            });
            self.editor = Some(entity);
        }

        let status = if self.has_file {
            rust_i18n::t!("subtitle.count", count = self.cues.len()).to_string()
        } else {
            rust_i18n::t!("subtitle.estimated", count = self.cues.len()).to_string()
        };
        let status = match &self.note {
            Some(note) => format!("{status} · {note}"),
            None => status,
        };

        let body: AnyElement = if self.editing {
            match self.editor.clone() {
                Some(editor) => Editor::new(&editor)
                    .appearance(false)
                    .bordered(false)
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .into_any_element(),
                None => div().into_any_element(),
            }
        } else if self.cues.is_empty() {
            div()
                .p_4()
                .text_sm()
                .text_color(muted)
                .child(rust_i18n::t!("subtitle.empty").to_string())
                .into_any_element()
        } else {
            let rows = self.cues.iter().enumerate().map(|(index, cue)| {
                let lines: Vec<String> = cue.text.split('\n').map(str::to_string).collect();
                v_flex()
                    .gap_0p5()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .flex_none()
                                    .child(format!("{}", index + 1)),
                            )
                            .child(div().text_xs().text_color(muted).flex_none().child(format!(
                                "{} → {}",
                                subtitles::format_timestamp(cue.start_ms),
                                subtitles::format_timestamp(cue.end_ms)
                            ))),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .children(lines.into_iter().map(|line| div().child(line))),
                    )
            });
            scrollbar::vertical(v_flex().flex_1().min_h_0().w_full().p_1().gap_1())
                .child(v_flex().gap_1().children(rows))
                .into_any_element()
        };

        v_flex()
            .size_full()
            .min_w_0()
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .text_ellipsis()
                    .child(status),
            )
            .child(body)
            .into_any_element()
    }
}
