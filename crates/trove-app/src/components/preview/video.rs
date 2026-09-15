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
//! Audio rides a second ffmpeg pipe (signed 16-bit stereo at 44.1 kHz) into
//! rodio — owned by [`AudioEngine`] rather than by this player, so the
//! soundtrack outlives any one window: opening or closing the fullscreen
//! window never interrupts it. Speed changes ride ffmpeg's `atempo` filter
//! so the pitch holds. The two pipes are kept together by the audio
//! clock: rodio only reports the position inside the chunk it is playing,
//! so the player reconstructs the soundtrack's timeline itself (chunks fed
//! minus chunks still queued, plus that position) and the decode loop
//! presents frames against it — waiting for a frame that is early for the
//! clock, dropping one the clock has already passed. That is mpv's
//! `--video-sync=audio` plus `--framedrop`; without audio (or while it is
//! paused, seeking or restarting) frames fall back to wall-clock pacing.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as MediaIcon;
use gpui_kit::base::POPUP_PRIORITY;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trove_core::media::video::{self, FramePipe, VideoStreamFacts};
use trove_core::model::AssetKind;

use super::audio::AudioEngine;
use super::{AssetPreviewData, fallback};
use crate::app::actions::ExitVideoFullscreen;

/// How long the decode loops sleep while paused before looking again.
pub(super) const IDLE_POLL: Duration = Duration::from_millis(120);

/// Fullscreen chrome: how long the pointer must rest before the floating
/// transport row hides itself.
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);

/// How often the fullscreen auto-hide watcher checks that countdown.
const CONTROLS_WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// Bottom band of the fullscreen window that counts as "on the controls":
/// the pointer inside it keeps the floating row visible.
const CONTROLS_BAND: f32 = 96.;

/// How early (relative to the master clock) a frame may be shown. A few
/// milliseconds early is invisible; late is judder.
const SYNC_LEAD_MS: f64 = 8.0;

/// A frame further behind the master clock than this many frame periods is
/// dropped instead of shown late — mpv's `--framedrop`.
const DROP_LATE_FRAMES: f64 = 1.5;

/// How often the presenter looks for a frame the decode task left behind:
/// well under a display refresh, and it notifies only when there is one, so
/// an idle player does not repaint.
const PRESENT_POLL: Duration = Duration::from_millis(4);

/// The playback speeds offered in the menu. The range matches what
/// [`video::atempo_filter`] can chain for the audio side, so the pitch
/// holds at every preset.
const SPEEDS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

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

/// What the panel hands to the fullscreen player: the facts it already
/// probed — so opening the window does not run ffprobe a second time on
/// the click path — and the frame it is showing, so the window opens on a
/// picture instead of a black gap until its own first decode lands.
pub(crate) struct FullscreenSeed {
    pub facts: VideoStreamFacts,
    pub frame: Option<Arc<RenderImage>>,
    /// The panel's soundtrack: the fullscreen window continues it instead of
    /// building a second one, which is what used to cut the sound.
    pub audio: Option<Entity<AudioEngine>>,
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
    let path = data.original.as_ref()?.clone();
    // One engine per playback, owned by the panel: windows come and go
    // without touching the soundtrack.
    let audio = AudioEngine::spawn(path.clone(), cx);
    VideoPlayer::spawn(path, audio, PlayerResume::default(), cx)
}

