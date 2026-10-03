//! Export: write asset files out of the library into a user-chosen folder.
//!
//! Images ride the convert pipeline ([`super::convert`]) when a raster target
//! is chosen, or are copied verbatim otherwise; videos are transcoded or
//! remuxed by the system `ffmpeg` — the same optional, never-linked runtime
//! dependency the preview player uses (see [`super::video`]). Every other
//! kind is a plain copy: an export is a hand-off, not a rewrite.
//!
//! Subprocess discipline follows [`super::proc`]: each ffmpeg call takes a
//! process slot, runs under a generous per-item timeout (a transcode
//! legitimately outlives the import pipeline's 60-second cap), and dies
//! promptly when the caller's cancel flag flips. Output files are written to
//! a temp name and renamed, so a killed encode never leaves a half file
//! behind under its final name.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::proc;

/// How long one export subprocess may run before it is killed. Far wider
/// than [`proc::PROC_TIMEOUT`]: this bounds a *hung* encoder, not a slow
/// one — a feature-length transcode can honestly take an hour.
const EXPORT_PROC_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// The video targets export offers, in dialog order. `Original` copies the
/// file untouched (no ffmpeg); `RemuxMp4` changes only the container
/// (`-c copy`) — seconds instead of minutes, no quality loss; the rest
/// re-encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoExportFormat {
    Original,
    RemuxMp4,
    Mp4H264,
    Mp4H265,
    MkvH264,
    WebmVp9,
}

/// The video formats, in dialog order.
pub const VIDEO_EXPORT_FORMATS: [VideoExportFormat; 6] = [
    VideoExportFormat::Original,
    VideoExportFormat::RemuxMp4,
    VideoExportFormat::Mp4H264,
    VideoExportFormat::Mp4H265,
    VideoExportFormat::MkvH264,
    VideoExportFormat::WebmVp9,
];

impl VideoExportFormat {
    /// Output extension; `None` for `Original`, which keeps the source's.
    pub fn ext(self) -> Option<&'static str> {
        match self {
            VideoExportFormat::Original => None,
            VideoExportFormat::RemuxMp4 => Some("mp4"),
            VideoExportFormat::Mp4H264 => Some("mp4"),
            VideoExportFormat::Mp4H265 => Some("mp4"),
            VideoExportFormat::MkvH264 => Some("mkv"),
            VideoExportFormat::WebmVp9 => Some("webm"),
        }
    }

    /// Whether ffmpeg must be on PATH for this target.
    pub fn needs_ffmpeg(self) -> bool {
        !matches!(self, VideoExportFormat::Original)
    }

    /// Whether this target re-encodes (quality and the height cap apply).
    /// A remux's whole point is to not.
    pub fn reencodes(self) -> bool {
        matches!(
            self,
            VideoExportFormat::Mp4H264
                | VideoExportFormat::Mp4H265
                | VideoExportFormat::MkvH264
                | VideoExportFormat::WebmVp9
        )
    }

    /// Parse the stable id used by the dialog.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "original" => Some(VideoExportFormat::Original),
            "remux-mp4" => Some(VideoExportFormat::RemuxMp4),
            "mp4-h264" => Some(VideoExportFormat::Mp4H264),
            "mp4-h265" => Some(VideoExportFormat::Mp4H265),
            "mkv-h264" => Some(VideoExportFormat::MkvH264),
            "webm-vp9" => Some(VideoExportFormat::WebmVp9),
            _ => None,
        }
    }

    /// Stable id (inverse of [`VideoExportFormat::parse`]).
    pub fn id(self) -> &'static str {
        match self {
            VideoExportFormat::Original => "original",
            VideoExportFormat::RemuxMp4 => "remux-mp4",
            VideoExportFormat::Mp4H264 => "mp4-h264",
            VideoExportFormat::Mp4H265 => "mp4-h265",
            VideoExportFormat::MkvH264 => "mkv-h264",
            VideoExportFormat::WebmVp9 => "webm-vp9",
        }
    }
}

/// Encode quality for the re-encoding targets, in dialog order. Each level
/// maps to a CRF per codec — the values an encoder's own scale calls
/// "visually lossless" / "default" / "small".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoQuality {
    High,
    #[default]
    Balanced,
    Compact,
}

