//! Animated images: the timing table a scrubber needs.
//!
//! A scrub bar speaks milliseconds; a GIF, APNG or animated WebP speaks
//! per-frame delays. Turning one into the other is all of this module, and it
//! is separated from any player so the arithmetic can be tested without a
//! decoder, a window, or a frame of pixels.
//!
//! ## Why this exists as data rather than as a widget detail
//!
//! gpui plays an animated image by being handed the file and looping it — no
//! seek, no pause, no frame access. To put a playhead on a GIF something has to
//! answer "which frame is at 1 240 ms" and "where in the timeline does frame 12
//! begin", and those answers depend on every frame's delay. So the delays are
//! read out first, once, into the tables here.
//!
//! ## All three containers are already decodable in-crate
//!
//! `image` 0.25.10 implements its `AnimationDecoder` for `GifDecoder`,
//! `ApngDecoder` **and** `WebPDecoder`, and the `png` / `gif` / `webp` features
//! are all on in the workspace `Cargo.toml`. No external process and no new
//! dependency is needed to get frame timings — see
//! [`delays`] and the round-trip test, which builds a real animated GIF and
//! reads its delays back.

use std::path::Path;
use std::time::Duration;

/// The timing of one animated image, in milliseconds from its own start.
///
/// Built from per-frame delays by [`FrameTimes::from_delays`]; every frame has
/// a duration, so `frames() == 0` is the only degenerate case (a file with no
/// frames at all is not an animation).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameTimes {
    /// When each frame begins: `[0, d0, d0+d1, …]`.
    starts_ms: Vec<u64>,
    /// How long each frame is shown.
    delays_ms: Vec<u32>,
}

/// The display floor every major browser applies to animation frames, and
/// Serpent with them: a delay under [`BROWSER_MIN_DELAY_MS`] plays at
/// [`BROWSER_FLOOR_MS`] instead. See [`FrameTimes::from_delays`].
const BROWSER_MIN_DELAY_MS: u32 = 20;
const BROWSER_FLOOR_MS: u32 = 100;

impl FrameTimes {
    /// The empty table: nothing to play.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build the timeline from per-frame delays.
    ///
    /// Delays under two centiseconds play at [`BROWSER_FLOOR_MS`]: the big
    /// browsers standardized that floor decades ago, most GIFs were authored
    /// while watching a browser, and Serpent applies the same rule — so an
    /// asset library playing the same old file 5–10× faster than every viewer
    /// its author ever used is not being more spec-correct, it is showing a
    /// different animation. This deliberately reverses an earlier position
    /// (keep zero delays so compositing frames stay invisible); that position
    /// had its own good argument, but the glitchy 100 ms-per-frame look it
    /// avoids is exactly what the author saw and what everyone else shows.
    pub fn from_delays(delays_ms: Vec<u32>) -> Self {
        // The clamped delays are what the timeline stores: starts, duration
        // and the playback readout are then one truth instead of two.
        let shown: Vec<u32> = delays_ms
            .iter()
            .map(|d| {
                if *d < BROWSER_MIN_DELAY_MS {
                    BROWSER_FLOOR_MS
                } else {
                    *d
                }
            })
            .collect();
        let mut starts = Vec::with_capacity(shown.len());
        let mut at = 0u64;
        for delay in &shown {
            starts.push(at);
            at = at.saturating_add(u64::from(*delay));
        }
        Self {
            starts_ms: starts,
            delays_ms: shown,
        }
    }

    /// How many frames the animation has.
    pub fn frames(&self) -> usize {
        self.starts_ms.len()
    }

    /// Whether there is nothing to play.
    pub fn is_empty(&self) -> bool {
        self.starts_ms.is_empty()
    }

    /// Total run time, which is where the end of a scrub bar sits.
    ///
    /// Saturating: a pathological delay cannot wrap the timeline back to zero.
    pub fn duration_ms(&self) -> u64 {
        self.delays_ms
            .iter()
            .fold(0u64, |acc, d| acc.saturating_add(u64::from(*d)))
    }