/// Spawn the player for the fullscreen window: continues from `resume`
/// and its control row shows the exit-fullscreen button instead of the
/// enter one.
pub(super) fn spawn_fullscreen(
    path: PathBuf,
    resume: PlayerResume,
    seed: FullscreenSeed,
    cx: &mut App,
) -> Entity<VideoPlayer> {
    let player =
        cx.new(|cx| VideoPlayer::with_facts(path, seed.facts, seed.audio, seed.frame, resume, cx));
    player.update(cx, |this, cx| {
        this.fullscreen_mode = true;
        this.start_controls_watcher(cx);
        cx.notify();
    });
    player
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

/// One decoded frame on its way to the screen.
struct FrameArrival {
    frame: Arc<RenderImage>,
    /// The playhead after showing it, already wrapped at the end of the
    /// stream.
    position_ms: f64,
}

/// What the decode task shares with the entity instead of borrowing the
/// `App`.
///
/// `AsyncApp::update` takes a `RefCell` borrow of the whole app and runs the
/// closure on the caller's thread, so a decode loop that hopped to the
/// entity once per frame was in practice paced by the UI thread — it waited
/// for whatever the main thread was doing, and painting (worst in
/// fullscreen) is the main thing a UI thread does. The loop now reads its
/// controls and leaves its frames here without touching a borrow at all;
/// the entity mirrors the controls in, and a presenter tick on the UI
/// thread takes the frames out.
#[derive(Default)]
struct PlaybackShared {
    /// Produce frames? `playing` folded with `seeking` (scrubbing counts as
    /// paused).
    playing: bool,
    speed: f32,
    /// A seek the loop has not served yet.
    seek_to: Option<u64>,
    /// The newest frame, taken by the render.
    arrived: Option<FrameArrival>,
}

/// A video player: frames piped out of ffmpeg plus audio, speed and
/// fullscreen controls. Dropping the entity ends the loops, which kill the
/// ffmpeg processes.
pub(super) struct VideoPlayer {
    path: PathBuf,
    facts: VideoStreamFacts,
    /// The decode task's controls and mailbox — see [`PlaybackShared`].
    shared: Arc<Mutex<PlaybackShared>>,
    /// Cleared when this entity drops: how the decode task learns to stop
    /// without borrowing the app to look for it.
    alive: Arc<AtomicBool>,
    /// Frame currently on screen.
    shown: Option<Arc<RenderImage>>,
    playing: bool,
    /// Playhead in milliseconds.
    position_ms: f64,
    slider: Entity<SliderState>,
    volume_slider: Entity<SliderState>,
    /// The soundtrack, shared with every window showing this video: it is
    /// what survives entering and leaving fullscreen without a cut.
    audio: Option<Entity<AudioEngine>>,
    /// The engine's clock, held as the shared slot so the decode loop can
    /// read it without borrowing the engine (or the app).
    clock: Option<super::audio::Clock>,
    /// Playback speed; re-paces the frame loop and re-tempos the audio pipe.
    speed: f32,
    /// Output volume 0.0–1.0, applied on the rodio sink.
    volume: f32,
    muted: bool,
    /// Last values pushed into the slider states. The states are
    /// user-draggable: re-pushing an unchanged external value every frame
    /// would yank the thumb back mid-drag, so sync only on real changes.
    synced_position: f32,
    synced_volume: f32,
    /// Whether this instance lives in the fullscreen window (its control
    /// row then shows an exit-fullscreen button).
    fullscreen_mode: bool,
    /// Whether the volume popup (vertical slider above the button) is open.
    volume_open: bool,
    /// The audio clock: where the soundtrack is (milliseconds on the
    /// timeline) and when that reading was taken, so the decode loop can
    /// extrapolate between updates. `None` while there is nothing to sync
    /// against — no audio stream, a dead device, a paused or restarting
    /// pipe — and then frames are paced against the wall clock instead.
    /// This is mpv's `--video-sync=audio`: the sound is the master.
    /// Set while the user drags the scrubber: the playhead previews the
    /// drag and the loop stops feeding it, so the thumb is not yanked back
    /// to the playing position every frame.
    seeking: bool,
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

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // The decode and audio tasks watch this instead of borrowing the
        // app to notice the entity is gone.
        self.alive.store(false, Ordering::Relaxed);
    }
}

impl VideoPlayer {
    /// Build a player entity for `path`, or `None` when the file cannot be
    /// probed (caller keeps showing the static poster).
    fn spawn(
        path: PathBuf,
        audio: Option<Entity<AudioEngine>>,
        resume: PlayerResume,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        let facts = video::probe(&path)?;
        Some(cx.new(|cx| Self::with_facts(path, facts, audio, None, resume, cx)))
    }

