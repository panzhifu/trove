//! Sequence playback: walking a frame run at the run's own frame rate.
//!
//! Where [`super::anim`] decodes one file's frames once and advances a cursor
//! through them, a sequence is *separate files* — so the player walks their
//! paths and lets gpui decode whichever one is on screen, the same way the
//! still preview always has. The clock is ours, on the same terms: wall-clock
//! anchored every tick, so a stalled UI thread plays the missing seconds at
//! normal speed instead of in fast forward, and a pause parks the cursor
//! rather than reconstructing where it was.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::base::h_flex;
use gpui_kit::base::v_flex;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Size};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;

use crate::components::controls::muted_label;
use crate::components::preview::transport;

/// How tightly the clock anchors: shorter than one frame at any fps the
/// store's `CHECK` constraint allows, so the anchor never rounds a frame to
/// zero. Re-anchoring every tick costs one comparison.
const TICK: Duration = Duration::from_millis(10);

pub(crate) struct SequencePlayer {
    frames: Vec<PathBuf>,
    position: usize,
    fps: f64,
    playing: bool,
}

impl SequencePlayer {
    /// Start playing `frames` at `fps`. The first frame is on screen from the
    /// first render; the clock advances from there.
    pub(crate) fn spawn(frames: Vec<PathBuf>, fps: f64, cx: &mut App) -> Entity<Self> {
        let fps = fps.clamp(1.0, 240.0);
        let interval = Duration::from_secs_f64(1.0 / fps);
        let entity = cx.new(|_| Self {
            frames,
            position: 0,
            fps,
            playing: true,
        });
        let weak = entity.downgrade();
        cx.spawn(async move |cx| {
            let mut due = Instant::now() + interval;
            loop {
                let now = Instant::now();
                if due > now {
                    cx.background_executor().timer(due - now).await;
                } else {
                    // Behind — a stalled UI thread. Re-anchor instead of
                    // replaying the missed frames in fast forward.
                    due = now;
                }
                due = (due + interval).max(now + TICK);
                if weak
                    .update(cx, |this, cx| {
                        if this.playing {
                            this.position = (this.position + 1) % this.frames.len().max(1);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        entity
    }

    fn toggle_playing(&mut self, cx: &mut Context<Self>) {
        self.playing = !self.playing;
        cx.notify();
    }

    /// Step one frame, pausing: a user who walks wants to stay where they
    /// stop, exactly as the video and animated players behave.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.frames.len() as isize;
        if total == 0 {
            return;
        }
        self.playing = false;
        self.position = (self.position as isize + delta).rem_euclid(total) as usize;
        cx.notify();
    }
}

impl Render for SequencePlayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.frames.len();
        let stage = div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .children(self.frames.get(self.position).cloned().map(|path| {
                img(path)
                    .max_h_full()
                    .max_w_full()
                    .object_fit(ObjectFit::Contain)
            }));
        let transport = h_flex()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .gap_2()
            .px_3()
            .py_2()
            .bg(cx.theme().popover)
            .child(transport::play_pause_button(
                self.playing,
                Size::Small,
                &cx.entity(),
                Self::toggle_playing,
            ))
            .child(
                Button::new("sequence-prev")
                    .ghost()
                    .with_size(Size::Small)
                    .icon(IconName::ChevronLeft)
                    .on_click({
                        let entity = cx.entity();
                        move |_, _, cx| entity.update(cx, |this, cx| this.step(-1, cx))
                    }),
            )
            .child(
                Button::new("sequence-next")
                    .ghost()
                    .with_size(Size::Small)
                    .icon(IconName::ChevronRight)
                    .on_click({
                        let entity = cx.entity();
                        move |_, _, cx| entity.update(cx, |this, cx| this.step(1, cx))
                    }),
            )
            .child(div().flex_1())
            .child(muted_label(
                format!("{} / {}", self.position + 1, total),
                cx,
            ))
            .child(muted_label(format!("{:.0} fps", self.fps), cx));
        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(stage)
            .child(transport)
    }
}
