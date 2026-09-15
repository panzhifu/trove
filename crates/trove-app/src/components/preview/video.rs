//! Video preview: a live player (with sound, speed and fullscreen) on the
//! main area, the cover thumbnail in the inspector.
//!
//! The player is an entity — video frames come off an ffmpeg pipe (see
//! `trove_core::media::video`) one at a time on a background task, so the
//! main-area host spawns it exactly once at open (never per render)
//! through [`spawn_player`]. When ffmpeg is missing or the file cannot be
//! probed it returns `None`, and the host falls back to the still, the
//! same picture [`cover`] shows in the inspector.
//!
//! Audio rides a second ffmpeg pipe ([`AudioPipe`], signed 16-bit stereo at
//! 44.1 kHz) into rodio. It exists only when the file carries an audio
//! stream and the audio device opens; speed changes ride ffmpeg's `atempo`
//! filter so the pitch holds. Audio and video are two independent pipes
//! aligned at the start point — a preview does not need sample-accurate
//! lip sync, so there is no clock correction between them.
//!
//! Speed re-paces the frame loop (`frame_ms / speed`) and rebuilds the
//! audio pipe; volume and mute apply on the rodio sink directly. Fullscreen
//! is a separate OS window hosting a fresh player that continues from the
//! main-area player's state ([`VideoPlayerEvent::EnterFullscreen`]); leaving
//! it goes through the [`ExitVideoFullscreen`] action, and the fullscreen
//! host hands the position back to this player.
//!
//! The player keeps a single decoded frame alive and hands the previous
//! one back to the window before painting the next, because gpui's sprite
//! atlas never evicts entries by itself.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as MediaIcon;
use gpui_kit::base::POPUP_PRIORITY;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trove_core::media::video::{self, AudioPipe, FramePipe, VideoStreamFacts};
use trove_core::model::AssetKind;

use super::{AssetPreviewData, fallback};
use crate::app::actions::ExitVideoFullscreen;

/// How long the decode loops sleep while paused before looking again.
const IDLE_POLL: Duration = Duration::from_millis(120);

/// Fullscreen chrome: how long the pointer must rest before the floating
/// transport row hides itself.
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);

/// How often the fullscreen auto-hide watcher checks that countdown.
const CONTROLS_WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// Bottom band of the fullscreen window that counts as "on the controls":
/// the pointer inside it keeps the floating row visible.
const CONTROLS_BAND: f32 = 96.;

/// The playback speeds offered in the menu. All inside the 0.5–2.0
/// single-instance range of ffmpeg's `atempo`, so no filter chain.
const SPEEDS: [f32; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// The process-wide audio output. `OutputStream` has to stay alive for as
/// long as any player might make sound, so it lives in a global; the sink
/// per player is created from its handle.
struct AudioHost {
    _stream: rodio::OutputStream,
    handle: rodio::OutputStreamHandle,
}

impl Global for AudioHost {}

/// The audio output handle, opening the device on first use. `None` when
/// the device cannot be opened — the player then keeps playing silently
/// (same as the pre-audio behavior) instead of erroring.
fn audio_handle(cx: &mut App) -> Option<rodio::OutputStreamHandle> {
    if let Some(host) = cx.try_global::<AudioHost>() {
        return Some(host.handle.clone());
    }
    let (stream, handle) = rodio::OutputStream::try_default().ok()?;
    cx.set_global(AudioHost {
        _stream: stream,
        handle: handle.clone(),
    });
    Some(handle)
}

/// What the player tells its host panel: the user wants fullscreen, so the
/// host pauses this player and opens the fullscreen window from this
/// state. Leaving fullscreen does not come back as an event — the
/// fullscreen host owns that window and exits via the
/// [`ExitVideoFullscreen`] action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum VideoPlayerEvent {
    EnterFullscreen {
        position_ms: f64,
        speed: f32,
        volume: f32,
        muted: bool,
    },
}

/// Playback state carried into a fresh player (the fullscreen window
/// continues where the main-area player was).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlayerResume {
    pub position_ms: f64,
    pub speed: f32,
    pub volume: f32,
    pub muted: bool,
    pub playing: bool,
}

