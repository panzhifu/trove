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
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::*;
use trove_core::media::spectrum::{BAND_COUNT, SpectrumAnalyzer, LOW_FFT_SIZE};
use trove_core::media::video::AudioPipe;
use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};

use super::video::IDLE_POLL;

/// Duration of one audio chunk, in milliseconds — must match
/// `trove_core::media::video`'s `AUDIO_CHUNK_BYTES` (100 ms of 44.1 kHz
/// stereo i16). The clock counts finished chunks with it.
const AUDIO_CHUNK_MS: f64 = 100.0;

/// How often the output's device-set fingerprint is refreshed. Enumerating
/// ALSA PCMs is not free, and the set only matters when the default device
/// reports as an alias whose name never changes.
const DEVICE_RESCAN: Duration = Duration::from_secs(2);

/// The dynamic-routing ALSA PCMs, in open-me order. A stream opened on one
/// of them is routed by the sound server, so it lands in whatever the
/// system's default output is at every instant — headphones, speakers,
/// HDMI — with no help from us.
const DYNAMIC_ALIASES: [&str; 2] = ["pulse", "pipewire"];

/// The process-wide audio output. `OutputStream` has to stay alive for as
/// long as any player might make sound — and it stays welded to the device
/// it was opened on, so the host is *rebuildable*: when the system's default
/// output moves (headphones plug in, the mixer's default flips), the stream
/// is replaced wholesale, the generation bumps, and every engine task that
/// sees the new generation requeues its sound from where the playhead
/// stands. An `Rc`, not an `Arc`: cpal's stream types are not `Send`, and
/// every hand that touches this runs on the UI thread.
#[derive(Clone)]
struct AudioOutput(Rc<OutputInner>);

impl Global for AudioOutput {}

struct OutputInner {
    /// The open stream, held alive. Swapped wholesale on a device change.
    stream: Mutex<Option<rodio::OutputStream>>,
    /// The handle new sinks are built from.
    handle: Mutex<Option<rodio::OutputStreamHandle>>,
    /// Name of the device the stream is on, "" while none is open.
    device_name: Mutex<String>,
    /// Fingerprint of the output device set as of the last open. On an ALSA
    /// host the default reports as the alias "default" forever, so the
    /// *name* comparison is blind to a headset plugging in — the set of
    /// enumerated devices is what actually moves.
    device_set: Mutex<String>,
    /// Bumped on every rebuild; engine tasks watch it.
    generation: AtomicU64,
    /// Serializes rebuilds across engine tasks.
    rebuilding: Mutex<()>,
    /// Last device-set scan, for the [`DEVICE_RESCAN`] throttle.
    last_scan: Mutex<Instant>,
}

impl AudioOutput {
    /// An output with no stream. `follow_default` opens it.
    fn empty() -> Self {
        Self(Rc::new(OutputInner {
            stream: Mutex::new(None),
            handle: Mutex::new(None),
            device_name: Mutex::new(String::new()),
            device_set: Mutex::new(String::new()),
            generation: AtomicU64::new(0),
            rebuilding: Mutex::new(()),
            last_scan: Mutex::new(
                Instant::now().checked_sub(DEVICE_RESCAN).unwrap_or(Instant::now()),
            ),
        }))
    }

    /// Fetch or create the process-wide output.
    fn global(cx: &mut App) -> Self {
        if let Some(output) = cx.try_global::<AudioOutput>() {
            return output.clone();
        }
        let output = Self::empty();
        cx.set_global(output.clone());
        output
    }

    /// The handle new sinks are built from, if the output is open.
    fn handle(&self) -> Option<rodio::OutputStreamHandle> {
        self.0.handle.lock().ok()?.clone()
    }