    /// The frame to show at `ms`.
    ///
    /// Clamped at both ends — before the start gives frame 0, past the end
    /// gives the last frame — so a caller that lost a race with a reload cannot
    /// panic. Last-wins on a tie, which is what a zero-delay frame needs to
    /// show at all.
    pub fn frame_at(&self, ms: u64) -> usize {
        if self.starts_ms.is_empty() {
            return 0;
        }
        // First start strictly greater than `ms`, minus one, clamped into range.
        let found = self.starts_ms.partition_point(|start| *start <= ms);
        found.saturating_sub(1).min(self.starts_ms.len() - 1)
    }

    /// Where frame `frame` begins on the timeline.
    ///
    /// A frame past the end reports the total duration, which is the honest
    /// answer for "how far along am I" and keeps a scrubbed-to-the-end playhead
    /// pinned at the right edge.
    pub fn ms_at(&self, frame: usize) -> u64 {
        self.starts_ms
            .get(frame)
            .copied()
            .unwrap_or_else(|| self.duration_ms())
    }

    /// The per-frame delays this table was built from.
    pub fn delays(&self) -> &[u32] {
        &self.delays_ms
    }
}

/// Every frame's delay, in milliseconds, for an animated GIF / APNG / WebP.
///
/// `None` means "not an animation" and covers every way a file can fail to be
/// one: a still PNG, a JPEG, a truncated or corrupt container, an unsupported
/// format, a single-frame file. That is deliberately the same contract as the
/// APNG decoder in `trove-app`'s `panels::common`, and for the same reason —
/// the caller's answer to all of them is the static thumbnail, and treating a
/// plain PNG as an error would put a warning in the log for the commonest case
/// in a picture library.
///
/// **The cost of this is a full decode.** `image`'s frame iterator hands back
/// finished buffers, not headers, so every frame's pixels are composited on the
/// way to its delay. Fine for the handful of files a preview opens; not
/// something to call on the app thread. Warm it from a task the way thumbnail
/// generation does.
pub fn delays(path: &Path) -> Option<Vec<u32>> {
    use image::AnimationDecoder as _;

    let head = std::fs::read(path).ok()?;
    // Magic bytes, not the extension: a GIF renamed `.png` still gets its real
    // frame list. APNG is the one case the container lies about — the format is
    // PNG and the animation lives inside it — which is why the PNG arm asks
    // `is_apng` instead of assuming.
    let format = image::guess_format(&head).ok()?;
    let file = std::io::Cursor::new(head);
    let mut reader = std::io::BufReader::new(file);

    let frames: Vec<image::Frame> = match format {
        image::ImageFormat::Gif => image::codecs::gif::GifDecoder::new(&mut reader)
            .ok()?
            .into_frames()
            .collect_frames()
            .ok()?,
        image::ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(&mut reader)
            .ok()?
            .into_frames()
            .collect_frames()
            .ok()?,
        image::ImageFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::new(&mut reader).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            decoder.apng().ok()?.into_frames().collect_frames().ok()?
        }
        _ => return None,
    };

    if frames.len() < MIN_FRAMES {
        return None;
    }
    Some(
        frames
            .iter()
            .map(|frame| {
                u32::try_from(Duration::from(frame.delay()).as_millis()).unwrap_or(u32::MAX)
            })
            .collect(),
    )
}

/// A decoded animation: every frame as RGBA pixels, plus the timing that
/// tells a player how long each one stays on screen.
///
/// The frames are `image::Frame` rather than anything renderable on purpose —
/// the renderer's pixel layout (BGRA for gpui) is the caller's business, and
/// keeping it out of here means the GIF / APNG / WebP paths share one decoder
/// instead of three.
pub struct Decoded {
    pub frames: Vec<image::Frame>,
    pub timing: FrameTimes,
}

