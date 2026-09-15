//! Video probing and frame extraction through the system `ffmpeg` /
//! `ffprobe`.
//!
//! Both are optional runtime dependencies (never linked, just executed): when
//! either is missing the caller keeps showing the static poster frame. The
//! player in `trove-app` reads raw BGRA frames from an ffmpeg pipe, so a
//! preview never decodes a whole clip into memory.

use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

/// Width cap for decoded frames: 720p BGRA is ~3.7 MB per frame and the
/// player keeps a single frame alive at a time.
pub const DEFAULT_MAX_WIDTH: u32 = 720;

/// Frame rate assumed when the container reports a nonsense value (`0/0`).
const FALLBACK_FPS: f64 = 30.0;

/// Whether `ffmpeg` is on PATH. Probed once per preview; the result is cheap
/// enough (a `-version` spawn) not to cache.
pub fn ffmpeg_available() -> bool {
    runs("ffmpeg")
}

/// Whether `ffprobe` is on PATH.
pub fn ffprobe_available() -> bool {
    runs("ffprobe")
}

/// Run `program -version` and report whether it exits successfully.
fn runs(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Geometry and timing of a video's first stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStreamFacts {
    pub width: u32,
    pub height: u32,
    /// Frames per second, rounded to the nearest integer (pacing does not
    /// need sub-Hertz precision).
    pub fps: u32,
    /// Duration in milliseconds; `0` when the container does not say.
    pub duration_ms: u64,
}

impl VideoStreamFacts {
    /// Milliseconds between frames at this frame rate.
    pub fn frame_ms(&self) -> u64 {
        if self.fps == 0 {
            1000 / FALLBACK_FPS as u64
        } else {
            1000 / u64::from(self.fps)
        }
    }
}

/// Parse an ffprobe frame rate: `"30000/1001"`, `"30/1"` or a bare `"30"`.
/// Unparseable and zero rates become `0.0` (the caller substitutes a
/// fallback).
pub fn parse_frame_rate(raw: &str) -> f64 {
    let raw = raw.trim();
    if raw.is_empty() {
        return 0.0;
    }
    let (num, den) = match raw.split_once('/') {
        Some((n, d)) => (n, d),
        None => (raw, "1"),
    };
    let num: f64 = num.trim().parse().unwrap_or(0.0);
    let den: f64 = den.trim().parse().unwrap_or(0.0);
    if num <= 0.0 || den <= 0.0 {
        0.0
    } else {
        num / den
    }
}

/// Frame size after scaling to `max_width`, never enlarging. Both dimensions
/// are rounded down to an even number as the rawvideo scaler requires.
pub fn scaled_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let scale = (f64::from(max_width) / f64::from(width)).min(1.0);
    let w = (f64::from(width) * scale) as u32;
    let h = (f64::from(height) * scale) as u32;
    ((w / 2) * 2, (h / 2) * 2)
}

/// Read the video stream's facts. `ffprobe` is the primary source (it covers
/// every container and reports the frame rate); when it is unavailable we
/// fall back to the MP4 moov box, which knows the size and duration but not
/// the rate. `None` when neither can read the file.
pub fn probe(path: &Path) -> Option<VideoStreamFacts> {
    if !path.is_file() {
        return None;
    }
    if ffprobe_available()
        && let Some(facts) = probe_with_ffprobe(path)
    {
        return Some(facts);
    }
    probe_with_mp4(path)
}

/// Probe via the system `ffprobe`.
fn probe_with_ffprobe(path: &Path) -> Option<VideoStreamFacts> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stream = json.get("streams")?.get(0)?;
    let width = stream.get("width")?.as_u64()? as u32;
    let height = stream.get("height")?.as_u64()? as u32;
    if width == 0 || height == 0 {
        return None;
    }
    let rate = stream
        .get("r_frame_rate")
        .and_then(|v| v.as_str())
        .map(parse_frame_rate)
        .unwrap_or(0.0);
    let fps = if rate > 0.0 {
        rate.round().clamp(1.0, 240.0) as u32
    } else {
        FALLBACK_FPS as u32
    };
    let duration_ms = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0).max(0.0) as u64)
        .unwrap_or(0);
    Some(VideoStreamFacts {
        width,
        height,
        fps,
        duration_ms,
    })
}

/// Probe an MP4-family container without any external tool (no frame rate).
fn probe_with_mp4(path: &Path) -> Option<VideoStreamFacts> {
    let facts = crate::media::probe::video_facts(path)?;
    Some(VideoStreamFacts {
        width: facts.width,
        height: facts.height,
        fps: FALLBACK_FPS as u32,
        duration_ms: facts.duration_ms.unwrap_or(0),
    })
}

/// A one-way pipe of raw BGRA frames decoded by ffmpeg.
///
/// Frames are produced as fast as they are read, so the consumer paces the
/// stream: while nobody reads, ffmpeg blocks on a full pipe (bounded memory),
/// and dropping the pipe kills the process.
pub struct FramePipe {
    child: Child,
    reader: BufReader<ChildStdout>,
    width: u32,
    height: u32,
}

