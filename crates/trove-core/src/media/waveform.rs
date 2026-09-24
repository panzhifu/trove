//! Waveform peaks: a fixed-size envelope of an audio file, cached beside the
//! thumbnails.
//!
//! ## Why the sample rate is the trick
//!
//! A waveform wants the loudest moment of each of a few hundred slices, and
//! the naive way is to decode the whole file at 44.1 kHz and reduce as it
//! streams. Instead the pipe asks ffmpeg for mono at [`ENVELOPE_RATE`] Hz:
//! swresample low-passes before decimating, so a downsampled sample *is* the
//! band's envelope, with the anti-aliasing done by code that has had more
//! scrutiny than anything written here. Three minutes becomes ~54k samples
//! instead of ~8M, which is what makes it affordable to compute on first
//! playback rather than at import.
//!
//! ## Shape of the cache
//!
//! `<cache>/libraries/<slug>/waveforms/<sha[:2]>/<sha>.bin`: an eight-byte
//! header and [`PEAK_COUNT`] bytes of level, 0 = silence to 255 = the file's
//! loudest slice. Like thumbnails it is derived purely from content, so
//! deleting it costs a recompute and nothing else.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::media::proc;

/// Buckets in the envelope. 400 is well past what a transport row a few
/// hundred pixels wide can show, and the file stays under half a kilobyte.
pub const PEAK_COUNT: usize = 400;

/// Mono sample rate fed to the reduction. 300 Hz is ~1.5 samples per bucket for
/// a three-minute file at 400 buckets, so no bucket is ever starved; it is also
/// far below the Nyquist limit for an envelope, which is the point of asking
/// ffmpeg for it.
const ENVELOPE_RATE: &str = "300";

/// Bytes per read while streaming. 4 KiB is 68 samples at this rate, which is
/// plenty for one syscall per many buckets.
const READ_BYTES: usize = 4096;

/// Most samples accepted before giving up: 8M at 300 Hz is ~7.4 hours, well
/// past any real asset, and it bounds memory for a pathological file.
const SAMPLE_CAP: usize = 8_000_000;

const MAGIC: &[u8; 8] = b"TRVWAVE1";

/// Relative cache path for `sha`, e.g. `waveforms/ab/<sha>.bin`.
pub fn rel_path(sha: &str) -> String {
    let (a, b) = sha.split_at(2);
    format!("waveforms/{a}/{b}.bin")
}

/// Absolute cache path for `sha` inside a library's cache root.
pub fn abs_path(root: &Path, sha: &str) -> PathBuf {
    root.join(rel_path(sha))
}

/// The envelope's levels, 0–255 per bucket.
pub type Peaks = Vec<u8>;

/// The cached envelope for `sha`, or `None` when none has been computed yet.
///
/// A read with no side effects: callers on a hot path use this to tell "already
/// drawn" apart from "would cost an ffmpeg pass", which [`load_or_build`]
/// cannot report.
pub fn cached(cache_root: &Path, sha: &str) -> Option<Peaks> {
    read_cached(&abs_path(cache_root, sha))
}

/// The cached envelope for `sha`, building it from `blob_path` on a miss.
///
/// `None` when ffmpeg is unavailable or the file has no decodable audio; the
/// caller then draws the plain slider, which is a worse instrument but not a
/// broken one. A cached file that cannot be read is treated as absent and
/// rebuilt, so one corrupt entry cannot strand a waveform forever.
pub fn load_or_build(cache_root: &Path, sha: &str, blob_path: &Path) -> Option<Peaks> {
    let path = abs_path(cache_root, sha);
    if let Some(peaks) = read_cached(&path) {
        return Some(peaks);
    }
    let peaks = peaks_from_file(blob_path)?;
    write_cached(&path, &peaks);
    Some(peaks)
}

/// Regenerate and overwrite, for the same "force rebuild" maintenance path
/// thumbnails have.
pub fn regenerate(cache_root: &Path, sha: &str, blob_path: &Path) -> Option<Peaks> {
    let peaks = peaks_from_file(blob_path)?;
    write_cached(&abs_path(cache_root, sha), &peaks);
    Some(peaks)
}

