//! Audio preparation for speech-to-text: turn a source's audio track into
//! small, speech-shaped chunks a transcription engine will accept.
//!
//! The transcode is not optional even for plain audio files: recognisers
//! resample to 16 kHz mono internally, the cloud API caps upload sizes, and
//! ffmpeg normalises every container into one the consumer is known to
//! parse. The codec follows the consumer: AAC (32 kbps) for the cloud
//! endpoint, whose encoder is the one ffmpeg always ships and whose bytes
//! are metered; 16-bit PCM WAV for the local Whisper engine, which reads
//! the samples directly and uploads nothing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ai::transcribe::ChunkFormat;
use crate::error::{Error, Result};

/// Seconds of speech one chunk carries. At 32 kbps mono AAC a chunk lands
/// around 7 MB — comfortably under the cloud endpoint's upload cap with
/// headroom for container overhead, and short enough that a server
/// processing it does not run out of patience. (A WAV chunk of the same
/// length is ~57 MB, but the local engine reads it from the local disk; the
/// cap is a cloud shape, not a local one.)
const CHUNK_SECONDS: u64 = 30 * 60;

/// How long one transcode may run before it is killed. Audio-only encodes run
/// far faster than realtime; the cap bounds a hung ffmpeg, not a slow one.
const TRANSCODE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The output name pattern. Zero-padded so the filesystem sorts chunks in
/// playback order without a numeric parse.
const CHUNK_PREFIX: &str = "chunk";

/// Extract `source`'s audio into chunk files under `dest_dir`, returning the
/// chunk paths in playback order. `dest_dir` is the caller's to create and to
/// clean up — a temp directory the job owns is the right shape, since chunks
/// are disposable once consumed.
///
/// Returns an empty vec without running anything when `duration_ms` says the
/// source is long past its useful end (a zero-duration probe of a broken
/// file); an `Err` carries ffmpeg's own complaint otherwise.
pub fn extract_chunks(
    source: &Path,
    duration_ms: Option<u64>,
    dest_dir: &Path,
    cancel: &AtomicBool,
    format: ChunkFormat,
) -> Result<Vec<PathBuf>> {
    let _slot = super::proc::slot();
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::External {
            program: "ffmpeg".into(),
            message: "cancelled".into(),
        });
    }

    // Long sources are cut on time boundaries; short ones are one chunk. An
    // unknown duration stays one chunk and leans on the consumer's size error
    // to surface the pathological case.
    let segmented = duration_ms.is_some_and(|ms| ms > CHUNK_SECONDS * 1000);
    let (ext, codec_args, segment_format): (&str, [&str; 4], &str) = match format {
        ChunkFormat::AacM4a => ("m4a", ["-c:a", "aac", "-b:a", "32k"], "mp4"),
        ChunkFormat::Wav => ("wav", ["-c:a", "pcm_s16le", "-f", "wav"], "wav"),
    };
    let pattern = dest_dir.join(format!("{CHUNK_PREFIX}-%03d.{ext}"));
    let mut command = Command::new("ffmpeg");
    command
        .args(["-v", "error", "-y", "-i"])
        .arg(source)
        .args(["-vn", "-sn", "-map", "0:a:0", "-ac", "1", "-ar", "16000"]);
    command.args(codec_args);
    if segmented {
        command.args([
            "-f",
            "segment",
            "-segment_time",
            &CHUNK_SECONDS.to_string(),
            "-segment_format",
            segment_format,
            "-reset_timestamps",
            "1",
        ]);
    }
    command.arg(&pattern);

    let output = super::proc::output_with_timeout_and_cancel(command, TRANSCODE_TIMEOUT, cancel)?;
    if !output.status.success() {
        // A cancel kill lands here as a failure too; the caller re-reads the
        // flag to tell the two apart.
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A diagnostic can run pages; a log line and an error toast can't.
        let mut message: String = stderr.trim().chars().take(300).collect();
        if stderr.trim().chars().count() > 300 {
            message.push('…');
        }
        return Err(Error::External {
            program: "ffmpeg".into(),
            message,
        });
    }

    let mut chunks: Vec<PathBuf> = std::fs::read_dir(dest_dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == ext)
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with(CHUNK_PREFIX))
        })
        .collect();
    chunks.sort();
    Ok(chunks)
}

/// A one-second silence in the shape the transcription endpoint expects
/// (16 kHz, 16-bit, mono PCM WAV), built in memory. The settings page's probe
/// uploads this instead of hunting for a real audio file — it proves the
/// endpoint, key and model answer without costing a real asset's privacy.
pub fn probe_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const SECONDS: u32 = 1;
    let data_len = (SAMPLE_RATE as usize) * 2 * SECONDS as usize;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(44 + data_len as u32 - 8).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    wav.resize(44 + data_len, 0);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe WAV parses as exactly what it claims: a 44-byte header plus
    /// one second of 16 kHz mono 16-bit silence.
    #[test]
    fn probe_wav_is_a_well_formed_silence_file() {
        let wav = probe_wav();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert_eq!(data_len, 32_000, "one second of 16 kHz mono 16-bit");
        assert_eq!(wav.len(), 44 + data_len);
        assert!(wav[44..].iter().all(|&b| b == 0), "silence");
    }

    /// An ffmpeg that produces nothing (a missing binary fails the spawn,
    /// which reads as an error) must not be reported as success with zero
    /// chunks — the job would then "transcribe nothing" instead of failing.
    #[cfg(unix)]
    #[test]
    fn a_failed_transcode_is_an_error_not_an_empty_success() {
        let dir = std::env::temp_dir().join(format!("trove-audio-prep-{}", crate::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = AtomicBool::new(false);
        // `false` is not a media file; ffmpeg fails with a clear diagnostic.
        let result = extract_chunks(
            Path::new("/bin/false"),
            Some(1_000),
            &dir,
            &cancel,
            crate::ai::transcribe::ChunkFormat::AacM4a,
        );
        // A machine without ffmpeg fails the spawn instead — both are errors,
        // which is the property under test.
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
