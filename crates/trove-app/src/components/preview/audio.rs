//! The audio-asset preview: the waveform over a transport, no new media
//! stack.
//!
//! Everything below the shape already existed — the ffmpeg pipe that emits
//! 44.1 kHz stereo PCM, the rodio sink, the clock, the tempo-preserving speed
//! chain — but it was only reachable through [`AudioEngine`], which the video
//! player owns one of per soundtrack. An audio file is that same soundtrack
//! with nothing to draw beside it, so this panel exists to point the engine at
//! a file and hand the user [`transport`]'s controls.
//!
//! Deliberately absent: fullscreen (the video stage exists to reconcile a
//! picture with its controls; there is no picture here). The envelope strip is
//! the cached peaks of `trove_core::media::waveform` drawn by the same
//! rasterizer that paints the grid card — and it is a second
//! hit target for the same seek, because pointing at a moment in the waveform
//! is the gesture the shape of the music invites.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::{ElementExt as _, v_flex};
use gpui_kit::component::{ActiveTheme, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::AssetPreviewData;
use super::soundtrack::AudioEngine;
use super::transport::{self, Transport};

/// Height of the envelope strip, in pixels — twice the rasterized card
/// shape it grew out of, because the strip is the only thing on the stage
/// and can carry the extra presence.
const WAVE_H: f32 = 96.0;

/// The strip spans the stage's width up to this cap: past it the envelope
/// stretches into mush, and a bounded shape reads better centered than
/// edge-to-edge.
const WAVE_MAX_W: f32 = 960.0;

/// Height of the spectrum strip above the transport, in pixels.
const SPECTRUM_H: f32 = 56.0;

/// Thickness of the playhead drawn over the envelope.
const PLAYHEAD_W: f32 = 2.0;

/// How often the playhead re-reads the audio clock while nothing animates.
/// The clock is a `(position, instant)` pair, so a tick locks it and
/// repaints; it never touches the decoder.
const TICK: Duration = Duration::from_millis(250);

/// The bounce's frame interval while something is audible. The bounce is a
/// UI-side decay of the engine's per-chunk RMS readings, so it needs its own
/// pace, well under the 100 ms chunk it samples.
const ANIM_TICK: Duration = Duration::from_millis(33);

/// Fraction of the display level kept per animation tick. Between two
/// 100 ms engine readings the bounce decays to ~0.6 — a springy fall that
/// never quite reaches the floor before the next reading lands.
const LEVEL_DECAY: f32 = 0.85;

/// Spectrum ballistics, the classic analyzer's: a bar leaps to a new
/// reading at once and falls under a hybrid model — an exponential pull
/// (fast off the top, where the eye expects energy to vanish) with a floor
/// velocity (so a bar never appears to stall mid-fall).
const SPECTRUM_FALL_K: f32 = 0.10;
const SPECTRUM_FALL_MIN: f32 = 0.012;
/// Peak-hold caps ride above the bars and fall on their own slower clock,
/// marking where the band has been.
const SPECTRUM_PEAK_FALL: f32 = 0.02;

/// Automatic gain: a rolling ceiling over the frames' peaks, released
/// slowly (`AGC_RELEASE` per tick) and capped in boost. A quiet master gets
/// lifted toward the display's range — up to `AGC_MAX_GAIN` — while a loud
/// one plays untouched; the fall of the ceiling is what keeps a single
/// transient from flattening everything after it.
const AGC_RELEASE: f32 = 0.995;
const AGC_MAX_GAIN: f32 = 3.0;

/// Below this the display level counts as silent and the ticker drops back
/// to its idle pace.
const LEVEL_FLOOR: f32 = 0.004;

/// Width of the live bounce's window around the playhead, in envelope
/// buckets — one bucket is one pixel on the strip, so ±1σ is roughly ±14 px
/// of visible lift travelling with the music.
const BOUNCE_SIGMA: f32 = 14.0;

/// Open the audio preview for `data`, or `None` when there is nothing to play:
/// no file behind the asset, or no engine (no ffmpeg, or a stream the probe
/// cannot find). The caller then keeps the still, which for an audio asset is
/// its cover art or kind icon — the same picture, without the transport.
pub(super) fn spawn_player(data: &AssetPreviewData, cx: &mut App) -> Option<Entity<AudioPlayer>> {
    let path = data.original.clone()?;
    if !path.is_file() {
        return None;
    }
    if !trove_core::media::video::has_audio_track(&path) {
        return None;
    }
    let engine = AudioEngine::spawn(path, cx);
    let player = cx.new(|cx| AudioPlayer::new(data, engine, cx));
    player.update(cx, |player, cx| player.start_ticker(cx));
    Some(player)
}

/// The live audio preview: the engine, the playhead it reports, and the
/// controls that drive it.
pub(super) struct AudioPlayer {
    engine: Entity<AudioEngine>,
    transport: Transport,
    duration_ms: u64,
    wave: Wave,
    /// The display level, 0..1 — the engine's per-chunk RMS held up by a
    /// per-tick decay, so the bounce falls off between readings instead of
    /// stepping. Zero when nothing is audible; the ticker owns the decay.
    level: f32,
    /// The display spectrum — [`BAND_COUNT`] bars, 0..1, risen to the latest
    /// engine reading at once and falling on the hybrid ballistics. The
    /// decay lives here, on scalars; the paint below only draws this frame.
    spectrum: Vec<f32>,
    /// Peak-hold caps, one per band: they follow the bars up and fall on
    /// their slower clock, marking where the band has been.
    peak_hold: Vec<f32>,
    /// The AGC's rolling ceiling — the recent peak of published frames.
    agc_ceiling: f32,
    /// The strip's left edge and width in window coordinates, recorded at
    /// prepaint — the strip is sized by the stage, not a constant, so a
    /// pointer position maps to a moment only through the measured box.
    /// `Cell`s because the prepaint closure cannot also borrow `self`.
    band_left: Rc<Cell<Option<f32>>>,
    band_width: Rc<Cell<Option<f32>>>,
    /// Whether a drag across the strip is in progress. `transport.seeking`
    /// cannot answer this: the slider sets it too, and a move over the strip
    /// must not follow a drag that happens on the thumb.
    band_dragging: Rc<Cell<bool>>,
}

/// The waveform's load state.
enum Wave {
    Pending,
    Unavailable,
    /// The cached envelope, one level 0–255 per bucket. Drawn as bars
    /// straight onto the strip each frame — the animation repaints the shape
    /// every tick, so a rasterized copy would only be re-uploaded to the
    /// sprite atlas at the same pace.
    Ready(trove_core::media::waveform::Peaks),
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
            engine,
            transport,
            duration_ms,
            wave: Wave::Pending,
            level: 0.0,
            spectrum: vec![0.0; trove_core::media::spectrum::BAND_COUNT],
            peak_hold: vec![0.0; trove_core::media::spectrum::BAND_COUNT],
            agc_ceiling: 1.0,
            band_left: Rc::new(Cell::new(None)),
            band_width: Rc::new(Cell::new(None)),
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
    /// `pub(super)` for the preview's space bar, which answers for the
    /// soundtrack exactly as it does for the video and the animated image.
    pub(super) fn toggle_playing(&mut self, cx: &mut Context<Self>) {
        self.transport.playing = !self.transport.playing;
        self.push_playing(cx);
    }

    /// `,` / `.`: step the playhead `delta_ms`, committed at once — the
    /// soundtrack's counterpart of the video's frame step, which has no
    /// answer for a stream with no frames. `pub(super)` for the same
    /// reason as [`Self::toggle_playing`].
    pub(super) fn seek_by(&mut self, delta_ms: f64, cx: &mut Context<Self>) {
        if self.duration_ms == 0 {
            return;
        }
        let target = (self.transport.position_ms + delta_ms)
            .clamp(0., self.duration_ms as f64);
        self.scrub_to(target, true, cx);
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
        let width = self.band_width.get()?;
        Some(((x - left) / width).clamp(0., 1.) as f64)
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

    /// Follow the audio clock, and drive the bounce. Two paces in one loop:
    /// ~33 ms while something is audible or the display level is still
    /// relaxing to zero, 250 ms once nothing moves. Only the position and a
    /// repaint — the slider has to be set from `render`, which is where a
    /// `Window` lives.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            let mut sleep = TICK;
            loop {
                cx.background_executor().timer(sleep).await;
                // The entity is gone once the preview closes, which is the only
                // reason this can fail. The clock handle comes out of the same
                // update because a task's `AsyncApp` cannot `read` an entity.
                let Ok((playing, seeking, clock, engine_level, engine_spectrum)) =
                    weak.update(cx, |this, cx| {
                        (
                            this.transport.playing,
                            this.transport.seeking,
                            this.engine.read(cx).clock(),
                            this.engine.read(cx).level(),
                            this.engine.read(cx).spectrum(),
                        )
                    })
                else {
                    break;
                };
                // The engine reports where its stream started plus when; wall
                // time since then is the playhead. Same reading the video loop
                // makes.
                let advanced = (playing && !seeking)
                    .then(|| clock.lock().ok())
                    .flatten()
                    .and_then(|base| *base)
                    .map(|(ms, at)| ms + at.elapsed().as_secs_f64() * 1000.0);
                // The bounce: the engine's reading of what is audible now,
                // held up by the previous display value decaying toward it —
                // the RMS arrives once per 100 ms chunk, the movement is
                // continuous.
                let mut animate = false;
                let _ = weak.update(cx, |this, cx| {
                    let mut moving = false;
                    if let Some(ms) = advanced {
                        this.transport.position_ms = ms;
                        this.level = engine_level.max(this.level * LEVEL_DECAY).min(1.0);
                        // AGC: roll the ceiling over this frame's peak and
                        // lift the frame by whatever the cap allows. The
                        // ceiling only falls — a transient stretches the
                        // range, and quiet passages climb back slowly.
                        let frame_max =
                            engine_spectrum.iter().copied().fold(0.0f32, f32::max);
                        this.agc_ceiling = (frame_max * 0.9).max(this.agc_ceiling * AGC_RELEASE);
                        let gain = (1.0 / this.agc_ceiling).clamp(1.0, AGC_MAX_GAIN);
                        // Spectrum ballistics: a bar leaps to a new reading
                        // the moment it lands; below it, an exponential pull
                        // with a floor velocity. The peak-hold cap follows
                        // the same reading up and rains down slower.
                        for (i, (bar, &raw)) in
                            this.spectrum.iter_mut().zip(&engine_spectrum).enumerate()
                        {
                            let target = (raw * gain).min(1.0);
                            *bar = if target > *bar {
                                target
                            } else {
                                (*bar - (*bar * SPECTRUM_FALL_K).max(SPECTRUM_FALL_MIN))
                                    .max(target)
                            };
                            let peak = &mut this.peak_hold[i];
                            *peak = if target > *peak {
                                target
                            } else {
                                (*peak - SPECTRUM_PEAK_FALL).max(*bar)
                            };
                        }
                        moving = true;
                    } else {
                        if this.level > LEVEL_FLOOR {
                            // Paused, seeking, or the clock not yet
                            // republished after a restart: relax the bounce
                            // back to the envelope.
                            this.level *= LEVEL_DECAY;
                            moving = true;
                        } else if this.level != 0.0 {
                            this.level = 0.0;
                            moving = true;
                        }
                        // Nothing audible: bars and caps fall to the floor,
                        // and the AGC ceiling drifts back up to unity.
                        for (bar, peak) in
                            this.spectrum.iter_mut().zip(this.peak_hold.iter_mut())
                        {
                            if *bar > 0.0 {
                                *bar = (*bar - (*bar * SPECTRUM_FALL_K).max(SPECTRUM_FALL_MIN))
                                    .max(0.0);
                                moving = true;
                            }
                            if *peak > *bar {
                                *peak = (*peak - SPECTRUM_PEAK_FALL).max(*bar);
                                moving = true;
                            }
                        }
                        if this.agc_ceiling < 1.0 {
                            this.agc_ceiling = (this.agc_ceiling * AGC_RELEASE).max(1.0);
                        }
                    }
                    if moving {
                        cx.notify();
                    }
                    animate = moving;
                });
                sleep = if animate { ANIM_TICK } else { TICK };
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
        let stage_peaks = self.wave.peaks();
        let band_left = self.band_left.clone();
        let band_width = self.band_width.clone();
        let accent = cx.theme().accent;
        let bounce = self.level;
        // Where the playhead sits on the strip — a pixel offset for the
        // accent line, a bucket index for the bounce window. A running drag
        // already wrote itself into `position_ms`, and the ticker holds off
        // while seeking, so the one number serves both.
        let strip_width = self.band_width.get();
        let playhead_ratio = strip_width
            .filter(|_| self.duration_ms > 0)
            .map(|_| {
                (self.transport.position_ms / self.duration_ms as f64).clamp(0., 1.) as f32
            });
        let playhead_left = playhead_ratio
            .zip(strip_width)
            .map(|(ratio, width)| (ratio * width - PLAYHEAD_W / 2.).max(0.));
        let playhead_bucket = playhead_ratio
            .map(|ratio| ratio * trove_core::media::waveform::PEAK_COUNT as f32);
        let controls = transport::row(
            self.transport.position_ms,
            self.duration_ms,
            &self.transport.slider,
            transport::play_pause_button(
                self.transport.playing,
                Size::Small,
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
            // The artwork and its strip take the stage; the transport rides
            // the bottom edge as its own surface, drawn the way the video
            // chrome_bar draws it — full width, popover colour, no separator
            // — so both players read as one control. The bar never covers
            // moving content here, so it stays put rather than borrowing the
            // video's auto-hide.
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .px_6()
                    .when(self.wave.is_ready(), |stage| {
                        let Some(peaks) = stage_peaks else {
                            return stage;
                        };
                        stage.child(
                            div()
                                .relative()
                                // Sized by the stage, not a constant: the
                                // strip spans the width it is given, up to
                                // the cap past which the envelope reads as
                                // mush.
                                .w_full()
                                .max_w(px(WAVE_MAX_W))
                                .h(px(WAVE_H))
                                .cursor_pointer()
                                // The strip is a second timeline. Pointing at a
                                // moment in the picture of the music is the
                                // obvious gesture, so it gets the same
                                // drag-then-commit as the thumb below it — one
                                // `scrub_to`, so the two cannot disagree about
                                // where the playhead is.
                                .on_prepaint(move |bounds: Bounds<Pixels>, _, _| {
                                    band_left.set(Some(f32::from(bounds.origin.x)));
                                    band_width.set(Some(f32::from(bounds.size.width)));
                                })
                                // The id comes after `on_prepaint` (the same
                                // contract the preview stage follows), and it is
                                // what carries the move/up stream while the
                                // pointer wanders off the strip's edge mid-drag.
                                .id("wave-band")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                        let Some(ratio) =
                                            this.band_ratio(f32::from(event.position.x))
                                        else {
                                            return;
                                        };
                                        this.band_dragging.set(true);
                                        this.scrub_to(ratio * this.duration_ms as f64, false, cx);
                                    }),
                                )
                                .on_mouse_move(cx.listener(
                                    |this, event: &MouseMoveEvent, _, cx| {
                                        if !this.band_dragging.get() {
                                            return;
                                        }
                                        let Some(ratio) =
                                            this.band_ratio(f32::from(event.position.x))
                                        else {
                                            return;
                                        };
                                        this.scrub_to(ratio * this.duration_ms as f64, false, cx);
                                    },
                                ))
                                // Both the plain and the escaped release land
                                // here: a drag that runs off the end of the
                                // strip still means "go to the last position I
                                // saw".
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                        this.end_band_drag(cx)
                                    }),
                                )
                                .on_mouse_up_out(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                        this.end_band_drag(cx)
                                    }),
                                )
                                .child(
                                    gpui::canvas(
                                        |_, _, _| {},
                                        move |bounds, _, window, _| {
                                            paint_wave(
                                                bounds,
                                                &peaks,
                                                bounce,
                                                playhead_bucket,
                                                window,
                                            );
                                        },
                                    )
                                    .size_full(),
                                )
                                .when_some(playhead_left, |band, left| {
                                    band.child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .left(px(left))
                                            .w(px(PLAYHEAD_W))
                                            .h(px(WAVE_H))
                                            .bg(accent),
                                    )
                                }),
                        )
                    }),
            )
            // The live spectrum rides just above the transport, always on
            // the stage — the strip answers "where in the song am I", the
            // bars answer "what does the moment sound like". Same width
            // rules as the envelope, so the two line up; at rest it reads
            // as an idle analyzer rather than a hole.
            .child({
                let spectrum = self.spectrum.clone();
                let peaks = self.peak_hold.clone();
                div()
                    .w_full()
                    .px_6()
                    .pb_2()
                    .child(
                        div().w_full().max_w(px(WAVE_MAX_W)).h(px(SPECTRUM_H)).child(
                            gpui::canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| {
                                    paint_spectrum(bounds, &spectrum, &peaks, accent, window);
                                },
                            )
                            .size_full(),
                        ),
                    )
            })
            .child(div().px_3().py_2().bg(cx.theme().popover).child(controls))
            .into_any_element()
    }
}

