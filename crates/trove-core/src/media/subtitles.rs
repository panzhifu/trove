//! Subtitle export: turn a stored transcript into an SRT file.
//!
//! The recognisers file plain text — the cloud endpoint is asked for
//! `response_format=json` and the local Whisper decodes without timestamp
//! tokens — so no segment timings exist to export. The cue timeline here is
//! therefore an **estimate**: the transcript is split into subtitle-sized
//! cues (sentence punctuation first, then a length cap) and laid across the
//! asset's duration proportionally to each cue's character count. That is
//! the honest fallback for text without timings — good enough for a preview
//! and a rough cut — and it costs nothing at transcription time. Segment
//! timings from the recognisers themselves would be the real fix and can
//! replace this without changing the callers.

use std::path::Path;

use crate::error::Result;

/// Characters one cue carries at most. Subtitle convention keeps a cue to
/// one or two short lines; 60 is a comfortable single line for either CJK
/// (wide glyphs) or Latin text, and splitting near it keeps the pacing
/// readable.
const MAX_CUE_CHARS: usize = 60;

/// Reading speed assumed when the asset's duration is unknown: a listener
/// keeps up with roughly this many characters a second across mixed CJK and
/// Latin text, so the timeline the cues span derives from the transcript's
/// own length.
const ESTIMATED_CHARS_PER_SECOND: f64 = 4.0;

/// One subtitle cue: the window it covers and the line it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Split a transcript into subtitle-sized cues laid across `duration_ms`,
/// proportionally to text length. A missing or zero duration falls back to
/// an estimated one (a fixed reading speed over the transcript's length).
pub fn cues(transcript: &str, duration_ms: Option<u64>) -> Vec<Cue> {
    let texts = split_cues(transcript);
    if texts.is_empty() {
        return Vec::new();
    }
    let duration = match duration_ms {
        Some(ms) if ms > 0 => ms as f64,
        _ => {
            let chars: usize = texts.iter().map(|text| text.chars().count()).sum();
            ((chars as f64 / ESTIMATED_CHARS_PER_SECOND) * 1000.0).max(1000.0)
        }
    };

    // Each cue's share of the timeline is its share of the text. The last
    // cue ends exactly at the duration; every window is kept wide enough
    // to flash on screen (a cue with zero width would never render).
    let weights: Vec<f64> = texts
        .iter()
        .map(|text| text.chars().count().max(1) as f64)
        .collect();
    let total: f64 = weights.iter().sum();
    let mut elapsed = 0.0;
    let last = texts.len() - 1;
    let mut out = Vec::with_capacity(texts.len());
    for (index, (text, weight)) in texts.into_iter().zip(weights).enumerate() {
        let start = elapsed;
        elapsed += weight / total * duration;
        // The last cue closes the timeline exactly; rounding must not
        // leave a sub-millisecond gap at the end.
        let end = if index == last { duration } else { elapsed };
        let start = start.min(duration);
        out.push(Cue {
            start_ms: start.round() as u64,
            end_ms: end.max(start + 50.0).round() as u64,
            text,
        });
    }
    out
}

/// The cue texts: lines, then sentences, then a length cap — most natural
/// break first, hard cut only for a run of text without any punctuation.
fn split_cues(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.lines() {
        for sentence in split_sentences(line.trim()) {
            for piece in split_long(sentence) {
                let piece = piece.trim();
                if !piece.is_empty() {
                    out.push(piece.to_string());
                }
            }
        }
    }
    out
}

/// Split one line into sentences at terminal punctuation, keeping the
/// punctuation with the sentence it ends. Newline-separated transcript
/// chunks (one recogniser window per line) split naturally here.
fn split_sentences(line: &str) -> Vec<String> {
    let enders = ['。', '！', '？', '!', '?', '…', '.'];
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        current.push(ch);
        if enders.contains(&ch) {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current);
    }
    sentences
}

/// Cap a sentence at [`MAX_CUE_CHARS`]: cut at the last soft break inside
/// the cap (clause punctuation, then whitespace), or hard-cut when the run
/// has neither. What remains is processed again, so a 200-character run
/// becomes several cues, not one.
fn split_long(sentence: String) -> Vec<String> {
    let soft = ['，', '、', '；', '：', ',', ';', ':', '—', '-'];
    let mut out: Vec<String> = Vec::new();
    let mut rest = sentence;
    loop {
        let count = rest.chars().count();
        if count <= MAX_CUE_CHARS {
            out.push(rest);
            break;
        }
        let cut = rest
            .char_indices()
            .take(MAX_CUE_CHARS)
            .filter(|&(_, ch)| soft.contains(&ch))
            .map(|(ix, ch)| ix + ch.len_utf8())
            .last()
            .or_else(|| rest.char_indices().nth(MAX_CUE_CHARS).map(|(ix, _)| ix))
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(cut);
        out.push(head.to_string());
        rest = tail.to_string();
    }
    out
}