impl FramePipe {
    /// Start decoding at `seek_ms` into a stream scaled to `max_width`.
    /// `-ss` before `-i` seeks to the preceding keyframe — fast, and close
    /// enough for a scrub.
    pub fn open(path: &Path, seek_ms: u64, max_width: u32) -> Option<Self> {
        let facts = probe(path)?;
        let (width, height) = scaled_size(facts.width, facts.height, max_width);
        if width == 0 || height == 0 {
            return None;
        }
        let seek = format!("{:.3}", seek_ms as f64 / 1000.0);
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", &seek, "-i"])
            .arg(path)
            .args([
                "-vf",
                &format!("scale={width}:{height}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "-an",
                "-sn",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let frame_bytes = (width as usize) * (height as usize) * 4;
        Some(Self {
            child,
            reader: BufReader::with_capacity(frame_bytes, stdout),
            width,
            height,
        })
    }

    /// Width of the frames this pipe produces.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height of the frames this pipe produces.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Read exactly one frame, or `None` at end of stream (or on any read
    /// error, which for a pipe means the decoder is gone).
    pub fn read_frame(&mut self) -> Option<Vec<u8>> {
        let mut buffer = vec![0u8; (self.width as usize) * (self.height as usize) * 4];
        self.reader.read_exact(&mut buffer).ok()?;
        Some(buffer)
    }
}

impl Drop for FramePipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the file carries at least one audio stream. Probed with
/// `ffprobe`; `false` when it is unavailable or the file has no audio —
/// the player then hides its volume controls instead of faking silence.
pub fn has_audio_track(path: &Path) -> bool {
    let Ok(output) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    output.status.success() && !output.stdout.is_empty()
}

/// Lowest playback speed the player offers; the audio side chains
/// [`atempo_filter`] instances down to this.
pub const MIN_PLAYBACK_SPEED: f32 = 0.25;

/// Highest playback speed the player offers; the audio side chains
/// [`atempo_filter`] instances up to this.
pub const MAX_PLAYBACK_SPEED: f32 = 4.0;

/// The `-af` expression that tempo-scales audio by `speed` while keeping
/// the pitch. A single `atempo` instance only spans 0.5–2.0, so anything
/// outside that is split into a chain (`4×` → `atempo=2,atempo=2.000`);
/// `speed` is clamped to [`MIN_PLAYBACK_SPEED`]–[`MAX_PLAYBACK_SPEED`].
pub fn atempo_filter(speed: f32) -> String {
    let mut remaining = speed.clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
    let mut filters: Vec<String> = Vec::new();
    while remaining > 2.0 {
        filters.push("atempo=2".to_string());
        remaining /= 2.0;
    }
    while remaining < 0.5 {
        filters.push("atempo=0.5".to_string());
        remaining /= 0.5;
    }
    filters.push(format!("atempo={remaining:.3}"));
    filters.join(",")
}

/// A one-way pipe of raw PCM frames decoded by ffmpeg: signed 16-bit LE,
/// 44.1 kHz, interleaved stereo, tempo-adjusted so playback speed changes
/// keep the pitch (ffmpeg's `atempo`, chained by [`atempo_filter`]).
///
/// Same backpressure contract as [`FramePipe`]: the consumer paces the
/// stream, dropping the pipe kills the process.
pub struct AudioPipe {
    child: Child,
    reader: BufReader<ChildStdout>,
}

/// Bytes per ~100 ms chunk: 44100 samples × 2 channels × 2 bytes ÷ 10.
const AUDIO_CHUNK_BYTES: usize = 44100 * 2 * 2 / 10;

impl AudioPipe {
    /// Start decoding audio at `seek_ms`, resampled to 44.1 kHz stereo and
    /// tempo-scaled by `speed` (clamped to the player's range and chained
    /// through as many `atempo` instances as it takes). `None` when ffmpeg
    /// cannot be spawned.
    pub fn open(path: &Path, seek_ms: u64, speed: f32) -> Option<Self> {
        let seek = format!("{:.3}", seek_ms as f64 / 1000.0);
        let tempo = atempo_filter(speed);
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", &seek, "-i"])
            .arg(path)
            .args([
                "-vn",
                "-sn",
                "-map",
                "0:a:0?",
                "-af",
                &tempo,
                "-f",
                "s16le",
                "-acodec",
                "pcm_s16le",
                "-ar",
                "44100",
                "-ac",
                "2",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        Some(Self {
            child,
            reader: BufReader::with_capacity(AUDIO_CHUNK_BYTES, stdout),
        })
    }

    /// Read the next ~100 ms of interleaved stereo samples, or `None` at
    /// end of stream (or when the decoder died).
    pub fn read_chunk(&mut self) -> Option<Vec<u8>> {
        let mut buffer = vec![0u8; AUDIO_CHUNK_BYTES];
        // Short reads only happen at the very end of the stream; a partial
        // chunk is still real audio, so it is returned as-is.
        let mut filled = 0;
        while filled < AUDIO_CHUNK_BYTES {
            let n = self
                .reader
                .read(&mut buffer[filled..AUDIO_CHUNK_BYTES])
                .ok()?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            None
        } else {
            buffer.truncate(filled);
            Some(buffer)
        }
    }
}

impl Drop for AudioPipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PLAYBACK_SPEED, MIN_PLAYBACK_SPEED, atempo_filter};

    #[test]
    fn atempo_chain_covers_the_whole_range() {
        // Inside one instance's 0.5–2.0 the filter stays single.
        assert_eq!(atempo_filter(1.0), "atempo=1.000");
        assert_eq!(atempo_filter(2.0), "atempo=2.000");
        assert_eq!(atempo_filter(0.5), "atempo=0.500");
        // Outside it, instances chain (each within 0.5–2.0).
        assert_eq!(atempo_filter(4.0), "atempo=2,atempo=2.000");
        assert_eq!(atempo_filter(3.0), "atempo=2,atempo=1.500");
        assert_eq!(atempo_filter(0.25), "atempo=0.5,atempo=0.500");
        // Beyond the player's range the value is clamped, not extrapolated.
        assert_eq!(atempo_filter(10.0), atempo_filter(MAX_PLAYBACK_SPEED));
        assert_eq!(atempo_filter(0.05), atempo_filter(MIN_PLAYBACK_SPEED));
    }

    use super::*;

    #[test]
    fn frame_rate_forms_are_parsed() {
        assert!((parse_frame_rate("30000/1001") - 29.97).abs() < 0.01);
        assert!((parse_frame_rate("30/1") - 30.0).abs() < 0.001);
        assert!((parse_frame_rate("24") - 24.0).abs() < 0.001);
        assert_eq!(parse_frame_rate("0/0"), 0.0);
        assert_eq!(parse_frame_rate(""), 0.0);
        assert_eq!(parse_frame_rate("nonsense"), 0.0);
    }

    #[test]
    fn scaling_caps_width_and_keeps_even_dimensions() {
        assert_eq!(scaled_size(1920, 1080, 720), (720, 404));
        assert_eq!(scaled_size(640, 480, 720), (640, 480));
        assert_eq!(scaled_size(0, 100, 720), (0, 0));
        // Odd source dimensions end up even.
        assert_eq!(scaled_size(1281, 721, 1281), (1280, 720));
    }

    #[test]
    fn frame_ms_matches_the_rounded_rate() {
        let facts = VideoStreamFacts {
            width: 640,
            height: 480,
            fps: 25,
            duration_ms: 1000,
        };
        assert_eq!(facts.frame_ms(), 40);
    }

    /// End-to-end: generate a tiny clip *with audio*, then stream PCM out of
    /// the audio pipe. Skipped when ffmpeg/ffprobe are not installed.
    #[test]
    fn audio_pipe_streams_pcm_when_ffmpeg_present() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-video-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip-with-audio.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "failed to generate the test clip");

        assert!(has_audio_track(&clip), "the clip reports an audio track");

        let mut pipe = AudioPipe::open(&clip, 0, 1.0).expect("audio pipe opens");
        let chunk = pipe.read_chunk().expect("first chunk decodes");
        assert!(!chunk.is_empty());
        assert!(chunk.len() <= 44100 * 2 * 2 / 10);

        // The stream ends within a few hundred chunks (1 s of audio).
        let mut count = 1usize;
        while pipe.read_chunk().is_some() {
            count += 1;
            assert!(count < 200, "audio stream never ended");
        }
        assert!(count >= 5, "only {count} chunks of a 1 s stream");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end: the same clip reads fine at 2× tempo (the atempo clamp
    /// stays inside ffmpeg's single-instance range).
    #[test]
    fn audio_pipe_opens_at_extreme_tempo_when_ffmpeg_present() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-video-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip-tempo.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "aac",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "failed to generate the test clip");

        let mut pipe = AudioPipe::open(&clip, 0, 2.0).expect("2× pipe opens");
        assert!(pipe.read_chunk().is_some(), "2× stream yields audio");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end: generate a tiny clip, probe it and read a frame. Skipped
    /// when ffmpeg/ffprobe are not installed.
    #[test]
    fn probe_and_read_frame_when_ffmpeg_present() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-video-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "failed to generate the test clip");

        let facts = probe(&clip).expect("probe succeeds");
        assert_eq!(facts.width, 320);
        assert_eq!(facts.height, 240);
        assert_eq!(facts.fps, 10);
        assert!(facts.duration_ms >= 900, "duration {facts:?}");

        let mut pipe = FramePipe::open(&clip, 0, 320).expect("pipe opens");
        assert_eq!(pipe.width(), 320);
        assert_eq!(pipe.height(), 240);
        let frame = pipe.read_frame().expect("first frame decodes");
        assert_eq!(frame.len(), 320 * 240 * 4);
        // A one second clip at 10 fps yields about ten frames.
        let mut count = 1;
        while pipe.read_frame().is_some() {
            count += 1;
            if count > 40 {
                break;
            }
        }
        assert!((5..=15).contains(&count), "decoded {count} frames");

        std::fs::remove_dir_all(&dir).ok();
    }
}
