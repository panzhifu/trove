//! The audio-asset preview: cover art over a transport, no new media stack.
//!
//! Everything below the picture already existed — the ffmpeg pipe that emits
//! 44.1 kHz stereo PCM, the rodio sink, the clock, the tempo-preserving speed
//! chain — but it was only reachable through [`AudioEngine`], which the video
//! player owns one of per soundtrack. An audio file is that same soundtrack
//! with nothing to draw beside it, so this panel exists to point the engine at
//! a file and hand the user [`transport`]'s controls.
//!
//! Deliberately absent: fullscreen (the video stage exists to reconcile a
//! picture with its controls; there is no picture here). The envelope strip is
//! the cached peaks of `trove_core::media::waveform` drawn by the same
//! rasterizer that paints a cover-less track's grid card — and it is a second
//! hit target for the same seek, because pointing at a moment in the waveform
//! is the gesture the picture of the music invites.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::{ElementExt as _, v_flex};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::soundtrack::AudioEngine;
use super::transport::{self, Transport};
use super::{AssetPreviewData, video};

/// Width and height of the envelope strip, in pixels. The peak count is also
/// 400, so one column per peak and no resampling artefacts at 1×.
const WAVE_W: u32 = 400;
const WAVE_H: u32 = 48;

/// Thickness of the playhead drawn over the envelope.
const PLAYHEAD_W: f32 = 2.0;

/// How often the playhead re-reads the audio clock. The clock is a
/// `(position, instant)` pair, so a tick locks it and repaints; it never
/// touches the decoder.
const TICK: Duration = Duration::from_millis(250);

/// Open the audio preview for `data`, or `None` when there is nothing to play:
/// no file behind the asset, or no engine (no ffmpeg, or a stream the probe
/// cannot find). The caller then keeps the still, which for an audio asset is
/// its cover art or kind icon — the same picture, without the transport.
pub(super) fn spawn_player(data: &AssetPreviewData, cx: &mut App) -> Option<Entity<AudioPlayer>> {
    let path = data.original.clone()?;
    if !path.is_file() {
        return None;
    }
    let engine = AudioEngine::spawn(path, cx)?;
    let player = cx.new(|cx| AudioPlayer::new(data, engine, cx));
    player.update(cx, |player, cx| player.start_ticker(cx));
    Some(player)
}

/// The live audio preview: the engine, the playhead it reports, and the
/// controls that drive it.
pub(super) struct AudioPlayer {
    data: AssetPreviewData,
    engine: Entity<AudioEngine>,
    transport: Transport,
    duration_ms: u64,
    /// The envelope strip: `Pending` until the load task lands, `None` when
    /// there is no ffmpeg, no cache address, or no decodable audio — in which
    /// case the row simply carries no waveform, exactly as it did before.
    wave: Wave,
    /// The strip's left edge in window coordinates, recorded at prepaint.
    /// Pointer events carry a window position, so a ratio needs something to
    /// subtract; a `Cell` because the prepaint closure cannot also borrow
    /// `self`.
    band_left: Rc<Cell<Option<f32>>>,
    /// Whether a drag across the strip is in progress. `transport.seeking`
    /// cannot answer this: the slider sets it too, and a move over the strip
    /// must not follow a drag that happens on the thumb.
    band_dragging: Rc<Cell<bool>>,
}

/// The waveform's load state.
enum Wave {
    Pending,
    Unavailable,
    Ready(Arc<RenderImage>),
}

