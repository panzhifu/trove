//! The media transport, shared by the video player and the audio player.
//!
//! Both preview a timeline you can scrub, play, mute and speed up. The state
//! that drives those controls lived in `video.rs`, so adding an audio player
//! meant either duplicating ~30 fields-and-widgets or pointing audio at a
//! half-feature. It is extracted here instead: the fields, the two sliders,
//! the three buttons and the mirror-back that keeps a drag from being reset by
//! the clock.
//!
//! What deliberately stays with each host is the *reaction*. A video seek has
//! to tell the decode loop (and the soundtrack follows the picture), so
//! `VideoPlayer` owns that; an audio seek is one call on the engine. The
//! widgets therefore take the host entity plus a plain method pointer rather
//! than trying to model both behaviours.

use gpui_kit::assets::IconName as MediaIcon;
use gpui_kit::base::{POPUP_PRIORITY, h_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderState};

/// Re-exported so a host wires its sliders without importing from two places.
pub(super) use gpui_kit::component::slider::SliderEvent;
use gpui_kit::component::{ActiveTheme, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Playback speeds both players offer. The range matches what
/// `trove_core::media::video::atempo_filter` can chain for the audio side, so
/// the pitch holds at every preset; the top of the list is a UI choice, not a
/// codec limit.
pub(super) const SPEEDS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// The transport's state: what is playing, where, and how loud.
pub(super) struct Transport {
    /// The user asked for sound/picture to advance.
    pub playing: bool,
    /// A timeline drag is in progress. The playhead follows the pointer and the
    /// media clock is ignored until release; the host also pauses the stream
    /// while set, or the two fight over the same thumb.
    pub seeking: bool,
    /// Where the playhead is, in milliseconds — the unit the clock reports in,
    /// so no conversion hides inside the drag handling.
    pub position_ms: f64,
    /// What the thumbs were last set to. The slider states are user-draggable:
    /// re-pushing an unchanged external value every frame would yank the thumb
    /// back mid-drag, so each mirrors back only on a real change.
    pub synced_position: f32,
    pub synced_volume: f32,
    pub speed: f32,
    pub volume: f32,
    pub muted: bool,
    /// Whether the volume popup is open.
    pub volume_open: bool,
    /// The timeline, in milliseconds.
    pub slider: Entity<SliderState>,
    pub volume_slider: Entity<SliderState>,
}

impl Transport {
    /// Build the state and its two sliders for a clip `duration_ms` long.
    ///
    /// Callers wire the slider subscriptions themselves: the drag and release
    /// handlers are exactly where the two players differ.
    pub(super) fn new(duration_ms: u64, cx: &mut App) -> Self {
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(duration_ms.max(1) as f32)
                .default_value(0.)
        });
        let volume_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                // The default step is 1.0 — on a 0..1 slider that rounds every
                // drag to plain 0 or 1, so volume would never follow the thumb.
                .step(0.01)
                .default_value(1.)
        });
        Self {
            playing: false,
            seeking: false,
            position_ms: 0.,
            synced_position: -1.,
            synced_volume: -1.,
            speed: 1.0,
            volume: 1.0,
            muted: false,
            volume_open: false,
            slider,
            volume_slider,
        }
    }

    /// Mirror the playhead and volume into the thumbs. Called from the host's
    /// `render`, which is where a `Window` lives — the media clock is read from
    /// a background task that has none.
    pub(super) fn sync_sliders(&mut self, window: &mut Window, cx: &mut App) {
        let position = self.position_ms as f32;
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
    }
}

/// Play or hold. The icon and the tooltip both follow the current state, so a
/// stalled `playing` flag cannot lie about what the button will do.
pub(super) fn play_pause_button<H: 'static>(
    playing: bool,
    host: &Entity<H>,
    toggle: fn(&mut H, &mut Context<H>),
) -> AnyElement {
    let host = host.clone();
    Button::new("transport-play-pause")
        .ghost()
        .small()
        .icon(if playing {
            IconName::Pause
        } else {
            IconName::Play
        })
        .tooltip(rust_i18n::t!(if playing { "audio.pause" } else { "audio.play" }).to_string())
        .on_click(move |_, _, cx| {
            host.update(cx, toggle);
        })
        .into_any_element()
}

