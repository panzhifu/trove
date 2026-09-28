//! The floating control bar's visibility, shared by the video and
//! animated-image players.
//!
//! The bar hides itself and comes back when the pointer reaches the player's
//! bottom edge — the arrangement the video fullscreen window already used, now
//! the one both players and both modes follow.

use std::time::{Duration, Instant};

use gpui_kit::*;

/// How long the bar stays after the pointer last revealed it.
const HIDE_AFTER: Duration = Duration::from_millis(2500);

/// How often the watcher checks that countdown.
const WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// Bottom band of the player that counts as "on the bar".
const BAND: f32 = 96.;

/// Visibility of the floating control bar: it opens showing, hides once the
/// pointer has rested off it, and comes back when the pointer reaches the
/// player's bottom edge.
pub(super) struct Chrome {
    shown: bool,
    hovered: bool,
    pinned: bool,
    revealed_at: Instant,
}

impl Chrome {
    pub(super) fn new() -> Self {
        Self {
            // Showing for the first stretch so the bar is discovered, then
            // the watcher hides it like any other reveal.
            shown: true,
            hovered: false,
            pinned: false,
            revealed_at: Instant::now(),
        }
    }

    pub(super) fn shown(&self) -> bool {
        self.shown
    }

    /// A menu is open: the bar, and the menu's anchor with it, stays up.
    pub(super) fn pin(&mut self, pinned: bool) {
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

    pub(super) fn reveal(&mut self) -> bool {
        self.revealed_at = Instant::now();
        if self.shown {
            return false;
        }
        self.shown = true;
        true
    }

    /// The watcher's tick. Returns whether the bar just hid.
    pub(super) fn tick(&mut self) -> bool {
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