/// Decode an animation's frames and timing, or `None` when the file is not one.
///
/// `budget` is a cap on total RGBA bytes, checked *while* decoding so a
/// pathological file is abandoned part way rather than after it has already
/// filled memory. This is the same shape as the APNG decode in the app, and it
/// exists for the same reason: a frame list is only ever held by something on
/// screen, so the cost is bounded by what one preview can show, not by what the
/// library contains.
pub fn decode(path: &Path, budget: u64) -> Option<Decoded> {
    use image::AnimationDecoder as _;

    let head = std::fs::read(path).ok()?;
    let format = image::guess_format(&head).ok()?;
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(head));

    // Materialise the frame iterator per container; the loop below is one
    // budget check rather than three copies of it.
    let raw: Box<dyn Iterator<Item = image::ImageResult<image::Frame>>> = match format {
        image::ImageFormat::Gif => Box::new(
            image::codecs::gif::GifDecoder::new(&mut reader)
                .ok()?
                .into_frames(),
        ),
        image::ImageFormat::WebP => Box::new(
            image::codecs::webp::WebPDecoder::new(&mut reader)
                .ok()?
                .into_frames(),
        ),
        image::ImageFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::new(&mut reader).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            Box::new(decoder.apng().ok()?.into_frames())
        }
        _ => return None,
    };

    let mut frames: Vec<image::Frame> = Vec::new();
    let mut delays: Vec<u32> = Vec::new();
    let mut spent = 0u64;
    for frame in raw {
        // A mid-file decode error ends the list rather than failing the whole
        // thing: what decoded so far still plays, and a truncated tail is
        // exactly the case a preview should degrade instead of refusing.
        let Ok(frame) = frame else { break };
        let (width, height) = frame.buffer().dimensions();
        spent = spent.saturating_add(u64::from(width) * u64::from(height) * 4);
        if spent > budget {
            break;
        }
        delays.push(u32::try_from(Duration::from(frame.delay()).as_millis()).unwrap_or(u32::MAX));
        frames.push(frame);
    }

    if frames.len() < MIN_FRAMES {
        return None;
    }
    Some(Decoded {
        frames,
        timing: FrameTimes::from_delays(delays),
    })
}

/// The timing table for an animated file, or `None` when it is not one.
pub fn frame_times(path: &Path) -> Option<FrameTimes> {
    Some(FrameTimes::from_delays(delays(path)?))
}

