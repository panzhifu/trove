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

use gpui_kit::base::v_flex;
use gpui_kit::base::{ElementExt as _, h_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme as _, Size};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::chrome::{self, Chrome};
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
    /// A timeline drag is in progress: the clock holds off until release, or
    /// it would advance the picture out from under the thumb.
    seeking: bool,
    /// The timeline, one thumb over the run's frame indices. The state is
    /// user-draggable, so re-pushing an unchanged position each render would
    /// yank the thumb back mid-drag.
    slider: Entity<SliderState>,
    synced_position: f32,
    /// Held so the slider's subscription lives with the player.
    _subscription: Subscription,
    /// The floating transport's visibility, on the same auto-hide clock as
    /// the video and animated-image players: showing for the first stretch,
    /// revealed at the picture's bottom edge, hidden once the pointer rests.
    chrome: Chrome,
    /// The player's own box, so the bar reveals at the picture's bottom edge.
    chrome_bounds: Entity<Bounds<Pixels>>,
}

/// Where a drag on the timeline asks to land, as a frame index.
///
/// The slider's range is `0..=total-1`, but a drag is an `f32` arriving from a
/// pointer: a NaN cannot be clamped (`f32::clamp` answers NaN with NaN), and
/// both it and a negative land at the first frame; anything past the end is
/// the last one. Same contract as the animated player's millisecond version.
fn seek_target_frame(value: f32, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if value.is_nan() {
        return 0;
    }
    (value.round().clamp(0.0, (total - 1) as f32)) as usize
}

impl SequencePlayer {
    /// Start playing `frames` at `fps`. The first frame is on screen from the
    /// first render; the clock advances from there.
    pub(crate) fn spawn(frames: Vec<PathBuf>, fps: f64, cx: &mut App) -> Entity<Self> {
        let fps = fps.clamp(1.0, 240.0);
        let interval = Duration::from_secs_f64(1.0 / fps);
        let total = frames.len();
        let chrome_bounds = cx.new(|_| Bounds::default());
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(total.saturating_sub(1).max(1) as f32)
                .step(1.0)
                .default_value(0.)
        });
        // Annotated like the animated player's: the subscriber *is* the entity
        // being built here, so the model type is still unknown at this line
        // without it.
        let entity = cx.new(|cx: &mut Context<Self>| {
            let subscription = cx.subscribe(
                &slider,
                |this: &mut Self, _slider, event: &SliderEvent, cx| {
                    let dragged = match event {
                        SliderEvent::Change(value) | SliderEvent::Release(value) => value.start(),
                    };
                    let target = seek_target_frame(dragged, this.frames.len());
                    this.position = target;
                    match event {
                        // Hold the clock off for the length of the drag. The
                        // frame itself loads on demand — gpui decodes whichever
                        // file is on screen, the way the still preview always
                        // has — so a drag over undecoded frames shows each one
                        // as its decode lands rather than instantly.
                        SliderEvent::Change(_) => this.seeking = true,
                        // Release hands back to the clock, which continues from
                        // the dropped frame: the loop reads this entity's own
                        // cursor, so there is no seek to serve separately.
                        SliderEvent::Release(_) => this.seeking = false,
                    }
                    cx.notify();
                },
            );
            Self {
                frames,
                position: 0,
                fps,
                playing: true,
                seeking: false,
                slider,
                synced_position: -1.,
                _subscription: subscription,
                chrome: Chrome::new(),
                chrome_bounds,
            }
        });
        chrome::watch(entity.downgrade(), cx, Self::chrome_mut);
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
                        if this.playing && !this.seeking {
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

    /// The chrome accessor the shared auto-hide watcher drives.
    fn chrome_mut(&mut self) -> &mut Chrome {
        &mut self.chrome
    }

    /// Pointer moved over the player: reveal the bar at the picture's bottom
    /// edge.
    fn on_pointer_moved(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = *self.chrome_bounds.read(cx);
        if self.chrome.moved(bounds, event.position) {
            cx.notify();
        }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.frames.len();
        // Mirror the cursor into the thumb, but never during a drag: the clock
        // notifies on every frame, so the value would be reset under the
        // pointer and the drag would never take.
        if !self.seeking && self.position as f32 != self.synced_position {
            self.synced_position = self.position as f32;
            let position = self.position as f32;
            self.slider
                .update(cx, |slider, cx| slider.set_value(position, window, cx));
        }
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
            // One bar, one run: the thumb sits on the frame index, and a drag
            // shows that frame as soon as its decode lands.
            .child(div().flex_1().child(Slider::new(&self.slider).horizontal()))
            .child(muted_label(
                format!("{} / {}", self.position + 1, total),
                cx,
            ))
            .child(muted_label(format!("{:.0} fps", self.fps), cx));
        // The bar's show/hide plays as a fade and hides on the same idle
        // clock as the video and animated-image bars — one contract for
        // everything that plays frames.
        let fade = chrome::presence(self.chrome.shown(), "sequence-bar", window, cx);
        let bounds = self.chrome_bounds.clone();
        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .on_prepaint(move |measured: Bounds<Pixels>, _, cx| {
                bounds.update(cx, |slot, _| *slot = measured);
            })
            .on_mouse_move(cx.listener(Self::on_pointer_moved))
            .child(stage)
            .when(fade.should_render(), |root| {
                root.child(transport.opacity(fade.progress))
            })
    }
}

#[cfg(test)]
mod tests {
    // Imported by name rather than by glob: `gpui_kit::*` carries a `test`
    // attribute macro of its own, and taking it with the rest of the parent's
    // scope turns every `#[test]` in here into a recursion error.
    use super::seek_target_frame;

    /// What a drag on the timeline asks for. The slider is built over frame
    /// indices, so the worth-pinning cases are the ones a pointer can still
    /// produce: a non-finite value (`f32::clamp` answers NaN with NaN, and an
    /// `as usize` cast would wrap it into a huge index), a negative, and a
    /// drag past the end.
    #[test]
    fn a_timeline_drag_resolves_to_a_frame_inside_the_run() {
        assert_eq!(seek_target_frame(0.0, 12), 0);
        assert_eq!(seek_target_frame(5.4, 12), 5, "a drag rounds to a frame");
        assert_eq!(
            seek_target_frame(11.9, 12),
            11,
            "rounding clamps to the last index"
        );
        assert_eq!(
            seek_target_frame(-3.0, 12),
            0,
            "a negative drag is the first"
        );
        assert_eq!(seek_target_frame(f32::NAN, 12), 0, "a NaN must not move it");
        assert_eq!(
            seek_target_frame(f32::INFINITY, 12),
            11,
            "and a long one is the last frame"
        );
        assert_eq!(seek_target_frame(0.0, 0), 0, "an empty run stays empty");
    }
}