impl Default for PlayerResume {
    fn default() -> Self {
        Self {
            position_ms: 0.0,
            speed: 1.0,
            volume: 1.0,
            muted: false,
            playing: true,
        }
    }
}

/// Spawn the live player for `data`, or `None` when the kind is not video,
/// ffmpeg is unavailable, or the file cannot be probed.
pub(super) fn spawn_player(data: &AssetPreviewData, cx: &mut App) -> Option<Entity<VideoPlayer>> {
    if data.kind != AssetKind::Video {
        return None;
    }
    if !trove_core::media::video::ffmpeg_available() {
        return None;
    }
    VideoPlayer::spawn(data.original.as_ref()?.clone(), PlayerResume::default(), cx)
}

/// Spawn the player for the fullscreen window: continues from `resume`
/// and its control row shows the exit-fullscreen button instead of the
/// enter one.
pub(super) fn spawn_fullscreen(
    path: PathBuf,
    resume: PlayerResume,
    cx: &mut App,
) -> Option<Entity<VideoPlayer>> {
    let player = VideoPlayer::spawn(path, resume, cx)?;
    player.update(cx, |this, cx| {
        this.fullscreen_mode = true;
        this.start_controls_watcher(cx);
        cx.notify();
    });
    Some(player)
}

/// The cover: the library thumbnail at the inspector card's aspect height,
/// falling back to the kind icon when no thumbnail exists.
pub(super) fn cover(data: &AssetPreviewData, cx: &App) -> AnyElement {
    let height = data.card_height();
    match &data.thumb {
        Some(path) => img(path.clone())
            .w_full()
            .h(px(height))
            .object_fit(ObjectFit::Contain)
            .into_any_element(),
        None => fallback::icon_card(data.kind, cx),
    }
}

/// A video player: frames piped out of ffmpeg plus audio, speed and
/// fullscreen controls. Dropping the entity ends the loops, which kill the
/// ffmpeg processes.
pub(super) struct VideoPlayer {
    path: PathBuf,
    facts: VideoStreamFacts,
    /// Frame decoded by the background loop, waiting for its first paint.
    pending: Option<Arc<RenderImage>>,
    /// Frame currently on screen.
    shown: Option<Arc<RenderImage>>,
    playing: bool,
    /// Playhead in milliseconds.
    position_ms: f64,
    /// Restart the pipe at this position (seek request).
    seek_to: Option<u64>,
    slider: Entity<SliderState>,
    volume_slider: Entity<SliderState>,
    /// Whether the file carries an audio stream (volume controls show).
    has_audio: bool,
    /// Playback speed; re-paces the frame loop and re-tempos the audio pipe.
    speed: f32,
    /// Output volume 0.0–1.0, applied on the rodio sink.
    volume: f32,
    muted: bool,
    /// Bumped whenever the audio pipe must restart: after a seek, a speed
    /// change, or the loop wrapping back to zero. The audio task watches it.
    audio_seq: u64,
    /// Last values pushed into the slider states. The states are
    /// user-draggable: re-pushing an unchanged external value every frame
    /// would yank the thumb back mid-drag, so sync only on real changes.
    synced_position: f32,
    synced_volume: f32,
    /// The rodio sink, created by the audio task and shared with the UI so
    /// volume changes apply immediately. `None` until the device opens.
    /// (`Sink` itself is not `Clone` in rodio 0.19 — the `Arc` lets the
    /// task and the entity hold the same voice.)
    sink: Option<Arc<rodio::Sink>>,
    /// Whether this instance lives in the fullscreen window (its control
    /// row then shows an exit-fullscreen button).
    fullscreen_mode: bool,
    /// Whether the volume popup (vertical slider above the button) is open.
    volume_open: bool,
    /// Fullscreen chrome: whether the floating transport row is showing.
    /// Always true outside fullscreen, where the row lives in the layout.
    controls_shown: bool,
    /// Pointer is on the floating row (or its volume popup): the auto-hide
    /// watcher leaves it alone then.
    controls_hovered: bool,
    /// When the pointer last moved in fullscreen; `None` starts the
    /// countdown afresh.
    controls_revealed_at: Option<std::time::Instant>,
    /// Whether the auto-hide watcher was spawned (fullscreen only).
    watcher_started: bool,
    _subscription: Subscription,
}

