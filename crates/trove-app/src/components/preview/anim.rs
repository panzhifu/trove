//! Animated-image playback, driven by a clock this panel owns.
//!
//! gpui can animate a GIF by itself: hand it the file and its `img` element
//! advances a frame index. That path is why a GIF used to look frozen here —
//! gpui's frame advance happens *inside its layout closure*, so a frame only
//! moves when something else happens to repaint the window, and it refuses to
//! advance at all while the window is inactive (gpui-pre 0.3.5, `elements/img.rs`:
//! `frame_count > 1 && !cx.reduce_motion()` nested under `window.is_window_active()`).
//! Neither of those is visible from the code that hands it a path, and both
//! stop the picture for reasons the user cannot influence.
//!
//! So the frames are decoded once ([`trove_core::media::anim::decode`]) into one
//! prepared [`gpui_kit::RenderImage`] per frame, and a loop of our own picks
//! which one is on screen. gpui still paints the picture; it no longer decides
//! when.
//!
//! The loop follows the video player's shape for the same reasons: what the user
//! just did lives in an `Arc<Mutex<…>>` so the loop never needs an `App` borrow
//! to find out, and each frame is scheduled against the wall clock rather than
//! sleeping a fixed span after every tick — otherwise each millisecond spent
//! notifying is added to the next frame's delay, which reads as judder.
//!
//! The transport bar carries a timeline whose length is **one cycle** of the
//! animation ([`trove_core::media::anim::FrameTimes::duration_ms`], the sum of
//! its frame delays). It is not "how much is left", because an animation has no
//! left: the bar fills, the picture wraps to frame 0, and the bar starts again.
//! Dragging it is the point — every frame is already decoded, so the picture
//! answers the pointer immediately and the clock is held off until the release.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::base::{ElementExt as _, h_flex};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::media::anim::FrameTimes;

use crate::components::controls::muted_label;

use super::chrome::{self, Chrome};
use super::{AssetPreviewData, AssetPreviewPanel, transport};

/// Whether this preview's picture wants a player of our own.
///
/// The gate is the *mime*, through the source the preview already resolved, not
/// a sniff of the file: deciding here whether to wait for a decode is on the
/// path every preview takes, and a GIF's frame list costs a full decode to
/// learn about. A `true` that turns out to be a still costs one background task
/// that answers `None`.
pub(crate) fn wants_player(data: &AssetPreviewData) -> bool {
    data.kind == trove_core::model::AssetKind::Image && data.animated.is_some()
}

/// Decode the preview's picture off the UI thread and hand it its player.
///
/// Returns immediately; until the decode answers, the panel shows the still it
/// was already showing. A file that turns out not to be animated keeps that
/// still, which is what the ordinary path would have drawn anyway.
pub(crate) fn load_player(panel: Entity<AssetPreviewPanel>, cx: &mut App) {
    let path = panel.read(cx).data.original.clone();
    // Held weakly, exactly as the video load holds its panel: a preview
    // dismissed while this decode is running must not be kept alive by the
    // task, and must not end up with a player in a stage that no window shows —
    // whose frames would then never reach `release` to leave the sprite atlas.
    let panel = panel.downgrade();
    let Some(path) = path else {
        // No file behind the asset: nothing is coming, and the stage has to
        // stop waiting rather than hold its gestures for a player that will
        // never arrive.
        settle(&panel, cx);
        return;
    };
    cx.spawn(async move |cx| {
        // Everything costly is in here: compositing every frame of a few
        // hundred-frame GIF is a second of work, and a preview that freezes
        // while it decodes has traded a broken animation for a frozen window.
        let prepared = cx
            .background_executor()
            .spawn(async move { prepare(&path) })
            .await;
        let _ = panel.update(cx, |this, cx| {
            // Settled either way: a picture that is not an animation is not
            // going to become one, so the stage stops waiting on it here.
            this.anim_loading = false;
            if let Some((frames, timing)) = prepared {
                this.anim = Some(AnimatedPlayer::start(frames, timing, cx));
            }
            cx.notify();
        });
    })
    .detach();
}