/// A one-frame "animation" is a still, and a scrub bar over one frame is not a
/// scrub bar. The player's loop machinery assumes at least two.
const MIN_FRAMES: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic, with no decoder in sight: the mapping has to hold at the
    /// boundaries and in the gaps, because those are where a scrub bar is drawn.
    #[test]
    fn the_table_maps_milliseconds_to_frames_and_back() {
        // 4 frames: 0ms→f0, 100ms→f1, 250ms→f2, 300ms→f3, ending at 500.
        let t = FrameTimes::from_delays(vec![100, 150, 50, 200]);
        assert_eq!(t.frames(), 4);
        assert_eq!(t.duration_ms(), 500);

        assert_eq!(t.frame_at(0), 0, "the first frame owns its start instant");
        assert_eq!(t.frame_at(99), 0);
        assert_eq!(
            t.frame_at(100),
            1,
            "a start instant belongs to the frame starting there"
        );
        assert_eq!(t.frame_at(249), 1);
        assert_eq!(t.frame_at(250), 2);
        assert_eq!(t.frame_at(499), 3);
        assert_eq!(t.frame_at(500), 3, "past the end clamps to the last frame");
        assert_eq!(t.frame_at(u64::MAX), 3, "no panic on an absurd position");

        assert_eq!(t.ms_at(0), 0);
        assert_eq!(t.ms_at(1), 100);
        assert_eq!(t.ms_at(2), 250);
        assert_eq!(t.ms_at(3), 300);
        assert_eq!(
            t.ms_at(99),
            500,
            "a frame past the end reports the duration"
        );

        // Round trip: every frame's own start resolves back to that frame.
        for frame in 0..t.frames() {
            assert_eq!(t.frame_at(t.ms_at(frame)), frame);
        }
    }

    /// A zero-delay frame shares the previous instant, and the round trip is
    /// what a player must still get right — it is also why the delay is not
    /// Sub-20 ms delays — the classic "loop as fast as you like" encodings of
    /// old GIFs — play at the browser floor, which is the whole point of the
    /// clamp: the same file must not play 5–10× faster here than in the
    /// browser it was authored against.
    #[test]
    fn sub_twenty_ms_delays_play_at_the_browser_floor() {
        let t = FrameTimes::from_delays(vec![100, 0, 100]);
        assert_eq!(t.duration_ms(), 300);
        assert_eq!(t.ms_at(1), 100);
        assert_eq!(t.ms_at(2), 200);
        assert_eq!(t.frame_at(250), 2);
        // One centisecond (10 ms) is under the floor too; two (20 ms) is not.
        let edge = FrameTimes::from_delays(vec![10, 19, 20, 21]);
        assert_eq!(edge.duration_ms(), 100 + 100 + 20 + 21);
    }

    #[test]
    fn an_empty_or_single_frame_animation_has_nothing_to_scrub() {
        let empty = FrameTimes::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.frames(), 0);
        assert_eq!(empty.duration_ms(), 0);
        // The lookup is total, not partial: no frame index can be returned for
        // a file with no frames, and callers must not have to special-case it.
        assert_eq!(empty.frame_at(0), 0);
        assert_eq!(empty.ms_at(5), 0);

        let one = FrameTimes::from_delays(vec![40]);
        assert_eq!(one.frames(), 1);
        assert_eq!(one.duration_ms(), 40);
        assert_eq!(one.frame_at(39), 0);
        assert_eq!(one.frame_at(40), 0);
    }

    /// Pathological delays saturate instead of wrapping the timeline back to
    /// zero, which would put the playhead before the content it indexes.
    #[test]
    fn absurd_durations_saturate_rather_than_wrapping() {
        let t = FrameTimes::from_delays(vec![u32::MAX, u32::MAX, u32::MAX]);
        assert_eq!(t.duration_ms(), 3 * u64::from(u32::MAX));
        assert_eq!(t.ms_at(2), 2 * u64::from(u32::MAX));
        assert_eq!(t.frame_at(u64::MAX), 2);
    }

    /// Write a real animated GIF with the given per-frame delays, one distinct
    /// 8x8 frame each.
    ///
    /// `gif` rather than `image`, because `image`'s GIF encoder exposes no
    /// per-frame API at all — `write_image` takes a single still — so the crate
    /// that reads a multi-frame GIF here cannot be the crate that writes one.
    /// Shared by the two tests below so neither trusts a fixture the other
    /// hand-rolled.
    fn write_animated_gif(path: &std::path::Path, delays: &[u32]) {
        use image::RgbaImage;

        let file = std::fs::File::create(path).unwrap();
        let mut encoder = gif::Encoder::new(std::io::BufWriter::new(file), 8, 8, &[]).unwrap();
        for (ix, ms) in delays.iter().enumerate() {
            let mut image = RgbaImage::new(8, 8);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = image::Rgba([(x * 30) as u8, (y * 30) as u8, (ix * 60) as u8, 255]);
            }
            let mut frame = gif::Frame::from_rgba_speed(8, 8, image.as_mut(), 1);
            // The format counts delays in hundredths of a second, so 80 ms is
            // 8 and 40 ms is 4 — both exact, which is why the round trip below
            // can demand the input back unchanged.
            frame.delay = (*ms / 10) as u16;
            encoder.write_frame(&frame).unwrap();
        }
        drop(encoder);
        assert!(path.is_file(), "the fixture was not written");
    }

    /// End to end through a real encoder: a GIF written with known per-frame
    /// delays must read them back.
    ///
    /// This is the measurement behind the claim that frame timings need no
    /// external process. It goes through `gif` rather than `image`'s encoder
    /// because `image::codecs::gif::GifEncoder` exposes no per-frame API at all
    /// — only `write_image` for a single still — so `image` can read an
    /// animated GIF but cannot produce one. A test written against the type
    /// this module invents would prove nothing about real files.
    #[test]
    fn an_animated_gifs_delays_survive_a_real_encode_decode_round_trip() {
        // 80, 120, 40, 200 ms — the third one deliberately short, because that
        // is the frame a naive "assume equal spacing" player gets wrong.
        let delays = [80u32, 120, 40, 200];
        let dir = std::env::temp_dir().join(format!("trove-anim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anim.gif");
        write_animated_gif(&path, &delays);

        let read = super::delays(&path).expect("an animated GIF must yield its delays");
        assert_eq!(read.len(), delays.len(), "frame count changed");
        assert_eq!(
            read,
            delays.to_vec(),
            "delays did not survive the round trip"
        );

        let table = super::frame_times(&path).unwrap();
        assert_eq!(table.duration_ms(), 440);
        assert_eq!(table.frames(), 4);
        assert_eq!(table.ms_at(0), 0);
        assert_eq!(table.ms_at(1), 80);
        assert_eq!(table.ms_at(2), 200);
        assert_eq!(
            table.ms_at(3),
            240,
            "the 40 ms frame is what moved the edge"
        );
        assert_eq!(table.frame_at(200), 2);
        assert_eq!(table.frame_at(239), 2);
        assert_eq!(table.frame_at(240), 3);
        assert_eq!(table.frame_at(439), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `decode` must hand back the frames *and* the timing together: a player
    /// driving its own clock needs both, and they cannot be allowed to disagree
    /// about how many frames there are.
    #[test]
    fn decoding_yields_one_frame_per_delay_and_respects_the_budget() {
        let delays = [100u32, 200, 300];
        let dir = std::env::temp_dir().join(format!("trove-anim-dec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.gif");
        write_animated_gif(&path, &delays);

        let all = super::decode(&path, u64::MAX).expect("a real animated GIF must decode");
        assert_eq!(
            all.frames.len(),
            delays.len(),
            "frame count is not the timing's"
        );
        assert_eq!(all.timing.frames(), all.frames.len());
        assert_eq!(
            all.timing.duration_ms(),
            delays.iter().map(|d| u64::from(*d)).sum::<u64>()
        );
        // Distinct frames, not one frame pushed N times: the fixture varies the
        // blue channel per frame, so that bug shows up here and nowhere else.
        assert_ne!(
            all.frames[0].buffer().as_raw(),
            all.frames[1].buffer().as_raw(),
            "two frames came back identical"
        );

        // A budget below one frame refuses the file outright rather than
        // handing back a truncated animation nobody asked for.
        assert!(
            super::decode(&path, 4).is_none(),
            "a 4-byte budget spent anyway"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A still PNG is not an animation, and must answer `None` rather than
    /// "error": the commonest case in a picture library must not log a warning.
    #[test]
    fn a_still_image_is_not_reported_as_an_animation() {
        use image::{ExtendedColorType, ImageEncoder};

        let dir = std::env::temp_dir().join(format!("trove-anim-still-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("still.png");
        {
            let file = std::fs::File::create(&png).unwrap();
            image::codecs::png::PngEncoder::new(file)
                .write_image(
                    image::RgbaImage::new(4, 4).as_raw(),
                    4,
                    4,
                    ExtendedColorType::Rgba8,
                )
                .unwrap();
        }
        assert!(
            super::delays(&png).is_none(),
            "a single-frame PNG must read as a still, not an animation"
        );
        assert!(super::delays(&dir.join("missing.gif")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