    /// The generation engine tasks watch.
    fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Relaxed)
    }

    /// Open the stream on the best output PCM available. The candidates, in
    /// order: the pulse/pipewire compatibility bridges — they route through
    /// the sound server and follow the system's default sink *on their
    /// own*, so whatever headphones connect next receives the stream
    /// without our touching it — then the bare "default" alias, whose
    /// routing is the distro's guess and can point at hardware that never
    /// moves. The stream re-opens only when the device set changes or the
    /// current device is no longer a candidate; between those, an open
    /// stream is left alone.
    fn follow_default(&self) {
        let scan_due = self
            .0
            .last_scan
            .lock()
            .map(|gate| gate.elapsed() >= DEVICE_RESCAN)
            .unwrap_or(false);
        if !scan_due {
            return;
        }
        if let Ok(mut gate) = self.0.last_scan.lock() {
            *gate = Instant::now();
        }
        let host = rodio::cpal::default_host();
        // One enumeration serves both halves: the fingerprint watches for
        // hotplug, the list names the candidates.
        let Ok(listing) = host.output_devices() else {
            return;
        };
        let mut devices: Vec<(String, rodio::Device)> = listing
            .filter_map(|d| {
                let name = d.name().ok()?;
                Some((name, d))
            })
            .collect();
        devices.sort_by(|a, b| a.0.cmp(&b.0));
        let fingerprint: String = devices
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut candidates: Vec<String> = DYNAMIC_ALIASES
            .iter()
            .filter(|alias| devices.iter().any(|(name, _)| name == *alias))
            .map(|alias| (*alias).to_string())
            .collect();
        if let Some(default) = host.default_output_device().and_then(|d| d.name().ok())
            && !candidates.contains(&default)
        {
            candidates.push(default);
        }
        if candidates.is_empty() {
            return;
        }

        // Re-checked under the lock: another engine's task may have rebuilt
        // for the same move a moment ago. An open stream whose device is
        // still a candidate — and whose device set has not moved under it —
        // stays exactly where it is.
        let guard = self.0.rebuilding.lock();
        let (Ok(current_name), Ok(current_set)) =
            (self.0.device_name.lock(), self.0.device_set.lock())
        else {
            return;
        };
        let settled = *current_set == fingerprint
            && candidates.iter().any(|name| name == &*current_name);
        if settled {
            return;
        }
        drop((current_name, current_set));
        for name in &candidates {
            let Some((_, device)) = devices.iter().find(|(known, _)| known == name) else {
                continue;
            };
            let Ok((stream, handle)) = rodio::OutputStream::try_from_device(device) else {
                tracing::warn!(
                    device = %name,
                    "audio: opening the output device failed; trying the next candidate"
                );
                continue;
            };
            if let Ok(mut slot) = self.0.stream.lock() {
                *slot = Some(stream);
            }
            if let Ok(mut slot) = self.0.handle.lock() {
                *slot = Some(handle);
            }
            if let Ok(mut slot) = self.0.device_name.lock() {
                *slot = name.clone();
            }
            if let Ok(mut slot) = self.0.device_set.lock() {
                *slot = fingerprint.clone();
            }
            let generation = self.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                device = %name,
                generation,
                "audio: output stream (re)opened"
            );
            break;
        }
        drop(guard);
    }
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
    /// Spectrum of the chunk that is audible right now — [`BAND_COUNT`]
    /// log-spaced band values, 0..1 — published by the same stroke as the
    /// level and sampled by the ticker at animation pace.
    spectrum: Arc<Mutex<Vec<f32>>>,
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
            spectrum: Arc::new(Mutex::new(vec![0.0; BAND_COUNT])),
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

    /// The audible chunk's spectrum, [`BAND_COUNT`] values 0..1. Cloned out:
    /// the ticker samples it at animation pace, the task rewrites it at
    /// chunk pace.
    pub(super) fn spectrum(&self) -> Vec<f32> {
        self.spectrum.lock().map(|s| s.clone()).unwrap_or_default()
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
    /// pace against. It also watches the audio output's generation: when the
    /// default device moves, the sink dies with the old stream and the
    /// soundtrack requeues from where the playhead stands.
    fn start(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let output = AudioOutput::global(cx);
        output.follow_default();
        let shared = self.shared.clone();
        let clock = self.clock.clone();
        let sink_slot = self.sink.clone();
        let level_slot = self.level.clone();
        let spectrum_slot = self.spectrum.clone();
        let alive = self.alive.clone();

        cx.spawn(async move |_weak, cx| {
            let mut pipe: Option<AudioPipe> = None;
            let mut sink: Option<Arc<rodio::Sink>> = None;
            let mut seq: u64 = 0;
            let mut base_ms = 0.0f64;
            let mut appended: u64 = 0;
            let mut last_published: Option<(f64, Instant)> = None;
            // Per-chunk analysis, indexed by the same count `appended` uses —
            // the meter and the strip read them back out by how far the sink
            // has drained. ~2 KB per second of audio; the restart clears it.
            // The analyzer wants a rolling mono history long enough for its
            // long window, so the low bands can resolve notes apart.
            let mut levels: Vec<f32> = Vec::new();
            let mut spectra: Vec<Vec<f32>> = Vec::new();
            let mut history: Vec<i16> = Vec::new();
            let mut analyzer = SpectrumAnalyzer::new(44_100.0);
            let mut seen_generation = output.generation();
            let mut force_restart = false;
            loop {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                // Follow the default output device: headphones plug in, the
                // mixer's default flips — the stream is rebuilt, and this
                // sink with it.
                output.follow_default();
                let generation = output.generation();
                if generation != seen_generation {
                    seen_generation = generation;
                    force_restart = true;
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

                if new_seq != seq || force_restart {
                    let device_moved = force_restart;
                    let from_seek = new_seq != seq;
                    force_restart = false;
                    seq = new_seq;
                    // A device move restarts from where the sound stands, not
                    // from the last seek: the clock's last reading, advanced
                    // by wall time while playing, frozen where it was when
                    // paused. A seek keeps its own requested position.
                    base_ms = if from_seek || !device_moved {
                        position
                    } else {
                        last_published
                            .map(|(ms, at)| {
                                if playing {
                                    ms + at.elapsed().as_secs_f64() * 1000.0
                                } else {
                                    ms
                                }
                            })
                            .unwrap_or(position)
                    };
                    appended = 0;
                    last_published = None;
                    levels.clear();
                    spectra.clear();
                    history.clear();
                    // The pipe is restarting: no clock to sync against until
                    // the first chunk is queued again.
                    if let Ok(mut clock) = clock.lock() {
                        *clock = None;
                    }
                    // Kill the old pipe before opening the new one.
                    drop(pipe.take());
                    // A device move welds the old sink to the stream that just
                    // died; drop it so the branch below builds a fresh one. A
                    // seek keeps its sink and just empties it.
                    if device_moved
                        && let Some(s) = sink.take()
                    {
                        s.clear();
                    }
                    match &sink {
                        Some(s) => {
                            s.clear();
                            s.set_volume(if muted { 0. } else { volume });
                        }
                        None => {
                            let Some(handle) = output.handle() else {
                                // No usable output right now: idle until a
                                // device shows up and the generation moves.
                                cx.background_executor().timer(IDLE_POLL).await;
                                continue;
                            };
                            let Ok(s) = rodio::Sink::try_new(&handle) else {
                                cx.background_executor().timer(IDLE_POLL).await;
                                continue;
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
                    let open_at = base_ms;
                    pipe = cx
                        .background_executor()
                        .spawn(async move { AudioPipe::open(&open_path, open_at as u64, speed) })
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
                    // The same chunk's spectrum, for the strip's bars —
                    // blended with the next chunk's by the progress through
                    // this one, so the frame the ticker repeats at animation
                    // pace rides a continuum instead of stepping at chunk
                    // boundaries.
                    let progress = (within / AUDIO_CHUNK_MS).clamp(0.0, 1.0) as f32;
                    let frame = match (spectra.get(audible), spectra.get(audible + 1)) {
                        (Some(a), Some(b)) => a
                            .iter()
                            .zip(b.iter())
                            .map(|(x, y)| x + (y - x) * progress)
                            .collect(),
                        (Some(a), None) => a.clone(),
                        _ => vec![0.0; BAND_COUNT],
                    };
                    if let Ok(mut slot) = spectrum_slot.lock() {
                        *slot = frame;
                    }
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
                            // Mono mix into the rolling history, before the
                            // buffer is handed to the sink and consumed.
                            // The history is the analyzer's memory: the long
                            // low window reads ~371 ms of it, four chunks
                            // deep.
                            let mono: Vec<i16> = samples
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|pair| {
                                    ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16
                                })
                                .collect();
                            history.extend_from_slice(&mono);
                            if history.len() > LOW_FFT_SIZE {
                                let excess = history.len() - LOW_FFT_SIZE;
                                history.drain(0..excess);
                            }
                            spectra.push(analyzer.bands(&history));
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
