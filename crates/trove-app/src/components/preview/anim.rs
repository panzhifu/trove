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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::base::{ElementExt as _, h_flex};
use gpui_kit::component::{ActiveTheme as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use trove_core::media::anim::FrameTimes;

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
    playing: bool,
    speed: f32,
}

/// A playing animated image: prepared frames, a current index, and the loop
/// that moves it.
pub(crate) struct AnimatedPlayer {
    /// One single-frame `RenderImage` per animation frame, built at open.
    frames: Vec<Arc<RenderImage>>,
    /// The frame on screen. Mirrored from the loop so rendering reads a field
    /// rather than locking.
    frame: usize,
    shared: Arc<Mutex<Shared>>,
    /// Cleared on drop so the loop exits with the panel.
    alive: Arc<AtomicBool>,
    /// Visibility of the floating transport bar.
    chrome: Chrome,
    /// The player's own box, so the bar reveals at the picture's bottom edge.
    chrome_bounds: Entity<Bounds<Pixels>>,
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
        let shared = Arc::new(Mutex::new(Shared {
            playing: true,
            speed: 1.0,
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let chrome_bounds = cx.new(|_| Bounds::default());
        let entity = cx.new(|_| Self {
            frames,
            frame: 0,
            shared: Arc::clone(&shared),
            alive: Arc::clone(&alive),
            chrome: Chrome::new(),
            chrome_bounds,
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
                let (playing, speed) = match loop_shared.lock() {
                    Ok(shared) => (shared.playing, shared.speed),
                    Err(_) => break,
                };
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

    /// Whether the loop is advancing, and how fast.
    fn playback(&self) -> (bool, f32) {
        self.shared
            .lock()
            .map(|shared| (shared.playing, shared.speed))
            .unwrap_or((false, 1.0))
    }

    /// Toggle play/pause. The loop picks it up on its next look.
    pub(crate) fn toggle_playing(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.playing = !shared.playing;
        }
        cx.notify();
    }

    /// The playback rate; re-paces the frame delays.
    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.speed = speed;
        }
        cx.notify();
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.frame.min(self.frames.len().saturating_sub(1));
        let source = ImageSource::Render(Arc::clone(&self.frames[frame]));
        let (playing, speed) = self.playback();
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
    use gpui_kit::{DevicePixels, RenderImage, size};
    use std::sync::Arc;
    use trove_core::media::anim::FrameTimes;

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