impl Wave {
    fn is_ready(&self) -> bool {
        matches!(self, Wave::Ready(_))
    }

    /// A copy of the envelope for the strip's canvas — 400 bytes, cloned per
    /// render so the paint closure owns its data.
    fn peaks(&self) -> Option<trove_core::media::waveform::Peaks> {
        match self {
            Wave::Ready(peaks) => Some(peaks.clone()),
            Wave::Pending | Wave::Unavailable => None,
        }
    }
}

/// The envelope for the transport strip, loaded (and cached) off the UI
/// thread. No bitmap is built here: the strip paints bars straight to the
/// window every frame, because the bounce repaints the shape at animation
/// pace and a rasterized copy would only be re-uploaded to the sprite atlas
/// at the same pace.
fn build_wave(
    cache_root: &std::path::Path,
    sha: &str,
    path: &std::path::Path,
) -> Option<trove_core::media::waveform::Peaks> {
    trove_core::media::waveform::load_or_build(cache_root, sha, path)
}

/// The animated transport strip: one mirrored bar per envelope bucket,
/// painted straight to the window so the bounce costs no bitmap upload.
/// The shape is the song — the envelope, mirrored about the centre line
/// exactly as the bitmap rasterizer drew it — and the buckets near the
/// playhead lift with the live level, the sound happening now travelling
/// along its own picture.
fn paint_wave(
    bounds: Bounds<Pixels>,
    peaks: &[u8],
    bounce: f32,
    playhead_bucket: Option<f32>,
    window: &mut Window,
) {
    let ink = gpui::rgb(0x8c8c96);
    // Work in plain f32: the strip's geometry is fixed, and the paint API
    // takes `Pixels` back at the end anyway.
    let left: f32 = bounds.origin.x.into();
    let top: f32 = bounds.origin.y.into();
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let bar_w = width / peaks.len().max(1) as f32;
    let centre = top + height / 2.0;
    // A full-scale bar spans 44% of the strip's height, matching the
    // PREVIEW style the bitmap rasterizer used for the same strip.
    let span = height * 0.44;
    for (i, &peak) in peaks.iter().enumerate() {
        let mut full = f32::from(peak) / 255.0;
        if bounce > 0.0
            && let Some(head) = playhead_bucket
        {
            let d = i as f32 - head;
            let lift = (-(d * d) / (2.0 * BOUNCE_SIGMA * BOUNCE_SIGMA)).exp();
            full = (full + bounce * lift).min(1.0);
        }
        if full == 0.0 {
            // A bucket that held no sound, and none playing over it:
            // silence stays silence rather than a drawn baseline.
            continue;
        }
        // At least one pixel each side, so a quiet slice is a thin band
        // rather than an empty box.
        let half = (full * span / 2.0).max(1.0);
        let x = left + i as f32 * bar_w;
        window.paint_quad(gpui::fill(
            Bounds::from_corners(
                Point {
                    x: px(x),
                    y: px(centre - half),
                },
                Point {
                    x: px(x + bar_w),
                    y: px(centre + half),
                },
            ),
            ink,
        ));
    }
}