impl EventEmitter<VideoPlayerEvent> for VideoPlayer {}

impl VideoPlayer {
    /// Build a player entity for `path`, or `None` when the file cannot be
    /// probed (caller keeps showing the static poster).
    fn spawn(path: PathBuf, resume: PlayerResume, cx: &mut App) -> Option<Entity<Self>> {
        let facts = video::probe(&path)?;
        let has_audio = video::has_audio_track(&path);
        Some(cx.new(|cx| Self::with_facts(path, facts, has_audio, resume, cx)))
    }

    fn with_facts(
        path: PathBuf,
        facts: VideoStreamFacts,
        has_audio: bool,
        resume: PlayerResume,
        cx: &mut Context<Self>,
    ) -> Self {
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(facts.duration_ms.max(1) as f32)
                .default_value(resume.position_ms as f32)
        });
        let subscription = cx.subscribe(&slider, |this, _slider, event: &SliderEvent, cx| {
            // Dragging only shows the target; the seek happens on release.
            if let SliderEvent::Release(value) = event {
                let target = value.start().max(0.);
                this.seek_to = Some(target as u64);
                // Reflect immediately: the audio pipe rebuilds from
                // `position_ms`, so a pause-drag-resume would otherwise
                // start the sound at the pre-drag position.
                this.position_ms = target as f64;
                // The audio pipe restarts at the new position too.
                this.audio_seq += 1;
                cx.notify();
            }
        });
        let volume_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                // The default step is 1.0 — on a 0..1 slider that rounds
                // every drag to plain 0 or 1, so the volume would never
                // actually follow the thumb.
                .step(0.01)
                .default_value(if resume.muted { 0. } else { resume.volume })
        });
        // The volume slider outlives the struct field list: detaching keeps
        // the subscription alive for the entity's lifetime.
        cx.subscribe(&volume_slider, |this, _slider, event: &SliderEvent, cx| {
            // Both events carry a value: Change streams live while dragging
            // (audible immediately), Release is the final one.
            let value = match event {
                SliderEvent::Change(value) | SliderEvent::Release(value) => {
                    value.start().clamp(0., 1.)
                }
            };
            // Dragging the volume unmutes, like every desktop player.
            this.volume = value;
            this.muted = false;
            this.synced_volume = value;
            this.apply_volume();
            cx.notify();
        })
        .detach();
        let mut this = Self {
            path,
            facts,
            pending: None,
            shown: None,
            playing: resume.playing,
            position_ms: resume.position_ms,
            seek_to: None,
            slider,
            volume_slider,
            has_audio,
            speed: resume.speed.clamp(SPEEDS[0], SPEEDS[SPEEDS.len() - 1]),
            volume: resume.volume.clamp(0., 1.),
            muted: resume.muted,
            audio_seq: 1,
            synced_position: resume.position_ms as f32,
            synced_volume: if resume.muted { 0. } else { resume.volume },
            sink: None,
            fullscreen_mode: false,
            volume_open: false,
            controls_shown: true,
            controls_hovered: false,
            controls_revealed_at: None,
            watcher_started: false,
            _subscription: subscription,
        };
        this.start_decoding(cx);
        if has_audio && audio_handle(cx).is_some() {
            this.start_audio(cx);
        }
        this
    }

    /// The video decode/playback loop: reads one frame per `frame_ms /
    /// speed`, advances the playhead and loops at the end of the stream.
    /// Exits as soon as the entity is dropped (the preview closed).
    fn start_decoding(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let frame_ms = self.facts.frame_ms() as f64;
        let duration_ms = self.facts.duration_ms as f64;

        cx.spawn(async move |weak, cx| {
            let mut pipe: Option<FramePipe> = None;
            loop {
                let state = weak.update(cx, |this, _cx| {
                    (
                        this.playing,
                        this.seek_to.take(),
                        this.position_ms,
                        this.speed,
                    )
                });
                let Ok((playing, seek, position, speed)) = state else {
                    break;
                };
                if !playing {
                    // Paused: stop consuming so ffmpeg blocks on a full pipe.
                    if pipe.take().is_some() {
                        let _ = weak.update(cx, |_this, cx| cx.notify());
                    }
                    cx.background_executor().timer(IDLE_POLL).await;
                    continue;
                }
                if let Some(target) = seek {
                    pipe = None;
                    if weak
                        .update(cx, |this, _| this.position_ms = target as f64)
                        .is_err()
                    {
                        break;
                    }
                }

                if pipe.is_none() {
                    let at = weak
                        .update(cx, |this, _| this.position_ms)
                        .unwrap_or(position) as u64;
                    let open_path = path.clone();
                    let opened =
                        cx.background_executor()
                            .spawn(async move {
                                FramePipe::open(&open_path, at, video::DEFAULT_MAX_WIDTH)
                            })
                            .await;
                    match opened {
                        Some(opened) => pipe = Some(opened),
                        None => {
                            // Undecodable: stop instead of spinning.
                            let _ = weak.update(cx, |this, cx| {
                                this.playing = false;
                                cx.notify();
                            });
                            break;
                        }
                    }
                }

                let mut decoding = pipe.take().expect("pipe present");
                let (returned, frame) = cx
                    .background_executor()
                    .spawn(async move {
                        let frame = decoding.read_frame();
                        (decoding, frame)
                    })
                    .await;
                let width = returned.width();
                let height = returned.height();
                pipe = Some(returned);

                match frame {
                    Some(bytes) => {
                        let Some(image) = frame_image(width, height, bytes) else {
                            break;
                        };
                        if weak
                            .update(cx, |this, cx| {
                                this.pending = Some(image);
                                this.position_ms += frame_ms;
                                if duration_ms > 0.0 && this.position_ms >= duration_ms {
                                    // Loop like the animated GIF preview.
                                    this.position_ms = 0.0;
                                    this.seek_to = Some(0);
                                    // The audio pipe restarts from zero too.
                                    this.audio_seq += 1;
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                        let paced = (frame_ms / f64::from(speed.max(0.1))).max(1.0) as u64;
                        cx.background_executor()
                            .timer(Duration::from_millis(paced))
                            .await;
                    }
                    None => {
                        // End of stream (or a dead decoder): loop from zero.
                        pipe = None;
                        if weak
                            .update(cx, |this, cx| {
                                this.position_ms = 0.0;
                                this.seek_to = Some(0);
                                this.audio_seq += 1;
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }

    /// The audio loop: streams ~100 ms PCM chunks from the [`AudioPipe`]
    /// into rodio. The pipe restarts whenever `audio_seq` moves (seek,
    /// speed change, loop wrap); the play/pause state is pushed onto the
    /// sink on every turn (and immediately from [`VideoPlayer::set_playing`]),
    /// while pausing also stops reading, which backpressures ffmpeg
    /// instead of drifting.
    fn start_audio(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let Some(host) = audio_handle(cx) else {
            return;
        };

        cx.spawn(async move |weak, cx| {
            let mut pipe: Option<AudioPipe> = None;
            let mut sink: Option<Arc<rodio::Sink>> = None;
            let mut seq: u64 = 0;
            loop {
                let state = weak.update(cx, |this, _cx| {
                    Some((
                        this.playing,
                        this.audio_seq,
                        this.position_ms,
                        this.speed,
                        this.volume,
                        this.muted,
                    ))
                });
                let Ok(Some((playing, new_seq, position, speed, volume, muted))) = state else {
                    break;
                };
                if new_seq != seq {
                    seq = new_seq;
                    // Kill the old pipe before opening the new one.
                    drop(pipe.take());
                    match &sink {
                        Some(s) => s.clear(),
                        None => {
                            let Ok(s) = rodio::Sink::try_new(&host) else {
                                break;
                            };
                            s.set_volume(if muted { 0. } else { volume });
                            let s = Arc::new(s);
                            weak.update(cx, |this, _| this.sink = Some(s.clone())).ok();
                            sink = Some(s);
                        }
                    }
                    let open_path = path.clone();
                    pipe = cx
                        .background_executor()
                        .spawn(async move { AudioPipe::open(&open_path, position as u64, speed) })
                        .await;
                }

                let Some(s) = &sink else {
                    // No sink (device died): idle until the next seq bump
                    // or the player goes away.
                    cx.background_executor().timer(IDLE_POLL).await;
                    continue;
                };
                // Apply the play state before any pipe work. The whole
                // soundtrack is queued as fast as ffmpeg decodes it, so the
                // pipe is often already gone (or never comes back) while
                // the sink still holds minutes of audio — a pause must
                // reach the sink from here, not only from the branches
                // that require a live pipe.
                if playing {
                    s.play();
                } else {
                    s.pause();
                }
                // Take the pipe out so `read_chunk` can run on a 'static
                // background task; hand it back below.
                let Some(mut p) = pipe.take() else {
                    // No pipe (stream ended): idle until the loop wrap
                    // bumps seq.
                    cx.background_executor().timer(IDLE_POLL).await;
                    continue;
                };

                if playing {
                    let (returned, chunk) = cx
                        .background_executor()
                        .spawn(async move {
                            let chunk = p.read_chunk();
                            (p, chunk)
                        })
                        .await;
                    pipe = Some(returned);
                    match chunk {
                        Some(bytes) => {
                            let samples: Vec<i16> = bytes
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|b| i16::from_le_bytes(*b))
                                .collect();
                            s.append(rodio::buffer::SamplesBuffer::new(2, 44_100, samples));
                        }
                        None => {
                            // Stream end: idle until the loop wrap bumps seq.
                            pipe = None;
                            cx.background_executor().timer(IDLE_POLL).await;
                        }
                    }
                } else {
                    // Paused: stop reading so ffmpeg blocks on a full pipe
                    // instead of drifting ahead.
                    pipe = Some(p);
                    cx.background_executor().timer(IDLE_POLL).await;
                }
            }
        })
        .detach();
    }

    /// Push the current volume/mute onto the rodio sink, if one is live.
    fn apply_volume(&mut self) {
        if let Some(sink) = &self.sink {
            sink.set_volume(if self.muted { 0. } else { self.volume });
        }
    }

    /// Change playback speed: re-pace the video loop and rebuild the audio
    /// pipe with the new tempo on its next pass.
    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        if self.speed != speed {
            self.speed = speed;
            self.audio_seq += 1;
            cx.notify();
        }
    }

    /// Resume from `position_ms` after the fullscreen window closed.
    pub(crate) fn resume_from(&mut self, position_ms: f64, playing: bool, cx: &mut Context<Self>) {
        self.position_ms = position_ms;
        self.seek_to = Some(position_ms.max(0.) as u64);
        self.playing = playing;
        self.audio_seq += 1;
        cx.notify();
    }

    /// Play/pause: flip the flag *and* apply it to the audio sink at once.
    ///
    /// The audio loop re-applies the state on every turn, but it can be a
    /// chunk read away from noticing — and once the whole soundtrack has
    /// been queued up front there is no pipe left to loop around, so a
    /// paused picture would keep singing. Acting on the shared sink here
    /// makes the pause immediate and independent of the loop's position.
    fn set_playing(&mut self, playing: bool, cx: &mut Context<Self>) {
        self.playing = playing;
        if let Some(sink) = &self.sink {
            if playing {
                sink.play();
            } else {
                sink.pause();
            }
        }
        cx.notify();
    }

    /// Pause playback (used while the fullscreen window holds the stage).
    pub(crate) fn pause(&mut self, cx: &mut Context<Self>) {
        self.set_playing(false, cx);
    }

    /// Show the fullscreen controls and restart the auto-hide countdown.
    fn reveal_controls(&mut self, cx: &mut Context<Self>) {
        self.controls_revealed_at = Some(std::time::Instant::now());
        if !self.controls_shown {
            self.controls_shown = true;
            cx.notify();
        }
    }

    /// Spawn the watcher that hides the fullscreen controls once the
    /// pointer has rested off them. Only the fullscreen player starts it,
    /// so nothing ticks outside fullscreen.
    fn start_controls_watcher(&mut self, cx: &mut Context<Self>) {
        if self.watcher_started {
            return;
        }
        self.watcher_started = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(CONTROLS_WATCH_INTERVAL)
                    .await;
                let hide = weak.update(cx, |this, _| {
                    this.controls_shown
                        && !this.controls_hovered
                        && !this.volume_open
                        && this
                            .controls_revealed_at
                            .is_some_and(|at| at.elapsed() >= CONTROLS_HIDE_AFTER)
                });
                let Ok(hide) = hide else { break };
                if hide {
                    let _ = weak.update(cx, |this, cx| {
                        this.controls_shown = false;
                        this.controls_revealed_at = None;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Position and playing flag, read by the fullscreen host on exit to
    /// hand the playback state back here.
    pub(crate) fn playback_state(&self) -> (f64, bool) {
        (self.position_ms, self.playing)
    }

    /// The frame element: the current frame, or a dark placeholder until the
    /// first one arrives. Fills whatever container hosts the player.
    fn frame_element(&self) -> AnyElement {
        match &self.shown {
            Some(frame) => img(ImageSource::Render(frame.clone()))
                .max_h_full()
                .max_w_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div().h_full().w_full().into_any_element(),
        }
    }

    /// Transport row: play/pause, scrubber, elapsed / total time, speed
    /// menu, volume, fullscreen.
    fn controls(&self, cx: &mut Context<Self>) -> Div {
        let playing = self.playing;
        let speed = self.speed;
        let muted = self.muted;

        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                Button::new("video-play")
                    .ghost()
                    .xsmall()
                    .icon(if playing {
                        MediaIcon::Pause
                    } else {
                        MediaIcon::Play
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let playing = this.playing;
                        this.set_playing(!playing, cx);
                    })),
            )
            .child(div().flex_1().child(Slider::new(&self.slider).horizontal()))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} / {}",
                        time_label(self.position_ms),
                        time_label(self.facts.duration_ms as f64)
                    )),
            )
            .child(self.speed_control(speed, cx))
            .when(self.has_audio, |row| {
                row.child(self.volume_control(muted, cx))
            })
            .child(
                Button::new("video-fullscreen")
                    .ghost()
                    .xsmall()
                    .icon(if self.fullscreen_mode {
                        MediaIcon::Shrink
                    } else {
                        MediaIcon::Maximize
                    })
                    .tooltip(if self.fullscreen_mode {
                        rust_i18n::t!("video.exit_fullscreen").to_string()
                    } else {
                        rust_i18n::t!("video.fullscreen").to_string()
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.fullscreen_mode {
                            // Leave via the action so the fullscreen host
                            // (which owns the window) handles the exit —
                            // the same path Esc takes.
                            window.dispatch_action(Box::new(ExitVideoFullscreen), cx);
                        } else {
                            cx.emit(VideoPlayerEvent::EnterFullscreen {
                                position_ms: this.position_ms,
                                speed: this.speed,
                                volume: this.volume,
                                muted: this.muted,
                            });
                        }
                    })),
            )
    }

    /// The speed button: its label shows the current rate, its menu picks a
    /// new one.
    fn speed_control(&self, speed: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let options = SPEEDS.to_vec();
        let d = cx.entity();
        Button::new("video-speed")
            .ghost()
            .xsmall()
            .label(format!("{speed:.2}×"))
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                // `Fn` closure: clone out of the capture each call.
                let options = options.clone();
                let mut menu = menu.min_w(px(110.));
                for option in options {
                    let d = d.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{option:.2}×"))
                            .checked(option == speed)
                            .on_click(move |_, _, cx| {
                                d.update(cx, |this, cx| this.set_speed(option, cx));
                            }),
                    );
                }
                menu
            })
    }

    /// The volume control: the state button toggles a vertical slider that
    /// pops up above the control row (only when the file has audio).
    /// Muting happens by dragging the slider to zero — the Change
    /// subscription unmutes as soon as it moves again. The component
    /// `Popover` can't open upwards (its corner placement always extends
    /// down-right from the anchor), so the popup is positioned by hand.
    fn volume_control(&self, muted: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let volume_icon = if muted || self.volume == 0. {
            MediaIcon::VolumeX
        } else if self.volume < 0.5 {
            MediaIcon::Volume1
        } else {
            MediaIcon::Volume2
        };
        let volume_slider = self.volume_slider.clone();
        div()
            .id("volume-anchor")
            .relative()
            .child(
                Button::new("video-mute")
                    .ghost()
                    .xsmall()
                    .icon(volume_icon)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.volume_open = !this.volume_open;
                        cx.notify();
                    })),
            )
            .when(self.volume_open, |anchor| {
                // Hangs off the button's top edge, centred on it; deferred
                // so it paints above the click-away overlay.
                anchor.child(
                    deferred(
                        div()
                            .absolute()
                            .bottom_full()
                            .left(px(-4.))
                            .p_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().popover)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_lg()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(Slider::new(&volume_slider).vertical().h(px(96.))),
                            ),
                    )
                    .with_priority(POPUP_PRIORITY),
                )
            })
    }

    /// Hand the decoded frames back to the window before the entity is
    /// dropped — gpui's sprite atlas never evicts on its own, so simply
    /// dropping the player would leave the last frame resident. Same
    /// contract as the 3D viewport's `release`.
    pub(super) fn release(&mut self, window: &mut Window) {
        if let Some(frame) = self.pending.take() {
            let _ = window.drop_image(frame);
        }
        if let Some(frame) = self.shown.take() {
            let _ = window.drop_image(frame);
        }
    }
}

