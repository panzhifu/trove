//! The soundtrack of the previewed video, owned once for the whole
//! playback.
//!
//! Audio used to belong to a player entity, so every window that showed the
//! same video built its own sink and its own ffmpeg pipe. Entering
//! fullscreen paused the panel's sink and started a fresh one from scratch:
//! spawning ffmpeg, seeking and decoding the first chunk takes 50–150 ms,
//! and that silence was audible as a cut when the window opened (and again
//! on the way back).
//!
//! The engine owns the soundtrack instead. Windows (the main-area player and
//! the fullscreen one) only send it commands and read its clock, so opening
//! or closing a window never interrupts the sound. Like the video loop, it
//! keeps no `App` borrow while running: commands and the clock travel
//! through shared blocks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gpui_kit::*;
use trove_core::media::video::AudioPipe;

use super::video::IDLE_POLL;

/// Duration of one audio chunk, in milliseconds — must match
/// `trove_core::media::video`'s `AUDIO_CHUNK_BYTES` (100 ms of 44.1 kHz
/// stereo i16). The clock counts finished chunks with it.
const AUDIO_CHUNK_MS: f64 = 100.0;

/// The process-wide audio output. `OutputStream` has to stay alive for as
/// long as any player might make sound, so it lives in a global; every
/// engine's sink is created from its handle.
struct AudioHost {
    _stream: rodio::OutputStream,
    handle: rodio::OutputStreamHandle,
}

impl Global for AudioHost {}

/// The audio output handle, opening the device on first use. `None` when the
/// device cannot be opened — playback then stays silent (same as the
/// pre-audio behavior) instead of erroring.
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

/// Commands the windows send, and the state the audio task reads. Written
/// from the UI thread, read by the task — no `App` borrow involved.
#[derive(Default)]
struct EngineShared {
    /// Play or hold the sink.
    playing: bool,
    /// Bumped to restart the pipe from `position_ms` (seek, speed change,
    /// the stream wrapping around).
    seq: u64,
    position_ms: f64,
    speed: f32,
    volume: f32,
    muted: bool,
}

/// The audio clock, plus when it was read. Handed to the video loops so
/// they can pace themselves without borrowing this entity.
pub(super) type Clock = Arc<Mutex<Option<(f64, Instant)>>>;
type SinkSlot = Arc<Mutex<Option<Arc<rodio::Sink>>>>;

/// One soundtrack: the sink, the ffmpeg pipe feeding it, and the clock the
/// video loops sync to.
pub(super) struct AudioEngine {
    path: PathBuf,
    shared: Arc<Mutex<EngineShared>>,
    /// Where the soundtrack is, plus when that was read. Empty when nothing
    /// is playing, which tells the video loops to pace themselves.
    clock: Clock,
    /// The live sink, so volume and pause apply immediately instead of at the
    /// task's next turn.
    sink: SinkSlot,
    /// RMS of the chunk that is audible *right now*, as `f32` bits — the
    /// level meter the waveform's bounce follows. Written by the task with
    /// every clock publication, read by the preview's ticker.
    level: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
}

impl AudioEngine {
    /// Build the engine for `path` and start feeding it. The engine's
    /// lifetime is the playback's: it is dropped with the preview panel that
    /// created it, not with a window.
    ///
    /// Whether the file carries an audio stream at all is the caller's call —
    /// answering it is an `ffprobe` round trip, and the video preview pays for
    /// it on a background thread while the poster is on screen. An engine on a
    /// silent file would just feed nothing.
    pub(super) fn spawn(path: PathBuf, cx: &mut App) -> Entity<Self> {
        let engine = cx.new(|_| Self {
            path,
            shared: Arc::new(Mutex::new(EngineShared {
                // The task opens its first pipe when the sequence moves, so
                // the engine starts one ahead of its (zero) counter.
                seq: 1,
                speed: 1.0,
                volume: 1.0,
                ..Default::default()
            })),
            clock: Arc::new(Mutex::new(None)),
            sink: Arc::new(Mutex::new(None)),
            level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            alive: Arc::new(AtomicBool::new(true)),
        });
        engine.update(cx, |engine, cx| engine.start(cx));
        engine
    }