/// The speed button: the current value as its label, every option checked
/// against it.
pub(super) fn speed_button<H: 'static>(
    speed: f32,
    host: &Entity<H>,
    apply: fn(&mut H, f32, &mut Context<H>),
) -> AnyElement {
    let options = SPEEDS.to_vec();
    let host = host.clone();
    Button::new("transport-speed")
        .ghost()
        .xsmall()
        .label(format!("{speed:.2}×"))
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
            // `Fn` closure: clone out of the capture on each call.
            let options = options.clone();
            let mut menu = menu.min_w(px(110.));
            for option in options {
                let host = host.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("{option:.2}×"))
                        .checked(option == speed)
                        .on_click(move |_, _, cx| {
                            host.update(cx, |this, cx| apply(this, option, cx));
                        }),
                );
            }
            menu
        })
        .into_any_element()
}

/// The volume button plus its popup.
///
/// Muting happens by dragging the slider to zero rather than from a dedicated
/// button — the host's Change subscription unmutes as soon as it moves again.
/// The component `Popover` cannot open upwards (its corner placement always
/// extends down-right from the anchor), so this one is positioned by hand.
pub(super) fn volume_button<H: 'static>(
    volume: f32,
    muted: bool,
    open: bool,
    volume_slider: &Entity<SliderState>,
    host: &Entity<H>,
    toggle_open: fn(&mut H, &mut Context<H>),
    cx: &App,
) -> AnyElement {
    let volume_icon = if muted || volume == 0. {
        MediaIcon::VolumeX
    } else if volume < 0.5 {
        MediaIcon::Volume1
    } else {
        MediaIcon::Volume2
    };
    let volume_slider = volume_slider.clone();
    let host = host.clone();
    // The popup's paint is built inside a `when` closure, which has no context
    // to read a theme from — resolve the two colours up front.
    let radius = cx.theme().radius;
    let popover_bg = cx.theme().popover;
    let border = cx.theme().border;
    div()
        .id("volume-anchor")
        .relative()
        .child(
            Button::new("transport-mute")
                .ghost()
                .xsmall()
                .icon(volume_icon)
                .tooltip(rust_i18n::t!("transport.volume").to_string())
                .on_click(move |_, _, cx| {
                    host.update(cx, toggle_open);
                }),
        )
        .when(open, |anchor| {
            // Hangs off the button's top edge, centred on it; deferred so it
            // paints above the click-away overlay.
            anchor.child(
                deferred(
                    div()
                        .absolute()
                        .bottom_full()
                        .left(px(-4.))
                        .p_1()
                        .rounded(radius)
                        .bg(popover_bg)
                        .border_1()
                        .border_color(border)
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
        .into_any_element()
}

/// The transport row: play/pause, timeline, `m:ss / m:ss`, speed, volume.
pub(super) fn row(
    position_ms: f64,
    duration_ms: u64,
    slider: &Entity<SliderState>,
    play_pause: AnyElement,
    speed: AnyElement,
    volume: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let slider = slider.clone();
    h_flex()
        .items_center()
        .gap_2()
        // Width belongs to whoever places the row: the video stage constrains
        // it, the audio stage centres it.
        .w_full()
        .child(play_pause)
        .child(div().flex_1().child(Slider::new(&slider).horizontal()))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "{} / {}",
                    time(position_ms),
                    time(duration_ms as f64)
                )),
        )
        .child(speed)
        .children(volume)
        .into_any_element()
}

/// `m:ss` from milliseconds.
pub(super) fn time(ms: f64) -> String {
    let secs = (ms / 1000.0).max(0.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}