impl AudioPlayer {
    fn new(data: &AssetPreviewData, engine: Entity<AudioEngine>, cx: &mut Context<Self>) -> Self {
        let duration_ms = data.duration_ms.unwrap_or(0);
        let transport = Transport::new(duration_ms, cx);

        // A drag previews the target; the seek itself happens on release, like
        // every other desktop player.
        let slider = transport.slider.clone();
        cx.subscribe(
            &slider,
            |this, _slider, event: &transport::SliderEvent, cx| {
                use transport::SliderEvent;
                match event {
                    SliderEvent::Change(value) => {
                        this.scrub_to(value.start().max(0.) as f64, false, cx)
                    }
                    SliderEvent::Release(value) => {
                        this.scrub_to(value.start().max(0.) as f64, true, cx)
                    }
                }
            },
        )
        .detach();

        let volume_slider = transport.volume_slider.clone();
        cx.subscribe(
            &volume_slider,
            |this, _slider, event: &transport::SliderEvent, cx| {
                let value = match event {
                    transport::SliderEvent::Change(value)
                    | transport::SliderEvent::Release(value) => value.start().clamp(0., 1.),
                };
                this.transport.volume = value;
                // Dragging the slider off zero is how you unmute — there is no
                // separate mute button to press.
                this.transport.muted = value == 0.;
                this.push_volume(cx);
                cx.notify();
            },
        )
        .detach();

        let this = Self {
            data: data.clone(),
            engine,
            transport,
            duration_ms,
            wave: Wave::Pending,
            band_left: Rc::new(Cell::new(None)),
            band_dragging: Rc::new(Cell::new(false)),
        };

        // Decode the envelope off the UI thread: it spawns ffmpeg and reads the
        // whole file at 300 Hz, which is fast but not free, and a first
        // playback must not stall the frame it is opening on.
        if let (Some(path), Some((cache_root, sha))) =
            (data.original.clone(), data.wave_cache.clone())
        {
            let entity = cx.entity();
            cx.spawn(async move |_, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move { build_wave(&cache_root, &sha, &path) })
                    .await;
                entity.update(cx, |this, cx| {
                    this.wave = match image {
                        Some(image) => Wave::Ready(image),
                        None => Wave::Unavailable,
                    };
                    cx.notify();
                });
            })
            .detach();
        }

        this
    }

    /// Nothing starts sound by itself: the grid plays a different file on every
    /// arrow-key move, and autoplay would turn browsing into a jukebox.
    fn toggle_playing(&mut self, cx: &mut Context<Self>) {
        self.transport.playing = !self.transport.playing;
        self.push_playing(cx);
    }

    fn toggle_volume(&mut self, cx: &mut Context<Self>) {
        self.transport.volume_open = !self.transport.volume_open;
        cx.notify();
    }

    /// Move the playhead. `commit` is the whole difference between a preview
    /// and a seek: the engine restarts its pipe only when the drag lands, so a
    /// scrub across the envelope does not spawn one ffmpeg per pixel. Both
    /// hit targets — the slider's thumb and the waveform strip — arrive here,
    /// which is the only reason the two cannot disagree about where the
    /// playhead is.
    fn scrub_to(&mut self, target_ms: f64, commit: bool, cx: &mut Context<Self>) {
        self.transport.seeking = !commit;
        self.transport.position_ms = target_ms;
        if commit {
            // Mirror the new position into the thumb's own guard, or
            // `sync_sliders` treats the value it just produced as an external
            // change and yanks a drag that has not finished.
            self.transport.synced_position = target_ms as f32;
            // The engine restarts its pipe from here; the clock follows on the
            // next tick.
            self.engine
                .update(cx, |engine, _| engine.restart_at(target_ms));
        }
        cx.notify();
    }

    /// Where in the envelope a window x-coordinate falls, as a 0..1 ratio.
    /// `None` before the strip has been measured, and for a clip with no known
    /// duration — a seek there could only land at zero, so the strip stays a
    /// picture rather than a bar you can click for nothing.
    fn band_ratio(&self, x: f32) -> Option<f64> {
        if self.duration_ms == 0 {
            return None;
        }
        let left = self.band_left.get()?;
        Some(((x - left) / WAVE_W as f32).clamp(0., 1.) as f64)
    }

    /// A band drag that lets go: the seek lands wherever the pointer last was.
    /// The flag is the guard — a release the strip never started (one that came
    /// from a thumb drag, or the second half of an escaped release) must not
    /// restart the engine.
    fn end_band_drag(&mut self, cx: &mut Context<Self>) {
        if !self.band_dragging.replace(false) {
            return;
        }
        let target = self.transport.position_ms;
        self.scrub_to(target, true, cx);
    }

    /// Scrubbing counts as paused, so the engine holds the sound while the
    /// thumb moves instead of fighting the drag.
    fn push_playing(&mut self, cx: &mut Context<Self>) {
        let playing = self.transport.playing && !self.transport.seeking;
        self.engine
            .update(cx, |engine, _| engine.set_playing(playing));
    }

    fn push_volume(&mut self, cx: &mut Context<Self>) {
        let (volume, muted) = (self.transport.volume, self.transport.muted);
        self.engine
            .update(cx, |engine, _| engine.set_volume(volume, muted));
    }

    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        if self.transport.speed == speed {
            return;
        }
        self.transport.speed = speed;
        // Restarting the pipe is how the tempo lands: `atempo` is set when the
        // ffmpeg child is spawned, so a running pipe keeps the old one.
        let position = self.transport.position_ms;
        self.engine.update(cx, |engine, _| {
            engine.set_speed(speed);
            engine.restart_at(position);
        });
        cx.notify();
    }

    /// Follow the audio clock. Only the position and a repaint — the slider has
    /// to be set from `render`, which is where a `Window` lives.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                // The entity is gone once the preview closes, which is the only
                // reason this can fail. The clock handle comes out of the same
                // update because a task's `AsyncApp` cannot `read` an entity.
                let Ok((playing, seeking, clock)) = weak.update(cx, |this, cx| {
                    (
                        this.transport.playing,
                        this.transport.seeking,
                        this.engine.read(cx).clock(),
                    )
                }) else {
                    break;
                };
                if !playing || seeking {
                    continue;
                }
                // The engine reports where its stream started plus when; wall
                // time since then is the playhead. Same reading the video loop
                // makes.
                let advanced = clock
                    .lock()
                    .ok()
                    .and_then(|base| *base)
                    .map(|(ms, at)| ms + at.elapsed().as_secs_f64() * 1000.0);
                let Some(ms) = advanced else { continue };
                let _ = weak.update(cx, |this, cx| {
                    this.transport.position_ms = ms;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for AudioPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = cx.entity();
        self.transport.sync_sliders(window, cx);
        // Pulled out first: `when`'s closure borrows the builder, not `self`.
        let stage_wave = self.wave.image();
        let band_left = self.band_left.clone();
        let accent = cx.theme().accent;
        // Where the playhead sits on the strip. A running drag already wrote
        // itself into `position_ms`, and the ticker holds off while seeking, so
        // the one number serves both.
        let playhead_left: Option<f32> =
            self.band_left
                .get()
                .filter(|_| self.duration_ms > 0)
                .map(|_| {
                    let ratio =
                        (self.transport.position_ms / self.duration_ms as f64).clamp(0., 1.) as f32;
                    (ratio * WAVE_W as f32 - PLAYHEAD_W / 2.).max(0.)
                });
        let controls = transport::row(
            self.transport.position_ms,
            self.duration_ms,
            &self.transport.slider,
            transport::play_pause_button(
                self.transport.playing,
                &host,
                AudioPlayer::toggle_playing,
            ),
            transport::speed_button(self.transport.speed, &host, AudioPlayer::set_speed),
            Some(transport::volume_button(
                self.transport.volume,
                self.transport.muted,
                self.transport.volume_open,
                &self.transport.volume_slider,
                &host,
                AudioPlayer::toggle_volume,
                cx,
            )),
            cx,
        );
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            // The artwork is the thumbnail the import pipeline wrote from the
            // file's own embedded cover; with none, this is the kind icon.
            .child(video::cover(&self.data, cx))
            .child(div().text_sm().text_center().child(self.data.name.clone()))
            .when(self.wave.is_ready(), |stage| {
                let image = match &stage_wave {
                    Some(image) => image.clone(),
                    None => return stage,
                };
                stage.child(
                    div()
                        .relative()
                        .w(px(WAVE_W as f32))
                        .h(px(WAVE_H as f32))
                        .cursor_pointer()
                        // The strip is a second timeline. Pointing at a moment
                        // in the picture of the music is the obvious gesture, so
                        // it gets the same drag-then-commit as the thumb below
                        // it — one `scrub_to`, so the two cannot disagree about
                        // where the playhead is.
                        .on_prepaint(move |bounds: Bounds<Pixels>, _, _| {
                            band_left.set(Some(f32::from(bounds.origin.x)));
                        })
                        // The id comes after `on_prepaint` (the same contract
                        // the preview stage follows), and it is what carries the
                        // move/up stream while the pointer wanders off the
                        // strip's edge mid-drag.
                        .id("wave-band")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                let Some(ratio) = this.band_ratio(f32::from(event.position.x))
                                else {
                                    return;
                                };
                                this.band_dragging.set(true);
                                this.scrub_to(ratio * this.duration_ms as f64, false, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                            if !this.band_dragging.get() {
                                return;
                            }
                            let Some(ratio) = this.band_ratio(f32::from(event.position.x)) else {
                                return;
                            };
                            this.scrub_to(ratio * this.duration_ms as f64, false, cx);
                        }))
                        // Both the plain and the escaped release land here: a
                        // drag that runs off the end of the strip still means
                        // "go to the last position I saw".
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_band_drag(cx)),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_band_drag(cx)),
                        )
                        .child(img(ImageSource::Render(image)).size_full())
                        .when_some(playhead_left, |band, left| {
                            band.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left(px(left))
                                    .w(px(PLAYHEAD_W))
                                    .h(px(WAVE_H as f32))
                                    .bg(accent),
                            )
                        }),
                )
            })
            .child(div().w(px(420.)).child(controls))
            .into_any_element()
    }
}

impl Wave {
    fn is_ready(&self) -> bool {
        matches!(self, Wave::Ready(_))
    }

    fn image(&self) -> Option<Arc<RenderImage>> {
        match self {
            Wave::Ready(image) => Some(image.clone()),
            Wave::Pending | Wave::Unavailable => None,
        }
    }
}

/// The envelope as a bitmap for the transport row: the same rasterizer the
/// grid card uses, in the style that sits over the panel.
fn build_wave(
    cache_root: &std::path::Path,
    sha: &str,
    path: &std::path::Path,
) -> Option<Arc<RenderImage>> {
    let peaks = trove_core::media::waveform::load_or_build(cache_root, sha, path)?;
    let canvas = trove_core::media::waveform::bitmap(
        &peaks,
        WAVE_W,
        WAVE_H,
        &trove_core::media::waveform::Style::PREVIEW,
    )?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(canvas)])))
}
