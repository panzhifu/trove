//! One live card at a time: the tile the pointer rests on plays what it holds.
//!
//! The grid is static everywhere else by design — a hundred thumbnails do not
//! move, and the row layout is frozen so a repaint never re-measures. This is the
//! one place a card is allowed to be alive, and the budget that keeps it honest:
//! **exactly one** tile, and only after the pointer has settled on it for
//! [`SETTLE`]. Sweeping the mouse across a library therefore starts nothing,
//! which is what makes the feature safe to leave on by default.
//!
//! It lives here rather than in `panels::workspace` because the two things it
//! drives — the frame pipe and the soundtrack — belong to the preview module, and
//! a grid cell should not have to widen their APIs to borrow them. The workspace
//! owns the entity, tells it which tile the pointer is on, and asks it what to
//! paint.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::*;
use trove_core::media::video::{self, FramePipe};
use trove_core::model::AssetKind;
use uuid::Uuid;

use super::soundtrack::AudioEngine;
use crate::library::LibraryController;

/// How long the pointer must rest on a tile before it comes alive. Half a second
/// is the difference between "I meant that one" and a jukebox that plays every
/// tile the mouse crosses.
const SETTLE: Duration = Duration::from_millis(500);

/// Width a hover decode is capped to. A card is never wider than this on screen
/// at any zoom the grid offers, and one 320-wide BGRA frame is ~400 KB rather
/// than the ~3.7 MB a 720-wide one costs — this loop must not be able to make
/// the grid slower than the grid itself.
const HOVER_MAX_WIDTH: u32 = 320;

/// How far the pointer must travel across a live card, as a fraction of its
/// width, before it counts as a seek rather than a jitter.
const SEEK_SLOP: f32 = 0.02;

/// How often the presenter looks for a newer frame. The decode loop runs at the
/// clip's own rate; a card repaints at most this often.
const PRESENT: Duration = Duration::from_millis(60);

/// What the decode loop shares with the entity. The loop never borrows the app,
/// so a busy UI thread cannot slow it down; the presenter is the only reader.
#[derive(Default)]
struct Mailbox {
    /// The newest frame, taken by the presenter.
    frame: Option<Arc<RenderImage>>,
    /// Where that frame sits in the clip, as the loop computed it — it is the
    /// only party that knows the duration.
    ratio: f32,
    /// Milliseconds of clip, `0` until the probe has read it.
    duration_ms: u64,
    /// A jump the loop has not served yet.
    seek: Option<u64>,
}

/// The live tile.
struct Live {
    id: Uuid,
    frame: Option<Arc<RenderImage>>,
    ratio: f32,
    /// Milliseconds of clip: the asset's own figure at first, then the probe's,
    /// which is what turns a pointer ratio into a seek target.
    duration_ms: u64,
    /// Both kinds carry sound: a video that hovers silently reads as broken, and
    /// an audio tile has nothing else to announce itself with.
    audio: Option<Entity<AudioEngine>>,
    /// `None` for an audio tile: no picture to advance.
    mailbox: Option<Arc<Mutex<Mailbox>>>,
    /// Cleared when this card stops, which is how the loop learns to end without
    /// the entity having to reach it.
    alive: Option<Arc<AtomicBool>>,
}

/// The grid's one live card.
pub(crate) struct HoverCards {
    /// The tile under the pointer, settled or not.
    over: Option<Uuid>,
    live: Option<Live>,
    /// Bumped by every hover change. A settle timer that wakes to a stale
    /// generation does nothing — cheaper and less racy than cancelling the task
    /// it was waiting for.
    generation: u64,
}

impl HoverCards {
    pub(crate) fn new() -> Self {
        Self {
            over: None,
            live: None,
            generation: 0,
        }
    }