/// The live spectrum strip: one bar per log-spaced band, rising from the
/// strip's floor, brighter as it climbs, with a peak-hold cap riding above
/// each bar at full strength. The ballistics live in the ticker — bars
/// leap to a reading and rain down under the hybrid model — so this only
/// draws the current frame, the same split the waveform strip uses:
/// scalars on the CPU, pixels on the GPU.
fn paint_spectrum(
    bounds: Bounds<Pixels>,
    bands: &[f32],
    peaks: &[f32],
    accent: Hsla,
    window: &mut Window,
) {
    let left: f32 = bounds.origin.x.into();
    let top: f32 = bounds.origin.y.into();
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let slot = width / bands.len().max(1) as f32;
    let bar_w = (slot * 0.7).max(1.0);
    let bottom = top + height;
    for (i, &value) in bands.iter().enumerate() {
        let h = value * height;
        if h < 0.5 {
            // A band at the floor reads as silence, not as a baseline.
            continue;
        }
        let x = left + i as f32 * slot + (slot - bar_w) / 2.0;
        window.paint_quad(gpui::fill(
            Bounds::from_corners(
                Point {
                    x: px(x),
                    y: px(bottom - h),
                },
                Point {
                    x: px(x + bar_w),
                    y: px(bottom),
                },
            ),
            accent.opacity(0.35 + 0.65 * value),
        ));
        // The peak-hold cap: a two-pixel marker at full strength, hanging
        // where the band last peaked while the bar rains down beneath it.
        let peak = peaks.get(i).copied().unwrap_or(value).max(value);
        let top = peak * height;
        if top > 0.5 {
            window.paint_quad(gpui::fill(
                Bounds::from_corners(
                    Point {
                        x: px(x),
                        y: px(bottom - top - 1.0),
                    },
                    Point {
                        x: px(x + bar_w),
                        y: px(bottom - top + 1.0),
                    },
                ),
                accent,
            ));
        }
    }
}