/// Render cues as an SRT document: a numbered block per cue with a
/// `HH:MM:SS,mmm` window and its line.
pub fn to_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (index, cue) in cues.iter().enumerate() {
        out.push_str(&(index + 1).to_string());
        out.push('\n');
        out.push_str(&format_timestamp(cue.start_ms));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(cue.end_ms));
        out.push('\n');
        out.push_str(&cue.text);
        out.push('\n');
        out.push('\n');
    }
    out
}

/// Milliseconds as SRT's `HH:MM:SS,mmm` — the comma is part of the format,
/// not a locale decimal. Public so the editor can label a cue's window with
/// the same spelling the file uses.
pub fn format_timestamp(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

/// Build the SRT for a transcript and write it to `path` — atomically, the
/// way the XMP sidecar is written: a temp file in the target's own
/// directory, renamed into place only when whole. Returns the cue count
/// (what the toast reports).
pub fn save(path: &Path, transcript: &str, duration_ms: Option<u64>) -> Result<usize> {
    let cues = cues(transcript, duration_ms);
    let count = cues.len();
    write_atomic(path, &to_srt(&cues))?;
    Ok(count)
}

/// Write an already-built cue list as `path`, atomically. The editor's own
/// save path: it edits the `SRT` document directly, so it does not go back
/// through a transcript.
pub fn save_cues(path: &Path, cues: &[Cue]) -> Result<()> {
    write_atomic(path, &to_srt(cues))
}

/// Write an `SRT` document verbatim, atomically. Used by the editor when the
/// user edits the raw text (timings and all) rather than a cue list.
pub fn write_document(path: &Path, srt: &str) -> Result<()> {
    write_atomic(path, srt)
}

/// Write `text` to `path` through a temp file in the target's own directory,
/// renamed into place only when whole, so a reader never sees a half file.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("srt.tmp-{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, text)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error.into())
        }
    }
}

/// Parse an `SRT` document back into cues. Tolerant of the shapes files in
/// the wild carry: a leading BOM, CRLF or bare-CR line endings, a missing
/// index line, and trailing position settings after the timing arrow. Blocks
/// that carry no parsable timing or no text are skipped rather than guessed
/// at — a malformed cue is a cue the user cannot see, and inventing one would
/// put words on screen nobody wrote.
pub fn parse(text: &str) -> Vec<Cue> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    normalized.split("\n\n").filter_map(parse_block).collect()
}

/// One SRT block (index line optional, then `start --> end`, then the text).
fn parse_block(block: &str) -> Option<Cue> {
    let mut lines = block.lines();
    let first = lines.next()?.trim();
    let timing = if first.parse::<u64>().is_ok() {
        lines.next()?.trim()
    } else {
        first
    };
    let (start_ms, end_ms) = parse_timing(timing)?;
    let text = lines.collect::<Vec<_>>().join("\n");
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Cue {
        start_ms,
        end_ms,
        text,
    })
}

/// The `HH:MM:SS,mmm --> HH:MM:SS,mmm` line, ignoring any trailing cue
/// settings (alignment, position) a player may have written.
fn parse_timing(line: &str) -> Option<(u64, u64)> {
    let (start, end) = line.split_once("-->")?;
    Some((parse_timestamp(start.trim())?, parse_timestamp(end.trim())?))
}

