//! The sequence card cycle: one card at a time, and only under the pointer.
//!
//! A run's card would otherwise be a still of its first frame forever — the
//! player in the preview shows what the run looks like in motion, but the grid
//! never does. The carousel is that motion for the grid, shaped by the same
//! two budgets that shape the quick look: exactly **one** card animates, and
//! nothing comes alive on its own — the pointer has to be sitting on the card.
//! A card left behind freezes on the frame it reached, which reads as "paused"
//! rather than snapping back.
//!
//! The frame data lives on the [`Cell`](super::data::Cell) (frozen per layout
//! epoch, like the thumbnail), so this entity holds only who is cycling and
//! where they are; [`build_cell_element`](super::cells::build_cell_element)
//! reads the cursor at paint time the same way it reads the live card's.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// How tightly the clock anchors — the same constant the preview's sequence
/// player uses, for the same reason: shorter than one frame at any fps the
/// store allows, so an anchor never rounds a frame away. Ticks that arrive
/// before a frame is due do nothing but re-arm.
const TICK: Duration = Duration::from_millis(10);

/// The data one cycling card needs, frozen per layout epoch with the cell it
/// hangs off. Thumbnail paths, not originals: a card is a 512-pixel box, and
/// decoding a 50-megapixel render per frame for one glance is the mistake the
/// loupe's comment is about.
#[derive(Debug, Clone)]
pub(super) struct SequenceCardThumbs {
    pub(super) fps: f64,
    pub(super) thumbs: Vec<PathBuf>,
}

/// Who is cycling, and where they are.
struct Active {
    id: Uuid,
    cursor: usize,
    last: Instant,
    card: Arc<SequenceCardThumbs>,
}

pub(super) struct SequenceCarousel {
    active: Option<Active>,
}

impl SequenceCarousel {
    pub(super) fn new() -> Self {
        Self { active: None }
    }

    /// The frame path the card should paint, if it is the one cycling.
    /// `None` for every other card — and for this one before the pointer
    /// arrived, which is what keeps the grid a still wall.
    pub(super) fn frame_for(&self, id: Uuid) -> Option<PathBuf> {
        let active = self.active.as_ref().filter(|active| active.id == id)?;
        active.card.thumbs.get(active.cursor).cloned()
    }

    /// The pointer entered a sequence card: cycle it. Starts on the run's
    /// *second* frame — the first one is already on screen, and a cycle that
    /// re-shows it first reads as nothing happening. A card already cycling
    /// is left alone; a different one takes over, and the clock the old card
    /// started stops on its next wake-up.
    pub(super) fn begin(
        &mut self,
        id: Uuid,
        card: Arc<SequenceCardThumbs>,
        cx: &mut Context<Self>,
    ) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            return;
        }
        self.active = Some(Active {
            id,
            cursor: 1 % card.thumbs.len().max(1),
            last: Instant::now(),
            card,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut due = Instant::now() + TICK;
            loop {
                let now = Instant::now();
                if due > now {
                    cx.background_executor().timer(due - now).await;
                }
                due = (due + TICK).max(Instant::now() + TICK);
                let Ok(still_cycling) = this.update(cx, |this, cx| {
                    // The clock serves the card it was started for: once the
                    // pointer moved elsewhere, this loop ends and the new
                    // card's own clock takes over. An entity drop (panel
                    // closed) ends it the same way.
                    let Some(active) = this.active.as_mut() else {
                        return false;
                    };
                    if active.id != id {
                        return false;
                    }
                    let interval = Duration::from_secs_f64(1.0 / active.card.fps.clamp(1.0, 240.0));
                    if active.last.elapsed() < interval {
                        return true;
                    }
                    active.last = Instant::now();
                    active.cursor = (active.cursor + 1) % active.card.thumbs.len().max(1);
                    cx.notify();
                    true
                }) else {
                    break;
                };
                if !still_cycling {
                    break;
                }
            }
        })
        .detach();
    }

    /// The pointer left a card: freeze it. A no-op when the pointer moved
    /// straight onto a different card, whose `begin` already replaced this
    /// one.
    pub(super) fn end(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.active = None;
            cx.notify();
        }
    }
}
