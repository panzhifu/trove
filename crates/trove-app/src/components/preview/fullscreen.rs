//! The fullscreen video window: a separate OS window hosting a fresh
//! player that continues where the main-area player was, and hands the
//! position back on the way out.
//!
//! Opening goes through the main-area player's
//! [`EnterFullscreen`][super::video::VideoPlayerEvent::EnterFullscreen]
//! event; leaving goes through the [`ExitVideoFullscreen`] action — both
//! the Esc key (bound in the `VideoFullscreen` context) and the control
//! row's exit button dispatch it, so there is a single exit path that owns
//! the window and the hand-back.

use std::path::PathBuf;

use gpui_kit::component::Root;
use gpui_kit::*;

use super::AssetPreviewPanel;
use super::video::{PlayerResume, VideoPlayer};
use crate::app::actions::ExitVideoFullscreen;

/// Restore size of the fullscreen window: where it would sit if it were
/// ever un-fullscreened.
const RESTORE_SIZE: Size<Pixels> = size(px(1280.), px(720.));

/// Open the fullscreen window for `path`. The player continues from
/// `resume`; leaving it hands the playback state back to the main-area
/// panel (`host`) and closes the window.
pub(crate) fn open(
    host: WeakEntity<AssetPreviewPanel>,
    path: PathBuf,
    resume: PlayerResume,
    cx: &mut App,
) {
    let Some(player) = super::video::spawn_fullscreen(path, resume, cx) else {
        return;
    };
    let options = gpui_kit::WindowOptions {
        window_bounds: Some(WindowBounds::Fullscreen(Bounds::centered(
            None,
            RESTORE_SIZE,
            cx,
        ))),
        ..crate::app::title_bar::window_options()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| FullscreenPlayer::new(host, player, cx));
        // The root is gpui-kit's `Root`, same as the main window: it owns
        // theme, tooltips and menu overlays for this window.
        cx.new(|cx| Root::new(view, window, cx))
    });
}

/// The fullscreen window's root view: a black stage hosting the player,
/// whose key context routes Esc to [`ExitVideoFullscreen`].
struct FullscreenPlayer {
    host: WeakEntity<AssetPreviewPanel>,
    player: Entity<VideoPlayer>,
    focus: FocusHandle,
    focused: bool,
}

impl FullscreenPlayer {
    fn new(
        host: WeakEntity<AssetPreviewPanel>,
        player: Entity<VideoPlayer>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            host,
            player,
            focus: cx.focus_handle(),
            focused: false,
        }
    }

    /// Leave fullscreen: hand the playback state back to the main-area
    /// player, free this window's frames, close the window. The preview
    /// then resumes exactly where this player was.
    fn exit(&mut self, _: &ExitVideoFullscreen, window: &mut Window, cx: &mut Context<Self>) {
        let (position_ms, playing) = self.player.read(cx).playback_state();
        if let Some(host) = self.host.upgrade() {
            host.update(cx, |panel, cx| {
                if let Some(video) = &panel.video {
                    video.update(cx, |player, cx| {
                        player.resume_from(position_ms, playing, cx);
                    });
                }
            });
        }
        // Frames live in this window's atlas; hand them back before the
        // window closes, or the last frame stays resident.
        self.player.update(cx, |player, _| player.release(window));
        window.remove_window();
    }
}

impl Focusable for FullscreenPlayer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FullscreenPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Take focus once so the VideoFullscreen key context routes Esc.
        if !self.focused {
            self.focused = true;
            cx.focus_self(window);
        }
        div()
            .size_full()
            .bg(black())
            .p_4()
            .key_context(crate::VIDEO_FULLSCREEN_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::exit))
            .child(self.player.clone())
    }
}