    /// The clock the video loops follow. Handing out the `Arc` lets them
    /// read it without borrowing this entity.
    pub(super) fn clock(&self) -> Clock {
        self.clock.clone()
    }

    /// How loud the sound that is playing this instant is, 0..1 — the RMS of
    /// the chunk sitting at the sink's front. Zero when nothing is playing.
    pub(super) fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Play or hold the soundtrack. Applied on the sink immediately: a pause
    /// must not wait for the task to come back from a chunk read.
    pub(super) fn set_playing(&self, playing: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            // No `seq` bump: play/pause holds the sink where it is (the pipe
            // survives a pause), and a second window asking to play must not
            // send the soundtrack back to the top.
            shared.playing = playing;
        }
        if let Ok(sink) = self.sink.lock()
            && let Some(sink) = sink.as_ref()
        {
            if playing {
                sink.play();
            } else {
                sink.pause();
            }
        }
    }

    /// Restart the pipe at `position_ms`.
    pub(super) fn restart_at(&self, position_ms: f64) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.position_ms = position_ms.max(0.);
            shared.seq += 1;
        }
    }

    /// Playback speed: re-tempod on the next pipe (re)start.
    pub(super) fn set_speed(&self, speed: f32) {
        let Some(position) = self.clock_ms() else {
            return;
        };
        if let Ok(mut shared) = self.shared.lock()
            && shared.speed != speed
        {
            shared.speed = speed;
            // Re-tempo from where the sound is, not from where the pipe last
            // started.
            shared.position_ms = position;
            shared.seq += 1;
        }
    }

    /// Where the soundtrack is right now, extrapolated from the last reading.
    fn clock_ms(&self) -> Option<f64> {
        let (reading, at) = (*self.clock.lock().ok()?)?;
        Some(reading + at.elapsed().as_secs_f64() * 1000.0)
    }

    /// Volume/mute apply on the sink right away.
    pub(super) fn set_volume(&self, volume: f32, muted: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.volume = volume;
            shared.muted = muted;
        }
        if let Ok(sink) = self.sink.lock()
            && let Some(sink) = sink.as_ref()
        {
            sink.set_volume(if muted { 0. } else { volume });
        }
    }

    /// The audio task: feeds ~100 ms PCM chunks into the sink, restarts the
    /// pipe whenever `seq` moves, and publishes the clock the video loops
    /// pace against.
    fn start(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let Some(host) = audio_handle(cx) else {
            return;
        };
        let shared = self.shared.clone();
        let clock = self.clock.clone();
        let sink_slot = self.sink.clone();
        let level_slot = self.level.clone();
        let alive = self.alive.clone();

        cx.spawn(async move |_weak, cx| {
            let mut pipe: Option<AudioPipe> = None;
            let mut sink: Option<Arc<rodio::Sink>> = None;
            let mut seq: u64 = 0;
            let mut base_ms = 0.0f64;
            let mut appended: u64 = 0;
            let mut last_published: Option<(f64, Instant)> = None;
            // RMS per chunk appended since the pipe started, indexed by the
            // same count `appended` uses — the level meter reads them back
            // out by how far the sink has drained. One f32 per 100 ms of
            // audio is ~40 KB an hour; the restart clears it.
            let mut levels: Vec<f32> = Vec::new();
            loop {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                let (playing, new_seq, position, speed, volume, muted) = {
                    let Ok(shared) = shared.lock() else {
                        break;
                    };
                    (
                        shared.playing,
                        shared.seq,
                        shared.position_ms,
                        shared.speed,
                        shared.volume,
                        shared.muted,
                    )
                };

                if new_seq != seq {
                    seq = new_seq;
                    base_ms = position;
                    appended = 0;
                    last_published = None;
                    levels.clear();
                    // The pipe is restarting: no clock to sync against until
                    // the first chunk is queued again.
                    if let Ok(mut clock) = clock.lock() {
                        *clock = None;
                    }
                    // Kill the old pipe before opening the new one.
                    drop(pipe.take());
                    match &sink {
                        Some(s) => {
                            s.clear();
                            s.set_volume(if muted { 0. } else { volume });
                        }
                        None => {
                            let Ok(s) = rodio::Sink::try_new(&host) else {
                                break;
                            };
                            s.set_volume(if muted { 0. } else { volume });
                            let s = Arc::new(s);
                            if let Ok(mut slot) = sink_slot.lock() {
                                *slot = Some(s.clone());
                            }
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
                    // No sink (device died): idle until the next command.
                    cx.background_executor().timer(IDLE_POLL).await;
                    continue;
                };
                // Apply the play state before any pipe work: the whole
                // soundtrack is queued as fast as ffmpeg decodes it, so the
                // pipe is often already gone while the sink still holds
                // minutes of audio — a pause has to reach the sink from here.
                if playing {
                    s.play();
                    // Publish the clock: chunks that have played out, plus
                    // the progress inside the one still playing. `len()`
                    // counts the current chunk too.
                    let queued = s.len() as u64;
                    let finished = appended.saturating_sub(queued);
                    let within = (s.get_pos().as_secs_f64() * 1000.0).min(AUDIO_CHUNK_MS);
                    let mut reading = base_ms + finished as f64 * AUDIO_CHUNK_MS + within;
                    // The chunk at the sink's front is `finished` — the first
                    // one not fully played — and its RMS is what is audible
                    // right now. The whole soundtrack is queued long before it
                    // plays, so a chunk's own decode moment is worthless as a
                    // level reading; the drain position is the sync point.
                    let audible = (finished as usize).min(levels.len().saturating_sub(1));
                    level_slot.store(
                        levels.get(audible).copied().unwrap_or(0.0).to_bits(),
                        Ordering::Relaxed,
                    );
                    // `len()` and `get_pos()` are read one after the other, so
                    // a chunk boundary landing between the two reads counts
                    // one chunk twice and reports the clock a whole chunk
                    // ahead. The soundtrack cannot advance faster than real
                    // time: clamp the reading to that and hold it monotonic,
                    // so a spike is absorbed instead of costing frames.
                    if let Some((last, at)) = last_published {
                        let ceiling = last + at.elapsed().as_secs_f64() * 1000.0 * 1.05 + 5.0;
                        reading = reading.clamp(last, ceiling);
                    }
                    last_published = Some((reading, Instant::now()));
                    if let Ok(mut clock) = clock.lock() {
                        *clock = Some((reading, Instant::now()));
                    }
                } else {
                    s.pause();
                    // Nothing is audible: the meter reads zero, and the
                    // waveform relaxes back to its envelope on the UI side.
                    level_slot.store(0.0f32.to_bits(), Ordering::Relaxed);
                    if let Ok(mut clock) = clock.lock() {
                        *clock = None;
                    }
                }

                // Take the pipe out so `read_chunk` can run on a 'static
                // background task; hand it back below.
                let Some(mut p) = pipe.take() else {
                    // No pipe (stream ended): idle until the loop wrap asks
                    // for a restart.
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
                            // RMS normalized to full scale — the reading the
                            // waveform's bounce displays a moment later, when
                            // this chunk reaches the sink's front.
                            let energy = samples.iter().map(|s| {
                                let v = f32::from(*s) / 32768.0;
                                v * v
                            }).sum::<f32>();
                            let rms =
                                (energy / samples.len().max(1) as f32).sqrt().min(1.0);
                            levels.push(rms);
                            s.append(rodio::buffer::SamplesBuffer::new(2, 44_100, samples));
                            appended += 1;
                        }
                        None => {
                            // Stream end: idle until the video loop wraps and
                            // restarts us.
                            pipe = None;
                            cx.background_executor().timer(IDLE_POLL).await;
                        }
                    }
                } else {
                    // Paused: stop reading so ffmpeg blocks on a full pipe
                    // instead of running ahead.
                    pipe = Some(p);
                    cx.background_executor().timer(IDLE_POLL).await;
                }
            }
        })
        .detach();
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}