/// Put an envelope in the cache as though it had just been decoded. Test-only:
/// every production writer reaches it through [`load_or_build`] or
/// [`regenerate`], which hold a real envelope from the file.
#[cfg(test)]
pub(crate) fn store(cache_root: &Path, sha: &str, peaks: &[u8]) {
    write_cached(&abs_path(cache_root, sha), peaks);
}

fn read_cached(path: &Path) -> Option<Peaks> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != MAGIC.len() + PEAK_COUNT || &bytes[..MAGIC.len()] != MAGIC {
        return None;
    }
    Some(bytes[MAGIC.len()..].to_vec())
}

fn write_cached(path: &Path, peaks: &[u8]) {
    // Written through a temporary file like the thumbnails: a half-written
    // envelope would otherwise be indistinguishable from a quiet ending.
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("tmp");
    let mut bytes = Vec::with_capacity(MAGIC.len() + peaks.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(peaks);
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

/// Decode `path` and reduce it to the envelope.
pub fn peaks_from_file(path: &Path) -> Option<Peaks> {
    let samples = envelope_samples(path)?;
    peaks_from_samples(&samples)
}

/// Stream the file through ffmpeg at [`ENVELOPE_RATE`] and return the absolute
/// sample values, already band-limited by the resampler.
fn envelope_samples(path: &Path) -> Option<Vec<u32>> {
    let _slot = proc::slot();
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-vn",
            "-sn",
            "-map",
            "0:a:0?",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ac",
            "1",
            "-ar",
            ENVELOPE_RATE,
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut out: Vec<u32> = Vec::with_capacity(PEAK_COUNT * 2);
    let mut buffer = vec![0u8; READ_BYTES];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            // A truncated read is still real audio up to here; a file whose
            // decoder died mid-stream is better drawn from what arrived than
            // not drawn at all.
            Err(_) => break,
        };
        // Little-endian pairs, walked by index: `chunks_exact(2)` draws a
        // clippy suggestion whose replacement wants a newer compiler.
        let mut at = 0;
        while at + 1 < read {
            out.push(
                i16::from_le_bytes([buffer[at], buffer[at + 1]])
                    .unsigned_abs()
                    .into(),
            );
            at += 2;
            if out.len() >= SAMPLE_CAP {
                child.kill().ok();
                let _ = child.wait();
                return Some(out);
            }
        }
    }
    child.wait().ok()?;
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Bucket the samples and scale so the loudest slice of the file is 255.
fn peaks_from_samples(samples: &[u32]) -> Option<Peaks> {
    if samples.is_empty() {
        return None;
    }
    // Each bucket owns the time slice [i·len/PEAK_COUNT, (i+1)·len/PEAK_COUNT).
    // Proportional rather than a fixed span so the last bucket gets no special
    // case, and so a file *shorter* than PEAK_COUNT samples is spread across
    // the row instead of running the slice arithmetic off the end — at 300 Hz
    // a doorbell is a few dozen samples, and a fixed span of 1 walked past it.
    let total = samples.len();
    let mut peaks = Vec::with_capacity(PEAK_COUNT);
    let mut loudest = 0u32;
    for index in 0..PEAK_COUNT {
        let from = index * total / PEAK_COUNT;
        let to = (index + 1) * total / PEAK_COUNT;
        let peak = if from >= to {
            0
        } else {
            *samples[from..to].iter().max().unwrap_or(&0)
        };
        loudest = loudest.max(peak);
        peaks.push(peak);
    }
    if loudest == 0 {
        // Digital silence. A flat line at zero, not nothing: the shape is the
        // information, and the row still has to line up with the timeline.
        return Some(vec![0; PEAK_COUNT]);
    }
    Some(
        peaks
            .iter()
            .map(|p| {
                // Floor non-zero peaks at 1 so a quiet slice is still drawn,
                // but leave a true zero at zero: the header of this module
                // promises "0 = silence", and a bucket that held no samples at
                // all must not be drawn as if it did.
                if *p == 0 {
                    0
                } else {
                    ((p * 254) / loudest + 1) as u8
                }
            })
            .collect(),
    )
}

/// How an envelope gets rasterized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Percentage of the image height a full-scale peak spans, shared by the
    /// two halves of the mirror.
    pub span: u32,
    /// Bar colour.
    pub ink: [u8; 3],
    /// `None` leaves the background transparent, which is what an overlay on a
    /// panel wants; a JPEG card has to bring its own paper.
    pub background: Option<[u8; 3]>,
}