/// Give the panel back its still: the player is not coming.
fn settle(panel: &WeakEntity<AssetPreviewPanel>, cx: &mut App) {
    let _ = panel.update(cx, |this, cx| {
        this.anim_loading = false;
        cx.notify();
    });
}

/// How long the loop sleeps when it is paused. Short enough that resuming
/// reads as instant, long enough to stay free.
const PAUSED_POLL: Duration = Duration::from_millis(16);

/// The cap on decoded frames held for one preview.
///
/// The same figure the APNG path has always used, so GIF, APNG and animated
/// WebP stop at the same wall: 256 MB of RGBA is about 350 frames at 450×512,
/// far more than any real animation and more than a preview should hold
/// anyway. Past it the decode stops, so the part of an absurd file that fits
/// still plays instead of nothing playing.
const FRAME_BUDGET: u64 = 256 * 1024 * 1024 / 4;

/// Decode and prepare every frame, BGRA-swapped. `None` when the file turns
/// out not to be an animation.
fn prepare(path: &std::path::Path) -> Option<(Vec<Arc<RenderImage>>, FrameTimes)> {
    let decoded = trove_core::media::anim::decode(path, FRAME_BUDGET)?;
    let timing = decoded.timing;
    let frames: Vec<Arc<RenderImage>> = decoded
        .frames
        .into_iter()
        .map(|mut frame| {
            // gpui's renderer wants BGRA and `image` hands back RGBA, so the
            // swap happens once per frame here rather than on every repaint.
            for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            // One frame per `RenderImage`, because handing gpui the whole
            // animation and hoping it advances is precisely what this replaces.
            Arc::new(RenderImage::new(vec![frame]))
        })
        .collect();
    if frames.len() < 2 {
        return None;
    }
    Some((frames, timing))
}

/// What the loop reads each tick, kept off the entity so an `App` borrow can
/// never stall the clock.
struct Shared {
    /// The *effective* gate: the loop advances only when the user wants
    /// playback and no timeline drag is in progress. The entity writes it from
    /// [`AnimatedPlayer::publish_controls`], so a drag can never fight the clock.
    playing: bool,
    speed: f32,
    /// A seek the loop has not served yet, in milliseconds of one cycle.
    seek_to: Option<u64>,
}

/// A playing animated image: prepared frames, a current index, and the loop
/// that moves it.
pub(crate) struct AnimatedPlayer {
    /// One single-frame `RenderImage` per animation frame, built at open.
    frames: Vec<Arc<RenderImage>>,
    /// The frame on screen. Mirrored from the loop so rendering reads a field
    /// rather than locking.
    frame: usize,
    /// The frame table, kept a second time for the UI side: a drag must resolve
    /// "which frame is this millisecond" without the loop's help, and the loop
    /// owns the copy it paces itself with.
    timing: FrameTimes,
    shared: Arc<Mutex<Shared>>,
    /// Cleared on drop so the loop exits with the panel.
    alive: Arc<AtomicBool>,
    /// Visibility of the floating transport bar.
    chrome: Chrome,
    /// The player's own box, so the bar reveals at the picture's bottom edge.
    chrome_bounds: Entity<Bounds<Pixels>>,
    /// Whether the user wants it moving. Kept beside [`Shared::playing`]
    /// because the button must show this, not the effective gate: a drag
    /// suspends the clock without pausing the picture's intent.
    playing: bool,
    /// Playback rate, mirrored into [`Shared`] on every change.
    speed: f32,
    /// A timeline drag is in progress: the thumb follows the pointer and the
    /// loop's playhead is ignored until release.
    seeking: bool,
    /// Playhead in milliseconds of one cycle.
    position_ms: f64,
    /// The timeline itself, and the last value mirrored into its thumb. The
    /// state is user-draggable, so re-pushing an unchanged position each frame
    /// would yank the thumb back mid-drag.
    slider: Entity<SliderState>,
    synced_position: f32,
    /// Held so the slider's subscription lives with the player.
    _subscription: Subscription,
}