    /// The pointer entered a tile (`Some`) or left the cards (`None`).
    ///
    /// The caller hands over the kind it already has on the paint path: reading
    /// the asset's record here would cost a database lookup for every tile the
    /// mouse crosses.
    pub(crate) fn hovered(
        &mut self,
        id: Option<Uuid>,
        kind: AssetKind,
        controller: &Entity<LibraryController>,
        cx: &mut Context<Self>,
    ) {
        if id == self.over {
            return;
        }
        self.over = id;
        self.generation += 1;
        // Whatever was playing ends the moment the pointer moves off it —
        // including when it moves onto another card, which is the common case.
        self.stop();
        let (Some(id), true) = (id, matches!(kind, AssetKind::Video | AssetKind::Audio)) else {
            cx.notify();
            return;
        };
        let generation = self.generation;
        let entity = cx.entity();
        let controller = controller.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SETTLE).await;
            entity.update(cx, move |this, cx| {
                // The pointer may have moved on, or landed somewhere else,
                // while this was waiting.
                if this.generation == generation && this.over == Some(id) {
                    this.start(id, kind, controller, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The pointer moved to a 0..1 position across the live card: a seek, for
    /// both kinds. The card keeps playing from there rather than freezing — the
    /// pointer is the shuttle, and stopping on a frame is what the full preview
    /// transport is for.
    pub(crate) fn scrubbed(&mut self, id: Uuid, ratio: f32, cx: &mut Context<Self>) {
        let Some(live) = self.live.as_mut().filter(|live| live.id == id) else {
            return;
        };
        let ratio = ratio.clamp(0., 1.);
        if (ratio - live.ratio).abs() < SEEK_SLOP {
            return;
        }
        // Without a known duration there is no target to jump to, and restarting
        // at zero would send a hovering track back to its top on every pointer
        // wobble — so an unknown length simply means the gesture does nothing.
        if live.duration_ms > 0 {
            let target = (ratio as f64 * live.duration_ms as f64) as u64;
            live.ratio = ratio;
            if let Some(mailbox) = &live.mailbox
                && let Ok(mut mailbox) = mailbox.lock()
            {
                mailbox.seek = Some(target);
            }
            // A video's picture and soundtrack are one gesture apart here:
            // leaving the engine at the old position would have them drift for
            // the rest of the dwell.
            if let Some(audio) = &live.audio {
                audio.update(cx, |audio, _| audio.restart_at(target as f64));
            }
            cx.notify();
        }
    }

    /// What the tile `id` paints instead of its still, while it is live.
    pub(crate) fn picture(&self, id: Uuid) -> Option<Arc<RenderImage>> {
        self.live
            .as_ref()
            .filter(|live| live.id == id)
            .and_then(|live| live.frame.clone())
    }

    /// The playhead bar's position for `id`, or `None` when it should not draw
    /// one — which is every tile but the live video, whose waveform-less card
    /// has no other sense of where it is.
    pub(crate) fn playhead(&self, id: Uuid) -> Option<f32> {
        let live = self.live.as_ref().filter(|live| live.id == id)?;
        live.mailbox.is_some().then_some(live.ratio)
    }

    /// Whether `id` is the live tile, which is what decides if a card gets the
    /// pointer handlers at all: a static card must not pay for a move listener.
    pub(crate) fn is_live(&self, id: Uuid) -> bool {
        self.live.as_ref().is_some_and(|live| live.id == id)
    }

    /// Start the tile the settle delay picked.
    fn start(
        &mut self,
        id: Uuid,
        kind: AssetKind,
        controller: Entity<LibraryController>,
        cx: &mut Context<Self>,
    ) {
        // Read here rather than on the paint path: this runs once per deliberate
        // dwell, while a hover change fires for every tile the mouse crosses.
        if !trove_core::config::AppConfig::load().hover_media() {
            return;
        }
        let (path, duration_ms) = {
            let controller = controller.read(cx);
            let record = trove_core::store::assets::get(controller.library.store().conn(), id)
                .ok()
                .flatten();
            (
                controller.asset_file(id),
                record.and_then(|a| a.duration_ms),
            )
        };
        let Some(path) = path.filter(|path| path.is_file()) else {
            return;
        };
        let duration_ms = duration_ms.unwrap_or(0);
        if kind == AssetKind::Audio {
            // No picture to advance, so the card stays the card and only the
            // sound arrives. `spawn` answers `None` with no ffmpeg, no decodable
            // stream, or no audio track at all — and then there is nothing to do,
            // which is the same card the pointer found.
            let audio = AudioEngine::spawn(path, cx);
            if let Some(audio) = &audio {
                audio.update(cx, |audio, _| audio.set_playing(true));
            }
            self.live = audio.map(|audio| Live {
                id,
                frame: None,
                ratio: 0.,
                duration_ms,
                audio: Some(audio),
                mailbox: None,
                alive: None,
            });
            cx.notify();
            return;
        }
        if !video::ffmpeg_available() {
            return;
        }
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let audio = AudioEngine::spawn(path.clone(), cx);
        if let Some(audio) = &audio {
            audio.update(cx, |audio, _| audio.set_playing(true));
        }
        self.live = Some(Live {
            id,
            frame: None,
            ratio: 0.,
            duration_ms,
            audio,
            mailbox: Some(mailbox.clone()),
            alive: Some(alive.clone()),
        });
        self.decode(mailbox, alive, path, cx);
        self.present(id, cx);
        cx.notify();
    }

    /// The decode loop: probe once, then hand frames to the mailbox, jump when
    /// told, and start over at the end of the stream so a long clip never leaves
    /// a card on its last frame.
    fn decode(
        &self,
        mailbox: Arc<Mutex<Mailbox>>,
        alive: Arc<AtomicBool>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        cx.spawn(async move |_, cx| {
            let facts = {
                let path = path.clone();
                cx.background_executor()
                    .spawn(async move { video::probe(&path) })
                    .await
            };
            let (duration_ms, frame_ms) = match facts {
                Some(facts) => (facts.duration_ms, facts.frame_ms()),
                // Undecodable: the card stays a card.
                None => {
                    entity.update(cx, |this, cx| {
                        this.stop();
                        cx.notify();
                    });
                    return;
                }
            };
            if let Ok(mut mailbox) = mailbox.lock() {
                mailbox.duration_ms = duration_ms;
            }
            let mut pipe: Option<FramePipe> = None;
            let mut playhead = 0u64;
            while alive.load(Ordering::SeqCst) {
                let mut opened = pipe.take();
                if let Some(target) = mailbox.lock().ok().and_then(|m| m.seek) {
                    opened = None;
                    playhead = target;
                    if let Ok(mut m) = mailbox.lock() {
                        m.seek = None;
                    }
                }
                let Some(pipe_now) = opened else {
                    let open_path = path.clone();
                    let at = playhead;
                    let fresh = cx
                        .background_executor()
                        .spawn(async move { FramePipe::open(&open_path, at, HOVER_MAX_WIDTH) })
                        .await;
                    match fresh {
                        Some(fresh) => pipe = Some(fresh),
                        None => break,
                    }
                    continue;
                };
                pipe = Some(pipe_now);
                let mut decoding = pipe.take().expect("pipe present");
                let (returned, frame) = cx
                    .background_executor()
                    .spawn(async move {
                        let frame = decoding.read_frame();
                        (decoding, frame)
                    })
                    .await;
                let (width, height) = (returned.width(), returned.height());
                pipe = Some(returned);
                let Some(bytes) = frame else {
                    // End of stream: the exhausted pipe has to go, or the next
                    // turn reads nothing again without ever reopening — a card
                    // that quietly burns a core.
                    pipe = None;
                    playhead = 0;
                    if let Ok(mut m) = mailbox.lock() {
                        m.ratio = 0.;
                    }
                    continue;
                };
                playhead = playhead.saturating_add(frame_ms);
                let image = card_frame(width, height, bytes);
                if let Ok(mut m) = mailbox.lock() {
                    m.frame = image;
                    m.ratio = if duration_ms > 0 {
                        (playhead as f64 / duration_ms as f64).clamp(0., 1.) as f32
                    } else {
                        m.ratio
                    };
                }
                cx.background_executor()
                    .timer(Duration::from_millis(frame_ms))
                    .await;
            }
            entity.update(cx, |this, cx| {
                // The loop ended on its own (a dead decoder, an undecodable
                // file): leave no card half-alive behind it.
                if this.live.as_ref().is_some_and(|live| {
                    live.alive
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::SeqCst))
                }) {
                    this.stop();
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Take the newest frame out of the mailbox on a fixed beat. One timer per
    /// live card, and only one card is ever live.
    fn present(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let entity = cx.entity();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(PRESENT).await;
                // `Entity::update` through an `AsyncApp` reaches the plain
                // app borrow, so this hands back the closure's value rather
                // than a `Result` — the entity is only gone if the update
                // itself fails, which the `is_some_and` below already covers.
                let keep_going = entity.update(cx, |this, cx| {
                    let Some(live) = this.live.as_mut().filter(|live| live.id == id) else {
                        return false;
                    };
                    let Some(mailbox) = live.mailbox.clone() else {
                        return false;
                    };
                    let Ok(mut mailbox) = mailbox.lock() else {
                        return true;
                    };
                    if mailbox.duration_ms > 0 {
                        // The probe's figure beats the container's: it is the
                        // one the loop is wrapping against.
                        live.duration_ms = mailbox.duration_ms;
                    }
                    if let Some(frame) = mailbox.frame.take() {
                        live.frame = Some(frame);
                        live.ratio = mailbox.ratio;
                        cx.notify();
                    }
                    true
                });
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Silence the live card and let its loop finish.
    fn stop(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        if let Some(alive) = &live.alive {
            alive.store(false, Ordering::SeqCst);
        }
        // Dropping the engine is what stops the sound: its sink and its pipe go
        // with it.
        drop(live.audio);
    }
}

/// Wrap a decoded BGRA frame into the image type gpui renders. The buffer keeps
/// BGRA order but is labelled RGBA, exactly as the player's own path does.
fn card_frame(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}