/// One `HH:MM:SS,mmm` (or `MM:SS.mmm`) timestamp in milliseconds. The comma
/// is the SRT spelling but a dot is accepted too — both arrive from editors.
fn parse_timestamp(raw: &str) -> Option<u64> {
    // Drop trailing position settings: the timestamp is the first token.
    let token = raw.split_whitespace().next()?;
    let parts: Vec<&str> = token.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => ("0", *m, *s),
        _ => return None,
    };
    let (secs, millis) = match seconds.split_once([',', '.']) {
        Some((s, m)) => (s, m),
        None => (seconds, "0"),
    };
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    let secs: u64 = secs.parse().ok()?;
    // Milliseconds may be written with one to three digits.
    let millis: u64 = match millis.len() {
        0 => 0,
        1 => millis.parse::<u64>().ok()? * 100,
        2 => millis.parse::<u64>().ok()? * 10,
        _ => millis[..3].parse().ok()?,
    };
    Some(hours * 3_600_000 + minutes * 60_000 + secs * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CJK and Latin sentences split at their punctuation, empty lines
    /// vanish, and every cue stays within the cap.
    #[test]
    fn cues_split_on_sentences_and_cap_length() {
        let transcript = "今天天气很好。我们去公园散步，顺便买了咖啡！\nIt was a long day, but a good one. Time to rest.\n\n\n";
        let texts = split_cues(transcript);
        assert_eq!(
            texts,
            vec![
                "今天天气很好。",
                "我们去公园散步，顺便买了咖啡！",
                "It was a long day, but a good one.",
                "Time to rest.",
            ]
        );
        for text in &texts {
            assert!(text.chars().count() <= MAX_CUE_CHARS);
        }
    }

    /// A run without any punctuation hard-cuts at the cap, and every piece
    /// stays inside it.
    #[test]
    fn unpunctuated_text_hard_cuts() {
        let text = "字".repeat(150);
        let texts = split_cues(&text);
        assert_eq!(texts.len(), 3);
        assert!(texts.iter().all(|t| t.chars().count() <= MAX_CUE_CHARS));
        assert_eq!(texts.concat().chars().count(), 150, "no text lost");
    }

    /// The timeline covers the duration exactly: monotonic windows, the
    /// first cue starts at zero, the last ends at the duration.
    #[test]
    fn timeline_spans_the_duration() {
        let duration = 60_000;
        let cues = cues(
            "第一句。第二句，比较长一些一些一些一些。Third and last!",
            Some(duration),
        );
        assert!(cues.len() >= 3);
        assert_eq!(cues.first().unwrap().start_ms, 0);
        assert_eq!(cues.last().unwrap().end_ms, duration);
        for pair in cues.windows(2) {
            assert!(pair[1].start_ms >= pair[0].end_ms - 1, "monotonic");
            assert!(pair[0].end_ms > pair[0].start_ms, "every cue is visible");
        }
    }

    /// No duration: the timeline derives from a fixed reading speed over
    /// the text, so the file still carries a usable timeline.
    #[test]
    fn missing_duration_falls_back_to_a_reading_speed_estimate() {
        let text = "一。二。三。四。";
        let cues = cues(text, None);
        assert_eq!(cues.len(), 4);
        let span = cues.last().unwrap().end_ms;
        assert!(span >= 1000, "a short transcript still gets a timeline");
        let expected = (8.0 / ESTIMATED_CHARS_PER_SECOND * 1000.0).round() as u64;
        assert_eq!(span, expected);
    }

    /// The rendered SRT is the numbered blocks players expect, with the
    /// comma milliseconds the format asks for.
    #[test]
    fn srt_rendering_matches_the_exchange_format() {
        let cues = vec![Cue {
            start_ms: 3_661_234,
            end_ms: 3_664_567,
            text: "你好。".into(),
        }];
        assert_eq!(
            to_srt(&cues),
            "1\n01:01:01,234 --> 01:01:04,567\n你好。\n\n"
        );
    }

    /// Empty input yields no cues and an empty document — the save path
    /// guards against writing one, but the builder itself stays total.
    #[test]
    fn empty_transcript_yields_nothing() {
        assert!(cues("   \n  ", Some(1_000)).is_empty());
        assert_eq!(to_srt(&[]), "");
    }

    /// Rendering then parsing is the identity: the editor round-trips a file
    /// it wrote itself without losing or reshaping a cue.
    #[test]
    fn parse_round_trips_to_srt() {
        let cues = vec![
            Cue {
                start_ms: 0,
                end_ms: 9_219,
                text: "We were both young.".into(),
            },
            Cue {
                start_ms: 9_219,
                end_ms: 18_284,
                text: "两句\n换行也保留。".into(),
            },
        ];
        assert_eq!(parse(&to_srt(&cues)), cues);
    }

    /// Files in the wild carry a BOM, CRLF endings, and sometimes no index
    /// line; all three still parse to the same cues.
    #[test]
    fn parse_tolerates_bom_crlf_and_missing_index() {
        let text = "\u{feff}1\r\n00:00:00,000 --> 00:00:01,500\r\n第一句\r\n\r\n\
                    00:00:01,500 --> 00:00:03,000\r\n第二句\r\n";
        let cues = parse(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "第一句");
        assert_eq!(cues[1].start_ms, 1_500);
    }

    /// A block with no timing or no text is skipped, not guessed at.
    #[test]
    fn parse_skips_malformed_blocks() {
        let text = "1\nnot a timing line\nwords\n\n\
                    2\n00:00:05,000 --> 00:00:06,000\n\n\
                    3\n00:00:07,000 --> 00:00:08,000\nkept\n";
        let cues = parse(text);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 7_000);
        assert_eq!(cues[0].text, "kept");
    }

    /// The timestamp reader accepts the SRT comma, a dot, and trailing cue
    /// settings; and rejects an unparsable token.
    #[test]
    fn parse_timestamp_accepts_the_spellings_editors_write() {
        assert_eq!(parse_timestamp("01:02:03,004"), Some(3_723_004));
        assert_eq!(parse_timestamp("01:02:03.004"), Some(3_723_004));
        assert_eq!(parse_timestamp("00:05,5"), Some(5_500));
        assert_eq!(parse_timestamp("00:00:01,000 X1:100 X2:200"), Some(1_000));
        assert_eq!(parse_timestamp("garbage"), None);
    }
}