impl Style {
    /// The transport row of the audio preview: grey bars over whatever the panel
    /// is painted in.
    pub const PREVIEW: Style = Style {
        span: 44,
        ink: [140, 140, 150],
        background: None,
    };

    /// A grid card for an audio file with no cover art: dark bars on the same
    /// paper the font and model cards are drawn on, tall enough that the shape
    /// is readable at thumbnail size.
    pub const CARD: Style = Style {
        span: 72,
        ink: [0x20, 0x21, 0x24],
        background: Some([0xF7, 0xF6, 0xF3]),
    };
}

/// Rasterize `peaks` into a `w`×`h` RGBA bitmap: one column per pixel, the
/// envelope mirrored about the centre line, which is the shape a waveform is
/// expected to have rather than a bar chart's.
pub fn bitmap(peaks: &[u8], w: u32, h: u32, style: &Style) -> Option<image::RgbaImage> {
    if peaks.is_empty() || w == 0 || h == 0 {
        return None;
    }
    let background = match style.background {
        Some([r, g, b]) => image::Rgba([r, g, b, 255]),
        None => image::Rgba([0, 0, 0, 0]),
    };
    let mut out = image::RgbaImage::from_pixel(w, h, background);
    let ink = image::Rgba([style.ink[0], style.ink[1], style.ink[2], 255]);
    // The envelope has [`PEAK_COUNT`] buckets and the image is whatever size the
    // caller asked for, so each column takes the bucket covering its moment:
    // the identity when the two widths match, a proportional resample when they
    // do not.
    let span = h * style.span / 100;
    let centre = h / 2;
    for x in 0..w {
        let level = u32::from(peaks[x as usize * peaks.len() / w as usize]);
        let full = level * span / 255;
        if full == 0 {
            // A bucket that held nothing stays the background: "0 = silence" is
            // the whole point of showing the shape.
            continue;
        }
        // At least one pixel each side, so a quiet file is a thin band rather
        // than an empty box.
        let half = (full / 2).max(1);
        for y in centre.saturating_sub(half)..=(centre + half).min(h - 1) {
            *out.get_pixel_mut(x, y) = ink;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bucketing is pure, so the shape of an envelope is testable with no
    /// ffmpeg and no file: a loud burst, a quiet tail, digital silence.
    #[test]
    fn buckets_collapse_to_one_peak_per_slice() {
        // 800 samples: 400 buckets of two. First half loud, second half quiet.
        let mut samples = vec![30_000u32; 200];
        samples.extend(vec![300u32; 600]);
        let peaks = peaks_from_samples(&samples).unwrap();
        assert_eq!(peaks.len(), PEAK_COUNT);
        assert_eq!(peaks[0], 255, "the loudest slice sets the scale");
        assert!(
            peaks[100] <= 10,
            "the quiet tail must stay quiet, got {}",
            peaks[100]
        );
        // Every bucket is covered: the last one is not silently dropped.
        assert!(!peaks.last().unwrap().eq(&0), "last bucket read samples");
    }

    /// A file shorter than the bucket count: 300 Hz means a doorbell is a few
    /// dozen samples. This used to run the slice arithmetic past the end of the
    /// buffer and panic.
    #[test]
    fn envelope_samples_shorter_than_the_bucket_count_survive() {
        let samples = vec![500u32; 42];
        let peaks = peaks_from_samples(&samples).unwrap();
        assert_eq!(peaks.len(), PEAK_COUNT);
        assert!(peaks.iter().any(|p| *p > 0), "the sound is still drawn");
        assert!(!peaks.iter().all(|p| *p > 0), "empty slices stay empty");
    }

    #[test]
    fn digital_silence_is_a_flat_line_and_not_an_absence() {
        let samples = vec![0u32; 1000];
        let peaks = peaks_from_samples(&samples).unwrap();
        assert_eq!(peaks.len(), PEAK_COUNT);
        assert!(peaks.iter().all(|p| *p == 0));
    }

    #[test]
    fn no_samples_is_no_envelope() {
        assert!(peaks_from_samples(&[]).is_none());
    }

    /// The transport strip and the grid card are the same drawing at two sizes,
    /// so the geometry is pinned once, here: a bar mirrored about the centre
    /// line, and nothing at all for a bucket that held no sound.
    #[test]
    fn a_bitmap_mirrors_the_envelope_about_its_centre_line() {
        let mut peaks = vec![0u8; PEAK_COUNT];
        peaks[0] = 255;
        let image = bitmap(&peaks, 400, 48, &Style::PREVIEW).unwrap();
        // 44% of 48 rows is 21, and the mirror gives each side 10 of them.
        assert_eq!(image.get_pixel(0, 24).0, [140, 140, 150, 255]);
        assert!(
            image.get_pixel(0, 14).0[3] > 0,
            "the top of the bar is drawn"
        );
        assert_eq!(
            image.get_pixel(0, 13).0,
            [0, 0, 0, 0],
            "one row past the bar is the panel showing through"
        );
        assert_eq!(
            image.get_pixel(1, 24).0,
            [0, 0, 0, 0],
            "a silent bucket must not draw a baseline"
        );
    }

    #[test]
    fn a_card_bitmap_brings_its_own_paper() {
        let peaks = vec![255u8; PEAK_COUNT];
        let image = bitmap(&peaks, 64, 48, &Style::CARD).unwrap();
        // 72% of 48 rows: the bar reaches rows 7..=41, so the corner is paper.
        assert_eq!(image.get_pixel(0, 0).0, [0xF7, 0xF6, 0xF3, 255]);
        assert_eq!(image.get_pixel(0, 24).0, [0x20, 0x21, 0x24, 255]);
    }

    /// The envelope has [`PEAK_COUNT`] buckets and the image has whatever width
    /// was asked for, so each column takes the bucket covering its moment.
    #[test]
    fn a_wider_or_narrower_image_resamples_instead_of_truncating() {
        let peaks = vec![255, 0];
        let image = bitmap(&peaks, 4, 20, &Style::PREVIEW).unwrap();
        assert!(image.get_pixel(0, 10).0[3] > 0, "the loud half");
        assert_eq!(image.get_pixel(3, 10).0, [0, 0, 0, 0], "the quiet half");
    }

    #[test]
    fn an_empty_image_or_an_empty_envelope_draws_nothing() {
        assert!(bitmap(&[], 10, 10, &Style::PREVIEW).is_none());
        assert!(bitmap(&[255], 0, 10, &Style::PREVIEW).is_none());
        assert!(bitmap(&[255], 10, 0, &Style::PREVIEW).is_none());
    }

    #[test]
    fn cache_paths_mirror_the_thumbnail_layout() {
        let sha = "ab".to_string() + &"c".repeat(62);
        assert_eq!(rel_path(&sha), format!("waveforms/ab/{}.bin", &sha[2..]));
        assert!(abs_path(Path::new("/cache"), &sha).starts_with("/cache/waveforms/ab/"));
    }

    /// A header that does not match is a miss, and the caller rebuilds — a
    /// truncated write must not be mistaken for a quiet file.
    #[test]
    fn a_stale_or_truncated_cache_entry_is_treated_as_absent() {
        let dir =
            std::env::temp_dir().join(format!("trove-wave-{}", crate::model::new_id().simple()));
        let sha = "d".repeat(64);
        let path = abs_path(&dir, &sha);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, b"TRVWAVE1").unwrap();
        assert!(
            read_cached(&path).is_none(),
            "header only is not an envelope"
        );

        let mut body = vec![7u8; PEAK_COUNT];
        body[0] = 99;
        std::fs::write(&path, [MAGIC.as_slice(), body.as_slice()].concat()).unwrap();
        let back = read_cached(&path).unwrap();
        assert_eq!(back[0], 99);
        assert_eq!(back.len(), PEAK_COUNT);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