impl Render for VideoPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Swap in the new frame and hand the old one back to the window so
        // its atlas entry is freed.
        if let Some(frame) = self.pending.take() {
            if let Some(old) = self.shown.take() {
                let _ = window.drop_image(old);
            }
            self.shown = Some(frame);
        }
        // Keep the scrubber on the playhead and the volume slider on the
        // applied level — but only when the external value actually moved.
        // Pushing every frame would fight a mid-drag thumb (the decode loop
        // notifies every frame while playing, so render runs constantly).
        // Release events are the user's seek/apply; this is the mirror back.
        let position = self.position_ms as f32;
        if (position - self.synced_position).abs() >= 0.5 {
            self.synced_position = position;
            self.slider
                .update(cx, |slider, cx| slider.set_value(position, window, cx));
        }
        let volume = if self.muted { 0. } else { self.volume };
        if (volume - self.synced_volume).abs() >= f32::EPSILON {
            self.synced_volume = volume;
            self.volume_slider
                .update(cx, |slider, cx| slider.set_value(volume, window, cx));
        }
        // Fullscreen is a bare picture: the transport row floats over the
        // bottom edge and hides itself while the pointer rests, revealed by
        // movement or by hovering it. Outside fullscreen the row stays in
        // the flow, under the video.
        if self.fullscreen_mode {
            let controls = div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .px_3()
                .py_2()
                .bg(black().opacity(0.55))
                .child(self.controls(cx));
            let mut root = div()
                .relative()
                .size_full()
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                    // The pointer resting on the row (or reaching for it)
                    // counts as hovering: the watcher then leaves it alone.
                    let bottom = f32::from(window.bounds().size.height);
                    let hovering = f32::from(event.position.y) >= bottom - CONTROLS_BAND;
                    this.controls_hovered = hovering;
                    if hovering {
                        this.reveal_controls(cx);
                    } else {
                        this.controls_revealed_at = Some(std::time::Instant::now());
                    }
                }))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .overflow_hidden()
                        .child(self.frame_element()),
                );
            if self.controls_shown {
                root = root.child(controls);
            }
            return root
                .when(self.volume_open, |root| {
                    root.child(div().absolute().inset_0().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.volume_open = false;
                            cx.notify();
                        }),
                    ))
                })
                .into_any_element();
        }

        // Fill the hosting stage: the frame takes all the height the
        // transport controls leave, and the picture contains itself inside.
        // The volume popup is anchored inside, so the root is positioned.
        // While the popup is open a click-away overlay paints over
        // everything (the deferred popup paints above it): any click closes
        // it, exactly like a menu.
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .child(self.frame_element()),
            )
            .child(self.controls(cx))
            .when(self.volume_open, |root| {
                root.child(div().absolute().inset_0().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.volume_open = false;
                        cx.notify();
                    }),
                ))
            })
            .into_any_element()
    }
}

/// Wrap a decoded BGRA frame into the image type gpui renders. The buffer
/// keeps BGRA order but is labelled RGBA, exactly like the APNG path in
/// `panels::common`.
fn frame_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// `m:ss` for the transport label.
fn time_label(ms: f64) -> String {
    let secs = (ms / 1000.0).max(0.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}