    fn with_facts(
        path: PathBuf,
        facts: VideoStreamFacts,
        audio: Option<Entity<AudioEngine>>,
        first_frame: Option<Arc<RenderImage>>,
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
            match event {
                // Dragging previews the target and freezes the playhead: the
                // decode loop leaves `position_ms` alone while `seeking`, so
                // the thumb follows the pointer instead of being reset to
                // the playing position on every frame. The seek itself
                // happens on release, like every other desktop player.
                SliderEvent::Change(value) => {
                    this.seeking = true;
                    this.position_ms = (value.start().max(0.)) as f64;
                    this.apply_playing_state(cx);
                    this.publish_controls();
                    cx.notify();
                }
                SliderEvent::Release(value) => {
                    let target = value.start().max(0.);
                    this.seeking = false;
                    if let Ok(mut shared) = this.shared.lock() {
                        shared.seek_to = Some(target as u64);
                    }
                    // The soundtrack jumps with the picture, at once.
                    if let Some(audio) = &this.audio {
                        audio.read(cx).restart_at(f64::from(target));
                    }
                    // Reflect immediately: the audio pipe rebuilds from
                    // `position_ms`, so a pause-drag-resume would otherwise
                    // start the sound at the pre-drag position.
                    this.position_ms = target as f64;
                    // The audio pipe restarts at the new position too.
                    this.apply_playing_state(cx);
                    this.publish_controls();
                    cx.notify();
                }
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
            this.apply_volume(cx);
            cx.notify();
        })
        .detach();
        let clock = audio.as_ref().map(|audio| audio.read(cx).clock());
        let shared = Arc::new(Mutex::new(PlaybackShared {
            playing: resume.playing,
            speed: resume.speed.clamp(SPEEDS[0], SPEEDS[SPEEDS.len() - 1]),
            ..Default::default()
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let mut this = Self {
            path,
            facts,
            // The fullscreen window is seeded with the frame the panel was
            // showing; a fresh panel player starts black.
            shown: first_frame,
            playing: resume.playing,
            position_ms: resume.position_ms,
            slider,
            volume_slider,
            audio,
            clock,
            speed: resume.speed.clamp(SPEEDS[0], SPEEDS[SPEEDS.len() - 1]),
            volume: resume.volume.clamp(0., 1.),
            muted: resume.muted,
            synced_position: resume.position_ms as f32,
            synced_volume: if resume.muted { 0. } else { resume.volume },
            fullscreen_mode: false,
            seeking: false,
            volume_open: false,
            controls_shown: true,
            controls_hovered: false,
            controls_revealed_at: None,
            watcher_started: false,
            shared,
            alive,
            _subscription: subscription,
        };
        this.start_decoding(cx);
        this.start_presenter(cx);
        // The engine may be shared with other windows: it follows this
        // player's state from the start, so playing here plays everywhere.
        this.apply_volume(cx);
        this.apply_playing_state(cx);
        this
    }

    /// The video decode/playback loop: reads one frame per `frame_ms /
    /// speed`, advances the playhead and loops at the end of the stream.
    /// Exits as soon as the entity is dropped (the preview closed).
    fn start_decoding(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let frame_ms = self.facts.frame_ms() as f64;
        let duration_ms = self.facts.duration_ms as f64;

        let shared = self.shared.clone();
        let alive = self.alive.clone();
        let clock = self.clock.clone();
        let start_ms = self.position_ms;
        cx.spawn(async move |weak, cx| {
            let mut pipe: Option<FramePipe> = None;
            // The loop's own playhead, seeded where the player stands and
            // advanced frame by frame; it travels back to the entity with
            // each arrival, so no frame has to borrow the app to report one.
            let mut playhead = start_ms;
            // Frame delivery is scheduled against the wall clock: frame `n`
            // is due one period after frame `n - 1`. Sleeping a fixed
            // interval *after* each frame's work would add that work time to
            // every period, so the picture would run slower than the file —
            // which reads as juddering. Falling behind simply skips the
            // wait: only the newest frame is kept, so late frames are
            // dropped instead of replayed in slow motion.
            let mut due = Instant::now();
            loop {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                // Controls come from the shared block: no `App` borrow, so
                // this loop is never paced by whatever the UI thread paints.
                let (playing, speed, seek) = {
                    let Ok(mut shared) = shared.lock() else {
                        break;
                    };
                    (shared.playing, shared.speed, shared.seek_to.take())
                };
                // The clock lives with the engine; reading its slot keeps
                // this loop free of app borrows.
                let clock = clock
                    .as_ref()
                    .and_then(|clock| clock.lock().ok().and_then(|reading| *reading))
                    .map(|(ms, at)| ms + at.elapsed().as_secs_f64() * 1000.0);
                if !playing {
                    // Paused (or scrubbing): let go of the pipe so ffmpeg
                    // blocks on a full one instead of running ahead.
                    pipe = None;
                    due = Instant::now();
                    cx.background_executor().timer(IDLE_POLL).await;
                    continue;
                }
                if let Some(target) = seek {
                    pipe = None;
                    due = Instant::now();
                    playhead = target as f64;
                }

                if pipe.is_none() {
                    let at = playhead.max(0.) as u64;
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
                                this.publish_controls();
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
                        // Present at the frame's due time, mpv's way: when
                        // audio is running its clock is the master, so a
                        // frame early for the clock waits and one the clock
                        // has already passed is dropped instead of shown
                        // late. Without audio the wall-clock deadline below
                        // plays the same role.
                        // The frame just read is the one due at `playhead`.
                        let late =
                            clock.is_some_and(|ms| ms - playhead > frame_ms * DROP_LATE_FRAMES);
                        if !late {
                            match clock {
                                Some(ms) => {
                                    let wait = playhead - SYNC_LEAD_MS - ms;
                                    if wait > 0.0 {
                                        cx.background_executor()
                                            .timer(Duration::from_secs_f64(wait / 1000.0))
                                            .await;
                                    }
                                }
                                None => {
                                    let period = Duration::from_secs_f64(
                                        (frame_ms / f64::from(speed.max(0.1)) / 1000.0).max(0.001),
                                    );
                                    due += period;
                                    let now = Instant::now();
                                    if due > now {
                                        cx.background_executor().timer(due - now).await;
                                    } else {
                                        // Late: re-anchor so a burst of lag
                                        // cannot make the loop sprint through
                                        // a backlog of waits.
                                        due = now;
                                    }
                                }
                            }
                        } else {
                            tracing::debug!(
                                position = playhead,
                                clock,
                                "dropped a late frame to stay with the audio"
                            );
                        }
                        playhead += frame_ms;
                        if duration_ms > 0.0 && playhead >= duration_ms {
                            // Loop like the animated GIF preview: back to the
                            // top, request a restart from the loop itself and
                            // have the soundtrack jump there too.
                            playhead = 0.0;
                            if let Ok(mut shared) = shared.lock() {
                                shared.seek_to = Some(0);
                            }
                            if weak
                                .update(cx, |this, cx| {
                                    if let Some(audio) = &this.audio {
                                        audio.read(cx).restart_at(0.0);
                                    }
                                    cx.notify();
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        if !late && let Ok(mut shared) = shared.lock() {
                            shared.arrived = Some(FrameArrival {
                                frame: image,
                                position_ms: playhead,
                            });
                        }
                    }
                    None => {
                        // End of stream (or a dead decoder): loop from zero.
                        pipe = None;
                        playhead = 0.0;
                        if let Ok(mut shared) = shared.lock() {
                            shared.seek_to = Some(0);
                        }
                        if weak
                            .update(cx, |this, cx| {
                                if let Some(audio) = &this.audio {
                                    audio.read(cx).restart_at(0.0);
                                }
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

    /// Push the current volume/mute onto the shared engine, if there is one.
    fn apply_volume(&self, cx: &App) {
        if let Some(audio) = &self.audio {
            audio.read(cx).set_volume(self.volume, self.muted);
        }
    }

    /// Change playback speed: re-pace the video loop and rebuild the audio
    /// pipe with the new tempo on its next pass.
    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        if self.speed != speed {
            self.speed = speed;
            if let Some(audio) = &self.audio {
                audio.read(cx).set_speed(speed);
            }
            // The decode loop paces by `shared.speed`: a speed change that
            // is not published there would leave the picture on the old
            // tempo while the sound followed the new one.
            self.publish_controls();
            cx.notify();
        }
    }

    /// Resume from `position_ms` after the fullscreen window closed.
    pub(crate) fn resume_from(&mut self, position_ms: f64, playing: bool, cx: &mut Context<Self>) {
        self.position_ms = position_ms;
        self.playing = playing;
        if let Ok(mut shared) = self.shared.lock() {
            shared.seek_to = Some(position_ms.max(0.) as u64);
        }
        // The soundtrack played straight through the fullscreen window, so
        // it is already where the picture should be: only the video jumps.
        self.apply_playing_state(cx);
        self.publish_controls();
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
        self.apply_playing_state(cx);
        self.publish_controls();
        cx.notify();
    }

    /// Push the effective play state (playing, but not while scrubbing)
    /// onto the shared sink, if one is live.
    fn apply_playing_state(&self, cx: &App) {
        if let Some(audio) = &self.audio {
            // Scrubbing counts as paused: the engine holds the sound while
            // the user drags, and picks it up again from the new position.
            audio.read(cx).set_playing(self.playing && !self.seeking);
        }
    }

    /// Stop the picture without touching the soundtrack: what the panel does
    /// when the fullscreen window takes the stage over. The engine keeps
    /// playing, so the sound never stops.
    pub(crate) fn pause_video(&mut self, cx: &mut Context<Self>) {
        self.playing = false;
        self.publish_controls();
        cx.notify();
    }

    /// Take the frames the decode task left in the mailbox: a tick on the
    /// UI thread, so presenting costs no cross-thread borrow — the decode
    /// loop no longer waits for a paint, and a paint no longer waits for the
    /// loop. It notifies only when there is something to show, and slows to
    /// a crawl while paused so an idle player stays idle.
    fn start_presenter(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            loop {
                let Ok(playing) = weak.update(cx, |this, cx| {
                    let ready = this
                        .shared
                        .lock()
                        .map(|shared| shared.arrived.is_some())
                        .unwrap_or(false);
                    if ready {
                        cx.notify();
                    }
                    this.playing
                }) else {
                    break;
                };
                let tick = if playing {
                    PRESENT_POLL
                } else {
                    Duration::from_millis(250)
                };
                cx.background_executor().timer(tick).await;
            }
        })
        .detach();
    }

    /// Mirror the controls the decode task reads into the shared block. Any
    /// user action that changes them calls this.
    fn publish_controls(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.playing = self.playing && !self.seeking;
            shared.speed = self.speed;
        }
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

    /// The probed stream facts; the fullscreen window reuses them instead
    /// of probing the file a second time.
    pub(crate) fn facts(&self) -> VideoStreamFacts {
        self.facts
    }

    /// Whether the file carries an audio stream.
    pub(crate) fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// The soundtrack, for the fullscreen window to share.
    pub(crate) fn audio(&self) -> Option<Entity<AudioEngine>> {
        self.audio.clone()
    }

    /// The frame currently on screen, used to seed the fullscreen window so
    /// it opens on a picture instead of black.
    pub(crate) fn current_frame(&self) -> Option<Arc<RenderImage>> {
        self.shown.clone()
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
            .when(self.has_audio(), |row| {
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
        if let Some(arrival) = self
            .shared
            .lock()
            .ok()
            .and_then(|mut shared| shared.arrived.take())
        {
            let _ = window.drop_image(arrival.frame);
        }
        if let Some(frame) = self.shown.take() {
            let _ = window.drop_image(frame);
        }
    }
}

impl Render for VideoPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Swap in whatever the decode task left behind and hand the old
        // frame back to the window so its atlas entry is freed. The playhead
        // travels with the frame, so the timeline advances without the loop
        // borrowing the app to report it.
        let arrival = self
            .shared
            .lock()
            .ok()
            .and_then(|mut shared| shared.arrived.take());
        if let Some(arrival) = arrival {
            if let Some(old) = self.shown.take() {
                let _ = window.drop_image(old);
            }
            self.shown = Some(arrival.frame);
            self.position_ms = arrival.position_ms;
        }
        // Keep the scrubber on the playhead and the volume slider on the
        // applied level — but only when the external value actually moved.
        // Pushing every frame would fight a mid-drag thumb (the decode loop
        // notifies every frame while playing, so render runs constantly).
        // Release events are the user's seek/apply; this is the mirror back.
        let position = self.position_ms as f32;
        // Never mirror while the user is dragging the thumb: the decode loop
        // notifies every frame, so the value would be reset under the
        // pointer and the drag would never take.
        if !self.seeking && (position - self.synced_position).abs() >= 0.5 {
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
                // The theme's own surface instead of a translucent black
                // scrim: buttons, slider and time label keep the contrast
                // they were designed for (a scrim leaves them dark-on-dark
                // in a light theme).
                .bg(cx.theme().popover)
                .border_t_1()
                .border_color(cx.theme().border)
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
