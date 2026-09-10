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

#[cfg(test)]
mod tests {
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