/// The quality levels, in dialog order.
pub const VIDEO_QUALITIES: [VideoQuality; 3] = [
    VideoQuality::High,
    VideoQuality::Balanced,
    VideoQuality::Compact,
];

impl VideoQuality {
    /// Constant-rate factor for `format`. Lower is better; each codec's
    /// scale differs, so the same level is three different numbers.
    pub fn crf(self, format: VideoExportFormat) -> u8 {
        match (format, self) {
            (VideoExportFormat::Mp4H264, VideoQuality::High) => 18,
            (VideoExportFormat::Mp4H264, VideoQuality::Balanced) => 23,
            (VideoExportFormat::Mp4H264, VideoQuality::Compact) => 28,
            (VideoExportFormat::Mp4H265, VideoQuality::High) => 20,
            (VideoExportFormat::Mp4H265, VideoQuality::Balanced) => 25,
            (VideoExportFormat::Mp4H265, VideoQuality::Compact) => 30,
            (VideoExportFormat::WebmVp9, VideoQuality::High) => 29,
            (VideoExportFormat::WebmVp9, VideoQuality::Balanced) => 33,
            (VideoExportFormat::WebmVp9, VideoQuality::Compact) => 37,
            // A remux takes no quality input; the caller never asks.
            _ => 23,
        }
    }

    /// Parse the stable id used by the dialog.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "high" => Some(VideoQuality::High),
            "balanced" => Some(VideoQuality::Balanced),
            "compact" => Some(VideoQuality::Compact),
            _ => None,
        }
    }

    /// Stable id (inverse of [`VideoQuality::parse`]).
    pub fn id(self) -> &'static str {
        match self {
            VideoQuality::High => "high",
            VideoQuality::Balanced => "balanced",
            VideoQuality::Compact => "compact",
        }
    }
}

/// Height caps offered by the dialog, in dialog order. `None` keeps the
/// original size; the numbers never enlarge (see [`export_video`]).
pub const VIDEO_MAX_HEIGHTS: [Option<u32>; 4] = [None, Some(2160), Some(1080), Some(720)];

/// The ffmpeg invocation for one video target. Exposed so tests can assert
/// the exact command shape without running an encoder. The comma inside the
/// scale filter's `min(ih,…)` is escaped the way ffmpeg's filtergraph parser
/// requires when the filter is one argv element.
pub fn ffmpeg_args(
    format: VideoExportFormat,
    quality: VideoQuality,
    max_height: Option<u32>,
    source: &Path,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"]
        .iter()
        .map(|a| a.to_string())
        .collect();
    args.push(source.display().to_string());
    match format {
        VideoExportFormat::Original => {
            // The caller routes Original to a file copy; reaching this means
            // a caller bug, and the safest reply is the copy-equivalent
            // stream clone rather than a re-encode.
            args.push("-c".into());
            args.push("copy".into());
        }
        VideoExportFormat::RemuxMp4 => {
            args.extend([
                "-c".into(),
                "copy".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        }
        VideoExportFormat::Mp4H264 => {
            args.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "medium".into(),
                "-crf".into(),
                quality.crf(format).to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
            ]);
            push_height_cap(&mut args, max_height);
            args.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        }
        VideoExportFormat::Mp4H265 => {
            args.extend([
                "-c:v".into(),
                "libx265".into(),
                "-preset".into(),
                "medium".into(),
                "-crf".into(),
                quality.crf(format).to_string(),
                "-tag:v".into(),
                "hvc1".into(),
                "-pix_fmt".into(),
                "yuv420p".into(),
            ]);
            push_height_cap(&mut args, max_height);
            args.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
                "-movflags".into(),
                "+faststart".into(),
            ]);
        }
        VideoExportFormat::MkvH264 => {
            args.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "medium".into(),
                "-crf".into(),
                quality.crf(format).to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
            ]);
            push_height_cap(&mut args, max_height);
            args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into()]);
        }
        VideoExportFormat::WebmVp9 => {
            args.extend([
                "-c:v".into(),
                "libvpx-vp9".into(),
                "-crf".into(),
                quality.crf(format).to_string(),
                "-b:v".into(),
                "0".into(),
                "-row-mt".into(),
                "1".into(),
            ]);
            push_height_cap(&mut args, max_height);
            args.extend([
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                "128k".into(),
            ]);
        }
    }
    args.push(out.display().to_string());
    args
}

