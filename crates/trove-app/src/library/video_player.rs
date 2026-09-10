//! Silent video playback for the preview dialog.
//!
//! Frames come from an ffmpeg pipe (see `trove_core::media::video`), one at a
//! time: the player keeps a single decoded frame alive and hands the previous
//! one back to the window before painting the next, because gpui's sprite
//! atlas never evicts entries by itself. Playback is muted — there is no
//! audio pipeline, and the preview is about looking, not listening.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::*;

use trove_core::media::video::{self, FramePipe, VideoStreamFacts};

/// How long the decode loop sleeps while paused before looking again.
const IDLE_POLL: Duration = Duration::from_millis(120);

/// A silent video player: a frame piped out of ffmpeg plus transport
/// controls. Dropping the entity ends the loop, which kills ffmpeg.
pub struct VideoPlayer {
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
    _subscription: Subscription,
}

impl VideoPlayer {
    /// Build a player entity for `path`, or `None` when the file cannot be
    /// probed (caller keeps showing the static poster).
    pub fn spawn(path: PathBuf, cx: &mut App) -> Option<Entity<Self>> {
        let facts = video::probe(&path)?;
        Some(cx.new(|cx| Self::with_facts(path, facts, cx)))
    }

    fn with_facts(path: PathBuf, facts: VideoStreamFacts, cx: &mut Context<Self>) -> Self {
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(facts.duration_ms.max(1) as f32)
                .default_value(0.)
        });
        let subscription = cx.subscribe(&slider, |this, _slider, event: &SliderEvent, cx| {
            // Dragging only shows the target; the seek happens on release.
            if let SliderEvent::Release(value) = event {
                this.seek_to = Some(value.start().max(0.) as u64);
                cx.notify();
            }
        });
        let mut this = Self {
            path,
            facts,
            pending: None,
            shown: None,
            playing: true,
            position_ms: 0.0,
            seek_to: None,
            slider,
            _subscription: subscription,
        };
        this.start_decoding(cx);
        this
    }

    /// The decode/playback loop: reads one frame per `frame_ms`, advances the
    /// playhead and loops at the end of the stream. Exits as soon as the
    /// entity is dropped (dialog closed).
    fn start_decoding(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let frame_ms = self.facts.frame_ms();
        let duration_ms = self.facts.duration_ms as f64;

        cx.spawn(async move |weak, cx| {
            let mut pipe: Option<FramePipe> = None;
            loop {
                let state = weak.update(cx, |this, _cx| {
                    (this.playing, this.seek_to.take(), this.position_ms)
                });
                let Ok((playing, seek, position)) = state else {
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
                                this.position_ms += frame_ms as f64;
                                if duration_ms > 0.0 && this.position_ms >= duration_ms {
                                    // Loop like the animated GIF preview.
                                    this.position_ms = 0.0;
                                    this.seek_to = Some(0);
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(frame_ms))
                            .await;
                    }
                    None => {
                        // End of stream (or a dead decoder): loop from zero.
                        pipe = None;
                        if weak
                            .update(cx, |this, cx| {
                                this.position_ms = 0.0;
                                this.seek_to = Some(0);
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

    /// The frame element: the current frame, or a dark placeholder until the
    /// first one arrives.
    fn frame_element(&self) -> AnyElement {
        match &self.shown {
            Some(frame) => img(ImageSource::Render(frame.clone()))
                .max_h(px(480.))
                .max_w(px(700.))
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div().h(px(320.)).w(px(560.)).into_any_element(),
        }
    }

    /// Transport row: play/pause, scrubber, elapsed / total time.
    fn controls(&self, cx: &mut Context<Self>) -> Div {
        let playing = self.playing;
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                Button::new("video-play")
                    .ghost()
                    .xsmall()
                    .icon(if playing {
                        IconName::Pause
                    } else {
                        IconName::Play
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.playing = !this.playing;
                        cx.notify();
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
        // Keep the scrubber on the playhead (release events are seeks, this
        // programmatic update is not).
        let position = self.position_ms as f32;
        self.slider
            .update(cx, |slider, cx| slider.set_value(position, window, cx));
        v_flex()
            .gap_2()
            .w_full()
            .items_center()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .child(self.frame_element()),
            )
            .child(self.controls(cx))
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