/// Where a drag on the timeline asks to land, in milliseconds of one cycle.
///
/// The slider's own range is `0..=duration_ms`, but a drag is a `f32` arriving
/// from a pointer: it must not become an absurd `as u64` cast. A NaN cannot be
/// clamped — `f32::clamp` answers NaN with NaN — so it lands at the start; a
/// negative is the start too, and anything past the end (an infinite drag
/// among them) is the end of the cycle.
fn seek_target_ms(value: f32, duration_ms: u64) -> u64 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(0., duration_ms as f32) as f64).round() as u64
}

/// How long frame `index` is shown, or nothing for an index off the end.
fn delay(timing: &FrameTimes, index: usize) -> Duration {
    Duration::from_millis(u64::from(timing.delays().get(index).copied().unwrap_or(0)))
}

/// [`delay`] at the requested speed, floored so a file that declares a
/// zero-length frame cannot spin the loop.
fn frame_delay(timing: &FrameTimes, index: usize, speed: f32) -> Duration {
    let base = delay(timing, index).as_secs_f64();
    Duration::from_secs_f64((base / f64::from(speed.max(0.05))).max(0.01))
}

impl AnimatedPlayer {
    /// Take already-decoded frames and start playing them.
    fn start(frames: Vec<Arc<RenderImage>>, timing: FrameTimes, cx: &mut App) -> Entity<Self> {
        let frame_count = timing.frames();
        let duration_ms = timing.duration_ms();
        let shared = Arc::new(Mutex::new(Shared {
            playing: true,
            speed: 1.0,
            seek_to: None,
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let chrome_bounds = cx.new(|_| Bounds::default());
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                // A file whose frames all declare zero delay is a cycle of no
                // length; a slider with an empty range has nothing to drag.
                .max(duration_ms.max(1) as f32)
                .default_value(0.)
        });
        // Annotated rather than inferred: the subscriber *is* the entity being
        // built here, so without this the model type is still unknown at the
        // `subscribe` line, which sits above the value that decides it.
        let entity = cx.new(|cx: &mut Context<Self>| {
            // The timeline. Held on the entity: a dropped subscription would
            // leave the bar draggable and mute.
            let subscription =
                cx.subscribe(&slider, move |this, _slider, event: &SliderEvent, cx| {
                    let dragged = match event {
                        SliderEvent::Change(value) | SliderEvent::Release(value) => value.start(),
                    };
                    let target = seek_target_ms(dragged, duration_ms);
                    match event {
                        // Every frame is already decoded, so a drag shows its target
                        // at once instead of waiting for the clock — and the clock is
                        // held off for the length of the drag, or the two would fight
                        // over the picture and the thumb.
                        SliderEvent::Change(_) => {
                            this.seeking = true;
                            this.frame = this.timing.frame_at(target);
                            // Must be pushed here, not just on release: the loop
                            // reads its gate, and without this it keeps advancing
                            // the picture against the pointer.
                            this.publish_controls();
                        }
                        SliderEvent::Release(_) => {
                            this.seeking = false;
                            if let Ok(mut shared) = this.shared.lock() {
                                shared.seek_to = Some(target);
                            }
                            this.publish_controls();
                        }
                    }
                    this.position_ms = target as f64;
                    cx.notify();
                });
            Self {
                frames,
                frame: 0,
                timing: timing.clone(),
                shared: Arc::clone(&shared),
                alive: Arc::clone(&alive),
                chrome: Chrome::new(),
                chrome_bounds,
                playing: true,
                speed: 1.0,
                seeking: false,
                position_ms: 0.,
                slider,
                synced_position: -1.,
                _subscription: subscription,
            }
        });
        chrome::watch(entity.downgrade(), cx, Self::chrome_mut);

        let weak = entity.downgrade();
        let loop_shared = Arc::clone(&shared);
        let loop_alive = Arc::clone(&alive);
        // The table moves into the loop rather than being read off the entity:
        // the clock must never need the app to know how long a frame lasts.
        cx.spawn(async move |cx| {
            // The loop's own cursor, so a paused stretch does not have to be
            // reconstructed from wall-clock arithmetic.
            let mut index = 0usize;
            // Frame 0 is on screen from the first tick, so the first wait is
            // its own delay; starting the clock at `now` would show it for no
            // time at all.
            let mut due = Instant::now() + frame_delay(&timing, index, 1.0);
            // The showing frame's remaining time at the pause, so resuming
            // finishes it instead of starting it over.
            let mut remaining: Option<Duration> = None;
            loop {
                if !loop_alive.load(Ordering::Relaxed) {
                    break;
                }
                let (playing, speed, seek) = match loop_shared.lock() {
                    Ok(mut shared) => (shared.playing, shared.speed, shared.seek_to.take()),
                    Err(_) => break,
                };
                if let Some(ms) = seek {
                    // Served whether or not the picture is playing: someone who
                    // dragged while paused and let go must resume from the frame
                    // they chose, not from where the clock left off. The playhead
                    // is *not* rewritten here: the drag (or the frame step) put
                    // the millisecond it asked for on the entity, and snapping it
                    // to the frame's own start would drag the label backwards
                    // under a pointer that released mid-frame. The next natural
                    // advance names the frame start again.
                    index = timing.frame_at(ms);
                    remaining = None;
                    due = Instant::now() + frame_delay(&timing, index, speed);
                    if weak
                        .update(cx, |this, cx| {
                            this.frame = index;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if !playing {
                    remaining.get_or_insert_with(|| due.saturating_duration_since(Instant::now()));
                    cx.background_executor().timer(PAUSED_POLL).await;
                    continue;
                }
                if let Some(left) = remaining.take() {
                    due = Instant::now() + left;
                }
                let now = Instant::now();
                if due > now {
                    cx.background_executor().timer(due - now).await;
                } else {
                    // Behind — a hung UI thread, a long drag. Re-anchor
                    // instead of replaying the backlog frame by frame, which
                    // would play the missed seconds in fast forward.
                    due = now;
                }
                // The wait above can have been long enough for the user to
                // pause during it.
                {
                    let Ok(shared) = loop_shared.lock() else {
                        break;
                    };
                    if !shared.playing {
                        continue;
                    }
                }
                index = (index + 1) % frame_count;
                due = due
                    .checked_add(frame_delay(&timing, index, speed))
                    .unwrap_or_else(Instant::now);
                if weak
                    .update(cx, |this, cx| {
                        this.frame = index;
                        // The playhead names the frame actually on screen, and
                        // wraps with it: one bar is one cycle of the animation.
                        // A drag owns the value instead, until it is released.
                        if !this.seeking {
                            this.position_ms = timing.ms_at(index) as f64;
                        }
                        cx.notify();
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

    /// Toggle play/pause. The loop picks it up on its next look.
    pub(crate) fn toggle_playing(&mut self, cx: &mut Context<Self>) {
        self.playing = !self.playing;
        self.publish_controls();
        cx.notify();
    }

    /// The playback rate; re-paces the frame delays.
    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.speed = speed;
        self.publish_controls();
        cx.notify();
    }

    /// `,` / `.`: one frame back or forward, and hold it there — the same
    /// contract as the video player's.
    ///
    /// Written as a seek rather than a nudge of the loop's cursor: the entity
    /// mirrors the frame on screen, so the neighbour's start time is known
    /// without consulting the clock, and the loop already knows how to land on a
    /// millisecond. Pausing first is what makes a step readable — the frame
    /// arrives and stays. Both directions wrap, because a six-frame GIF is as
    /// steerable as a six-hundred-frame one.
    ///
    /// The new frame is shown here rather than left to the loop: every frame is
    /// already decoded, and waiting up to `PAUSED_POLL` per press would make the
    /// key impossible to mash.
    pub(crate) fn step_frame(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.timing.frames();
        if count == 0 {
            return;
        }
        let current = self.frame.min(count - 1);
        let target = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.frame = target;
        self.playing = false;
        // The playhead is ours to move now — the loop serves the seek without
        // touching it (a drag's millisecond must not be snapped either), so a
        // stepped-to frame names itself from here, not from the last tick.
        let at = self.timing.ms_at(target);
        self.position_ms = at as f64;
        if let Ok(mut shared) = self.shared.lock() {
            shared.seek_to = Some(at);
        }
        self.publish_controls();
        cx.notify();
    }

    /// Push the UI's intent into the loop's gate. A timeline drag counts as
    /// paused, so the clock cannot move the picture out from under the thumb.
    fn publish_controls(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.playing = self.playing && !self.seeking;
            shared.speed = self.speed;
        }
    }

    /// The chrome accessor the shared auto-hide watcher drives.
    fn chrome_mut(&mut self) -> &mut Chrome {
        &mut self.chrome
    }

    /// Pointer moved over the player: reveal the bar at the bottom edge.
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

    /// Hand every prepared frame back to the window before the entity is
    /// dropped. gpui's sprite atlas has no capacity and evicts nothing on its
    /// own — a frame leaves only when `drop_image` says so — and this player
    /// holds one atlas entry per animation frame, so dropping it without this
    /// would keep a whole GIF's pixels resident for the rest of the session.
    /// Same contract as the video player's and the 3D viewport's `release`.
    /// The `Vec` is left in place rather than drained: the renderer indexes it,
    /// and the entity has no reason to survive this call for long.
    pub(super) fn release(&mut self, window: &mut Window) {
        for frame in &self.frames {
            let _ = window.drop_image(Arc::clone(frame));
        }
    }
}

impl Drop for AnimatedPlayer {
    fn drop(&mut self) {
        // The loop holds only a downgrade of this entity, so it cannot keep it
        // alive; this flag is how it learns to stop instead.
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl Render for AnimatedPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.frame.min(self.frames.len().saturating_sub(1));
        let source = ImageSource::Render(Arc::clone(&self.frames[frame]));
        let (playing, speed) = (self.playing, self.speed);
        let duration_ms = self.timing.duration_ms() as f64;
        let position_ms = self.position_ms;
        // Mirror the playhead into the thumb, but never during a drag: the loop
        // notifies on every frame, so the value would be reset under the pointer
        // and the drag would never take.
        if !self.seeking && (position_ms as f32 - self.synced_position).abs() >= 0.5 {
            self.synced_position = position_ms as f32;
            self.slider.update(cx, |slider, cx| {
                slider.set_value(position_ms as f32, window, cx)
            });
        }
        let bar = h_flex()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .gap_2()
            .px_3()
            .py_2()
            .bg(cx.theme().popover)
            .child(transport::play_pause_button(
                playing,
                Size::Small,
                &cx.entity(),
                Self::toggle_playing,
            ))
            // One bar, one cycle: it fills over the animation's own length and
            // starts again where the picture starts again.
            .child(div().flex_1().child(Slider::new(&self.slider).horizontal()))
            .child(muted_label(
                format!(
                    "{} / {}",
                    transport::time(position_ms),
                    transport::time(duration_ms)
                ),
                cx,
            ))
            .child(transport::speed_button(
                speed,
                &cx.entity(),
                Self::set_speed,
            ));
        let shown = self.chrome.shown();
        let bounds = self.chrome_bounds.clone();
        div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_prepaint(move |measured: Bounds<Pixels>, _, cx| {
                bounds.update(cx, |slot, _| *slot = measured);
            })
            .on_mouse_move(cx.listener(Self::on_pointer_moved))
            .child(
                img(source)
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    // The picture itself still answers a click.
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_playing(cx);
                    })),
            )
            .when(shown, |root| root.child(bar))
    }
}

#[cfg(test)]
mod tests {
    // Imported by name rather than by glob: `gpui_kit::*` carries a `test`
    // attribute macro of its own, and taking it with the rest of the parent's
    // scope turns every `#[test]` in here into a recursion error.
    use super::prepare;
    use super::seek_target_ms;
    use gpui_kit::{DevicePixels, RenderImage, size};
    use std::sync::Arc;
    use trove_core::media::anim::FrameTimes;

    /// What a drag on the timeline asks for.
    ///
    /// The slider is built with `0..=duration_ms`, so the range itself is not
    /// what is worth pinning: the two cases a pointer can still produce are a
    /// value past the end and a non-finite one. The second is the one that fails
    /// quietly — `f32 as u64` turns a NaN into 0, and `f32::clamp` answers NaN
    /// with NaN, so a NaN is refused by hand while everything else clamps.
    #[test]
    fn a_timeline_drag_resolves_to_a_millisecond_inside_the_cycle() {
        assert_eq!(seek_target_ms(0., 320), 0);
        assert_eq!(seek_target_ms(150.4, 320), 150);
        assert_eq!(seek_target_ms(-8., 320), 0, "a negative drag is the start");
        assert_eq!(
            seek_target_ms(9_000., 320),
            320,
            "and a long one is the end of the cycle"
        );
        assert_eq!(seek_target_ms(f32::NAN, 320), 0, "a NaN must not move it");
        assert_eq!(
            seek_target_ms(f32::INFINITY, 320),
            320,
            "an infinite drag is the end, not the start"
        );
    }

    /// Write a real animated GIF: one 8×8 frame per delay, each a distinct red.
    ///
    /// Encoded with `gif` because `image`'s GIF encoder exposes no per-frame
    /// API, so the crate that reads an animated GIF cannot produce one. This
    /// repeats the fixture that also exists in `trove_core::media::anim`'s
    /// tests rather than sharing it: the two ask different questions, and a test
    /// that borrows its input from the other crate's helper fails (or passes)
    /// together with it.
    fn write_gif(path: &std::path::Path, delays: &[u32]) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = gif::Encoder::new(std::io::BufWriter::new(file), 8, 8, &[]).unwrap();
        for (ix, ms) in delays.iter().enumerate() {
            let mut image = image::RgbaImage::from_pixel(
                8,
                8,
                image::Rgba([u8::try_from(ix + 1).unwrap(), 40, 70, 255]),
            );
            // The format counts delays in hundredths of a second.
            let mut frame = gif::Frame::from_rgba_speed(8, 8, image.as_mut(), 1);
            frame.delay = (*ms / 10) as u16;
            encoder.write_frame(&frame).unwrap();
        }
    }

    fn prepared(delays: &[u32]) -> Option<(Vec<Arc<RenderImage>>, FrameTimes)> {
        let dir = std::env::temp_dir().join(format!("trove-player-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.gif");
        write_gif(&path, delays);
        let out = prepare(&path);
        std::fs::remove_dir_all(&dir).ok();
        out
    }

    /// What the player hands the renderer, frame by frame.
    ///
    /// The pixel order is the point: gpui's renderer wants BGRA and `image`
    /// decodes RGBA, so a forgotten swap draws every animation in the wrong
    /// colours and nothing else in the code would say so. The frame count is the
    /// other half — the loop indexes `frames` by a value it derived from
    /// `timing`, so the two must be the same length or a preview can panic.
    #[test]
    fn each_prepared_frame_is_one_bgra_image() {
        let (frames, timing) = prepared(&[80, 120, 40]).expect("a real animated GIF must prepare");
        assert_eq!(frames.len(), 3);
        assert_eq!(timing.frames(), frames.len(), "timing and pixels disagree");
        for (ix, frame) in frames.iter().enumerate() {
            // One frame each: a multi-frame `RenderImage` would put gpui back in
            // charge of advancing it, which is the behaviour this replaces.
            assert_eq!(frame.frame_count(), 1, "frame {ix} holds several images");
            assert_eq!(frame.size(0), size(DevicePixels(8), DevicePixels(8)));
            let bytes = frame.as_bytes(0).unwrap();
            assert_eq!(&bytes[0..4], &[70, 40, u8::try_from(ix + 1).unwrap(), 255]);
        }
    }

    /// A one-frame file is a still, and a still must go on playing the ordinary
    /// thumbnail rather than a player with nothing to advance.
    #[test]
    fn a_single_frame_picture_prepares_nothing() {
        assert!(prepared(&[100]).is_none());
    }
}