/// Append the never-enlarging height cap. `min(ih, cap)` in the scale
/// filter keeps smaller videos untouched; `-2` keeps the width even, which
/// the pixel formats the encoders output require.
fn push_height_cap(args: &mut Vec<String>, max_height: Option<u32>) {
    if let Some(cap) = max_height {
        args.push("-vf".into());
        args.push(format!("scale=-2:min(ih\\,{cap})"));
    }
}

/// Transcode or remux one video into `out` through the system ffmpeg. The
/// write lands on a temp name and is renamed, so a failed or killed encode
/// never leaves a half file under its final name. Fails with the tail of
/// ffmpeg's stderr, which is the one human-readable thing it prints.
pub fn export_video(
    source: &Path,
    out: &Path,
    format: VideoExportFormat,
    quality: VideoQuality,
    max_height: Option<u32>,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let ext = format
        .ext()
        .unwrap_or(source.extension().and_then(|e| e.to_str()).unwrap_or("bin"));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = out.with_extension(format!("tmp.{ext}"));
    let run = || -> Result<(), String> {
        let _slot = proc::slot();
        let mut command = Command::new("ffmpeg");
        for arg in ffmpeg_args(format, quality, max_height, source, &tmp) {
            command.arg(arg);
        }
        let output = proc::output_with_timeout_and_cancel(command, EXPORT_PROC_TIMEOUT, cancel)
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        Err(stderr_tail(&output.stderr))
    };
    match run() {
        Ok(()) => {
            std::fs::rename(&tmp, out).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })?;
            Ok(out.to_path_buf())
        }
        Err(reason) => {
            let _ = std::fs::remove_file(&tmp);
            Err(reason)
        }
    }
}

/// Copy one file to `out`, same temp-then-rename discipline as every other
/// export step: a cancelled or half-finished copy must not read as a
/// complete file under its final name.
pub fn copy_file(source: &Path, out: &Path) -> Result<PathBuf, String> {
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = out.with_extension(format!("tmp.{ext}"));
    match std::fs::copy(source, &tmp) {
        Ok(_) => {
            std::fs::rename(&tmp, out).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })?;
            Ok(out.to_path_buf())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.to_string())
        }
    }
}

