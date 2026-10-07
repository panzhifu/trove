//! The floating control bar's visibility, shared by the video and
//! animated-image players — and by the fullscreen stage's corner exit, which
//! hides on the same idle clock but reveals on any pointer movement rather
//! than at the picture's bottom edge.
//!
//! The bar hides itself and comes back when the pointer reaches the player's
//! bottom edge — the arrangement the video fullscreen window already used, now
//! the one both players and both modes follow. What the pointer state drives
//! is a boolean; the fade that boolean plays is [`presence`], so a bar grows
//! in, dissolves out, and reverses mid-fade from the value on screen.

use std::time::{Duration, Instant};

use gpui_kit::base::motion::{Presence, PresenceSample, Transition};
use gpui_kit::*;

/// How long the bar stays after the pointer last revealed it.
const HIDE_AFTER: Duration = Duration::from_millis(2500);

/// How often the watcher checks that countdown.
pub(crate) const WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// How long a chrome surface takes to fade in or out.
const FADE_TIME: Duration = Duration::from_millis(150);

/// Bottom band of the player that counts as "on the bar".
const BAND: f32 = 96.;

/// Sample the show/hide fade of a chrome surface keyed by `id`. The caller
/// paints the surface while [`PresenceSample::should_render`] says so, using
/// [`PresenceSample::progress`] as its opacity: a reveal grows in, a hide
/// stays mounted while it dissolves, and a flip mid-fade reverses from the
/// value on screen rather than restarting from an end. Reduced motion
/// collapses the whole thing back to the bare boolean.
pub(crate) fn presence(
    shown: bool,
    id: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> PresenceSample {
    Presence::new(id, shown)
        .transition(Transition::new(FADE_TIME))
        .sample(window, cx)
}

/// Visibility of the floating control bar: it opens showing, hides once the
/// pointer has rested off it, and comes back when the pointer reaches the
/// player's bottom edge.
pub(crate) struct Chrome {
    shown: bool,
    hovered: bool,
    pinned: bool,
    revealed_at: Instant,
}

impl Chrome {
    pub(crate) fn new() -> Self {
        Self {
            // Showing for the first stretch so the bar is discovered, then
            // the watcher hides it like any other reveal.
            shown: true,
            hovered: false,
            pinned: false,
            revealed_at: Instant::now(),
        }
    }

    pub(crate) fn shown(&self) -> bool {
        self.shown
    }

    /// A menu is open: the bar, and the menu's anchor with it, stays up.
    pub(crate) fn pin(&mut self, pinned: bool) {
        self.pinned = pinned;
    }

    /// Pointer moved over the player. Returns whether the bar just appeared.
    pub(super) fn moved(&mut self, bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
        let bottom = f32::from(bounds.origin.y + bounds.size.height);
        self.hovered = f32::from(position.y) >= bottom - BAND;
        if self.hovered {
            return self.reveal();
        }
        self.revealed_at = Instant::now();
        false
    }

    /// Pointer moved anywhere over the surface. The corner exit answers this
    /// rather than [`Self::moved`]: it sits top-right, so "near the bottom
    /// edge" is the one place it should not key off — any movement is the
    /// gesture that says someone is there.
    pub(crate) fn moved_anywhere(&mut self) -> bool {
        self.revealed_at = Instant::now();
        self.reveal()
    }

    pub(super) fn reveal(&mut self) -> bool {
        self.revealed_at = Instant::now();
        if self.shown {
            return false;
        }
        self.shown = true;
        true
    }

    /// The watcher's tick. Returns whether the bar just hid.
    pub(crate) fn tick(&mut self) -> bool {
        if self.shown && !self.hovered && !self.pinned && self.revealed_at.elapsed() >= HIDE_AFTER {
            self.shown = false;
            return true;
        }
        false
    }
}

/// Run the auto-hide countdown for `H`, notifying it when the bar hides.
pub(super) fn watch<H: 'static>(
    weak: WeakEntity<H>,
    cx: &mut App,
    chrome: fn(&mut H) -> &mut Chrome,
) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(WATCH_INTERVAL).await;
            let Ok(hidden) = weak.update(cx, |this, _| chrome(this).tick()) else {
                break;
            };
            if hidden && weak.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
    .detach();
}