/// The last line or two of ffmpeg's stderr — the "Error …" that says why an
/// encode refused. Capped so a pathological encoder cannot hand the UI a
/// novel.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    let tail: String = trimmed
        .lines()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" · ");
    let out: String = tail.chars().take(300).collect();
    if out.is_empty() {
        "ffmpeg failed".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::video;

    #[test]
    fn format_metadata_is_consistent() {
        for format in VIDEO_EXPORT_FORMATS {
            assert_eq!(
                VideoExportFormat::parse(format.id()),
                Some(format),
                "{} does not round-trip",
                format.id()
            );
            assert_eq!(
                format.needs_ffmpeg(),
                !matches!(format, VideoExportFormat::Original),
            );
            assert_eq!(
                format.reencodes(),
                matches!(
                    format,
                    VideoExportFormat::Mp4H264
                        | VideoExportFormat::Mp4H265
                        | VideoExportFormat::MkvH264
                        | VideoExportFormat::WebmVp9
                ),
            );
        }
        assert_eq!(VideoExportFormat::parse("nope"), None);
        for quality in VIDEO_QUALITIES {
            assert_eq!(VideoQuality::parse(quality.id()), Some(quality));
        }
    }

    /// The two no-encode targets must not grow encoder arguments: a remux
    /// that silently re-encodes would take minutes and lose quality, the
    /// exact opposite of its promise.
    #[test]
    fn a_remux_never_carries_encoder_arguments() {
        let args = ffmpeg_args(
            VideoExportFormat::RemuxMp4,
            VideoQuality::High,
            Some(1080),
            Path::new("in.mov"),
            Path::new("out.mp4"),
        );
        let joined = args.join(" ");
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"copy".to_string()));
        assert!(!joined.contains("libx264"), "{joined}");
        assert!(!joined.contains("crf"), "{joined}");
        assert!(!joined.contains("scale"), "{joined}");
        assert!(joined.contains("faststart"), "{joined}");
    }

    #[test]
    fn transcode_arguments_carry_codec_quality_and_cap() {
        let args = ffmpeg_args(
            VideoExportFormat::Mp4H264,
            VideoQuality::Balanced,
            Some(720),
            Path::new("in.mov"),
            Path::new("out.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("libx264"), "{joined}");
        assert!(joined.contains("-crf 23"), "{joined}");
        assert!(joined.contains("scale=-2:min(ih\\,720)"), "{joined}");
        assert!(joined.contains("yuv420p"), "{joined}");
        assert!(joined.contains("-b:a 192k"), "{joined}");
        // Input and output are the argv around the options, in that order.
        assert_eq!(args[5], "in.mov");
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    #[test]
    fn crf_follows_each_codec_scale() {
        assert_eq!(VideoQuality::High.crf(VideoExportFormat::Mp4H264), 18);
        assert_eq!(VideoQuality::Compact.crf(VideoExportFormat::Mp4H264), 28);
        assert_eq!(VideoQuality::High.crf(VideoExportFormat::Mp4H265), 20);
        assert_eq!(VideoQuality::Compact.crf(VideoExportFormat::WebmVp9), 37);
    }

    /// Without a cap no scale filter appears — the export must never
    /// silently resample a video the user asked to keep as is.
    #[test]
    fn no_cap_means_no_scale_filter() {
        let args = ffmpeg_args(
            VideoExportFormat::MkvH264,
            VideoQuality::Compact,
            None,
            Path::new("in.mp4"),
            Path::new("out.mkv"),
        );
        assert!(
            !args.iter().any(|a| a.contains("scale")),
            "{:?}",
            args.join(" ")
        );
    }

    /// A copy is byte-exact and leaves no temp behind under either name.
    #[test]
    fn a_copied_file_is_byte_exact_and_atomic() {
        let dir = std::env::temp_dir().join(format!("trove-export-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("src.dat");
        std::fs::write(&source, [0u8, 1, 2, 3, 250, 251]).unwrap();
        let out = copy_file(&source, &dir.join("dest.dat")).unwrap();
        assert_eq!(out, dir.join("dest.dat"));
        assert_eq!(
            std::fs::read(&out).unwrap(),
            vec![0u8, 1, 2, 3, 250, 251],
            "the copy is not byte-exact"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("dest.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a temp file survived: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real encoder path: a one-second clip is remuxed and transcoded
    /// without ffmpeg knowing anything unusual happened. Skipped where
    /// ffmpeg is not installed — the feature degrades gracefully there too.
    #[test]
    fn ffmpeg_remuxes_and_transcodes_a_real_clip() {
        if !video::ffmpeg_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-export-ffmpeg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        let source = dir.join("src.mp4");
        let make = || -> Command {
            let mut c = Command::new("ffmpeg");
            c.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=128x96:rate=15",
                "-pix_fmt",
                "yuv420p",
                source.to_str().unwrap(),
            ]);
            c
        };
        let output = proc::output_with_timeout(make()).unwrap();
        assert!(output.status.success(), "ffmpeg could not make a test clip");

        let remuxed = export_video(
            &source,
            &dir.join("remux.mp4"),
            VideoExportFormat::RemuxMp4,
            VideoQuality::Balanced,
            None,
            &cancel,
        )
        .expect("remux succeeds");
        assert!(remuxed.is_file());

        let transcoded = export_video(
            &source,
            &dir.join("small.mp4"),
            VideoExportFormat::Mp4H264,
            VideoQuality::Compact,
            Some(64),
            &cancel,
        )
        .expect("transcode succeeds");
        let facts = video::probe(&transcoded).expect("the output probes");
        assert!(
            facts.height <= 64,
            "the height cap was ignored: {}",
            facts.height
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A garbage input is a per-item failure carrying ffmpeg's own words,
    /// not a panic and not a silent success.
    #[test]
    fn a_bad_source_fails_with_ffmpeg_stderr() {
        if !video::ffmpeg_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("trove-export-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        let source = dir.join("garbage.mp4");
        std::fs::write(&source, b"definitely not a video").unwrap();
        let error = export_video(
            &source,
            &dir.join("out.mp4"),
            VideoExportFormat::Mp4H264,
            VideoQuality::Balanced,
            None,
            &cancel,
        )
        .expect_err("garbage must fail");
        assert!(!error.is_empty(), "{error}");
        assert!(
            !dir.join("out.mp4").exists(),
            "no output under the final name"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
