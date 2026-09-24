//! Reading a text file: one extension list, one encoding ladder, one size cap.
//!
//! Three decisions worth stating, because each one was arrived at the hard way
//! somewhere else:
//!
//! * **One extension list.** The same drift that made `is_video_ext` and
//!   `is_audio_ext` single lists applies here — a viewer gate, a card gate and a
//!   "what can I open with" gate that disagree produce a file that previews in
//!   one place and not another.
//! * **Detect, don't assume.** A `.txt` written by a Windows editor in 2003 is
//!   GBK, not UTF-8, so the ladder below is the one ICU uses: byte-order mark,
//!   then the UTF-16 NUL-parity tell, then the binary test, then strict UTF-8,
//!   then a statistical guess over legacy code pages. A wrong guess shows up
//!   immediately as mojibake, so the *order* matters more than the cleverness.
//! * **Cap the read, and say so.** One MiB is the viewer's budget; a log file
//!   that big is not something anyone reads linearly. Truncation is reported
//!   rather than hidden, because a truncated buffer written back over the source
//!   is data loss.

use std::io::Read as _;
use std::path::Path;

/// How much of a text file the viewer loads, in bytes.
pub const MAX_VIEW_BYTES: usize = 1024 * 1024;

/// How many characters a card shows. Enough to recognise a file by its opening
/// lines, few enough that the card is a picture and not a page.
pub const CARD_SNIPPET_CHARS: usize = 360;

/// Bytes the encoding ladder sniffs: a BOM, a UTF-16 parity sample, and enough
/// NUL counting to call a file binary.
const SNIFF_BYTES: usize = 4096;

/// Bytes handed to the statistical guess: 64 KiB, where a legacy-codepage guess
/// has enough evidence and stops getting better from more of it.
const DETECT_BYTES: usize = 512 * 128;

/// Whether this extension is text the app can show.
///
/// Deliberately excludes anything Trove renders as a picture (`.svg`) or treats
/// as a document it cannot read (`.pdf`, `.doc`): those have their own paths,
/// and a list that claimed them would show a wall of mojibake.
pub fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        // Plain text and markup
        "txt" | "text" | "log" | "md" | "markdown" | "mdx" | "rst" | "tex" | "adoc"
        // Data and configuration
        | "json" | "jsonc" | "json5" | "csv" | "tsv" | "xml" | "html" | "htm" | "yaml" | "yml"
        | "toml" | "ini" | "cfg" | "conf" | "properties" | "plist" | "xmp" | "env"
        // Styles, scripts, sources
        | "css" | "scss" | "sass" | "less" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "vue"
        | "svelte" | "php" | "py" | "rb" | "pl" | "pm" | "lua" | "r" | "jl" | "dart" | "go"
        | "zig" | "rs" | "java" | "kt" | "kts" | "swift" | "scala" | "c" | "h" | "cc" | "cpp"
        | "cxx" | "hpp" | "hh" | "cs" | "glsl" | "hlsl" | "wgsl" | "vert" | "frag" | "sql"
        | "proto" | "graphql" | "sh" | "bash" | "zsh" | "fish" | "bat" | "cmd" | "ps1"
        | "cmake" | "mk" | "make" | "diff" | "patch" | "po" | "pot"
        // Timed text and contacts, which are text and are searched as such
        | "srt" | "vtt" | "ass" | "sub" | "ics" | "vcf"
        // Dotfile names that arrive as an extension when they are the whole name
        | "editorconfig" | "gitignore" | "gitattributes" | "dockerignore"
    )
}

/// The media type to record for a text file: the structured ones get their own,
/// the rest of the family is `text/plain`.
pub fn text_mime(ext: &str) -> &'static str {
    match ext {
        "json" | "jsonc" | "json5" => "application/json",
        "html" | "htm" => "text/html",
        "xml" | "plist" | "xmp" => "text/xml",
        "css" | "scss" | "sass" | "less" => "text/css",
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => "text/javascript",
        "yaml" | "yml" => "text/yaml",
        "csv" => "text/csv",
        "md" | "markdown" | "mdx" => "text/markdown",
        _ => "text/plain",
    }
}

/// What a text read produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextContent {
    /// The decoded characters, up to the caller's byte cap.
    pub text: String,
    /// The encoding that was actually used, as a label for the inspector.
    pub encoding: &'static str,
    /// Whether the file is bigger than the cap, so this is only its beginning.
    /// A caller that can write the file back must refuse while this is set.
    pub truncated: bool,
    /// Bytes read off disk.
    pub bytes_read: usize,
    /// Total bytes of the file, when they could be had.
    pub total_bytes: u64,
    /// Lines in [`text`](Self::text).
    pub line_count: usize,
    /// Whether this looks like a binary file that merely has a text extension.
    pub binary: bool,
}

impl TextContent {
    /// A short opening excerpt, for a card: one run of text with every break
    /// flattened, so the card stays a block rather than a page.
    pub fn snippet(&self) -> String {
        let chars = self.text.chars().count();
        let mut out: String = self
            .text
            .chars()
            .take(CARD_SNIPPET_CHARS)
            .map(|c| {
                // Every control character (tab and the breaks among them) and
                // the two Unicode line separators flatten to a space.
                if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if chars > CARD_SNIPPET_CHARS {
            out.push('…');
        }
        out
    }
}

/// Read at most `max_bytes` of `path` as text.
///
/// `None` only when the file cannot be opened or read at all. A binary file or
/// an undecodable one still answers — with [`TextContent::binary`] set, or with
/// text that is merely wrong — so the caller can say *why* instead of showing
/// nothing.
pub fn read(path: &Path, max_bytes: usize) -> Option<TextContent> {
    let file = std::fs::File::open(path).ok()?;
    let total_bytes = file.metadata().ok()?.len();
    // One byte past the cap is all it costs to know whether we hit it.
    let want = (max_bytes as u64 + 1).min(total_bytes) as usize;
    let mut buffer = Vec::with_capacity(want.min(MAX_VIEW_BYTES + 1));
    std::io::BufReader::new(file)
        .take(want as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    let bytes_read = buffer.len();
    // The extra byte was only ever a question: was there more? What the caller
    // gets is at most `max_bytes` of it, cut on a byte boundary *before*
    // decoding, so a multi-byte character cannot be split in half.
    let shown = bytes_read.min(max_bytes);
    let truncated = total_bytes as usize > max_bytes;
    Some(decode(&buffer[..shown], truncated, shown, total_bytes))
}

/// The reading convenience every viewer caller wants: the viewer's cap.
pub fn read_for_viewer(path: &Path) -> Option<TextContent> {
    read(path, MAX_VIEW_BYTES)
}

/// Run a buffer through the ladder: BOM, UTF-16 parity, binary test, strict
/// UTF-8, then a statistical guess over legacy code pages.
pub fn decode(bytes: &[u8], truncated: bool, bytes_read: usize, total_bytes: u64) -> TextContent {
    let finish = |text: String, encoding: &'static str, binary: bool| TextContent {
        line_count: line_count(&text),
        text,
        encoding,
        binary,
        truncated,
        bytes_read,
        total_bytes,
    };
    if bytes.is_empty() {
        return finish(String::new(), "utf-8", false);
    }
    // 1. A byte-order mark is the file telling us what it is.
    if let Some((width, label)) = bom(bytes) {
        let body = &bytes[width..];
        let text = match label {
            "utf-16le" => decode_utf16(body, true),
            "utf-16be" => decode_utf16(body, false),
            other => {
                let _ = other;
                String::from_utf8_lossy(body).into_owned()
            }
        };
        return finish(text, label, false);
    }
    // 2. No mark: the NUL pattern still says UTF-16, because ASCII-range text in
    // UTF-16 alternates real bytes with zero bytes in a way no other encoding
    // does.
    if let Some(little) = utf16_by_parity(bytes) {
        let label = if little { "utf-16le" } else { "utf-16be" };
        return finish(decode_utf16(bytes, little), label, false);
    }
    // 3. Scattered NULs mean this is not text at all, whatever it is called.
    // UTF-32 lands here too: its marks are excluded above precisely because it
    // is not supported, and half of every UTF-32 character is a NUL.
    if looks_binary(bytes) {
        return finish(String::new(), "utf-8", true);
    }
    // 4. Strict UTF-8, which a modern file almost always is. A cap that landed
    // mid-character must not demote a UTF-8 file to "legacy guess", so the
    // incomplete tail is trimmed rather than the whole buffer rejected.
    if let Ok(text) = std::str::from_utf8(trim_incomplete_utf8(bytes)) {
        return finish(text.to_string(), "utf-8", false);
    }
    // 5. Legacy codepage: guess, then decode. The fallback is Windows-1252,
    // which cannot fail — every byte maps to a character — so the file is always
    // at least readable, and its real encoding is what the inspector reports.
    let (text, label) = detect_legacy(bytes);
    finish(text, label, false)
}

/// The byte-order marks this ladder respects, as `(bytes to skip, label)`.
///
/// UTF-32's marks are deliberately *not* decoded: a UTF-32LE mark is a UTF-16LE
/// mark followed by two NULs, so they have to be excluded here rather than
/// guessed at later, and reading UTF-32 as UTF-16 would produce a plausible
/// string of CJK garbage.
fn bom(bytes: &[u8]) -> Option<(usize, &'static str)> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        Some((3, "utf-8"))
    } else if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff])
    {
        None
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        Some((2, "utf-16le"))
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some((2, "utf-16be"))
    } else {
        None
    }
}

/// UTF-16 without a mark, from the parity of its NUL bytes: `Some(true)` for
/// little-endian, `Some(false)` for big-endian, `None` when the sample is not
/// convincingly either.
fn utf16_by_parity(bytes: &[u8]) -> Option<bool> {
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    let mut odd_nul = 0usize;
    let mut even_nul = 0usize;
    let mut pairs = 0usize;
    for pair in sample.as_chunks::<2>().0 {
        pairs += 1;
        odd_nul += usize::from(pair[1] == 0);
        even_nul += usize::from(pair[0] == 0);
    }
    if pairs < 4 {
        return None;
    }
    let odd = odd_nul as f64 / pairs as f64;
    let even = even_nul as f64 / pairs as f64;
    // Text mostly in the ASCII range puts a NUL in the high byte of nearly every
    // unit and in the low byte of almost none. The thresholds are ICU's.
    if odd >= 0.3 && even <= 0.1 {
        Some(true)
    } else if even >= 0.3 && odd <= 0.1 {
        Some(false)
    } else {
        None
    }
}

/// More than 2% NUL in the sniff window is a binary file.
fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if sample.is_empty() {
        return false;
    }
    let nuls = sample.iter().filter(|b| **b == 0).count();
    nuls as f64 / sample.len() as f64 > 0.02
}

/// Decode UTF-16 of either endianness, dropping a trailing half unit.
fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// The statistical guess over legacy code pages, and the name of what it chose.
fn detect_legacy(bytes: &[u8]) -> (String, &'static str) {
    let sample = &bytes[..bytes.len().min(DETECT_BYTES)];
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(sample, true);
    let guess = detector.guess(None, chardetng::Utf8Detection::Deny);
    let (text, _had_replacements) = guess.decode_without_bom_handling(bytes);
    (text.into_owned(), guess.name())
}

/// Drop a trailing incomplete UTF-8 sequence, up to three bytes.
///
/// A byte cap lands wherever it lands, and the last character of a capped read
/// is often cut in half. Left in place, `from_utf8` rejects the whole buffer and
/// a perfectly ordinary UTF-8 file gets reported as some legacy codepage.
fn trim_incomplete_utf8(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    for back in 1..=3usize {
        if end < back {
            break;
        }
        let byte = bytes[end - back];
        if byte & 0xc0 == 0x80 {
            continue; // a continuation byte: keep walking back to its lead
        }
        let needed = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => break, // not a lead byte at all: leave the tail alone
        };
        if back < needed {
            end -= back;
        }
        break;
    }
    &bytes[..end]
}

/// Lines in a text buffer, counting the four break styles a file might use.
///
/// Empty text is one line, not zero: an empty file still has a line to sit on,
/// and a card that says "0 lines" reads as a failure.
pub fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut lines = 1usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines += 1;
                i += 1;
            }
            b'\r' => {
                lines += 1;
                // A CRLF is one break, not two.
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            // U+2028 and U+2029 are line separators in their own right, and both
            // share a lead byte.
            0xe2 if bytes.get(i + 1) == Some(&0x80)
                && matches!(bytes.get(i + 2), Some(0xa8) | Some(0xa9)) =>
            {
                lines += 1;
                i += 3;
            }
            _ => i += 1,
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(bytes: &[u8]) -> TextContent {
        decode(bytes, false, bytes.len(), bytes.len() as u64)
    }

    #[test]
    fn utf8_needs_no_mark() {
        let text = decoded("中文 — hello".as_bytes());
        assert_eq!(text.encoding, "utf-8");
        assert_eq!(text.text, "中文 — hello");
        assert!(!text.binary);
    }

    /// Every mark the ladder claims, decoded with its body stripped: leaving the
    /// mark in the text is how a viewer ends up showing a visible `\u{feff}`.
    #[test]
    fn byte_order_marks_are_stripped_and_respected() {
        let utf8 = decoded(&[0xef, 0xbb, 0xbf, b'a', b'b']);
        assert_eq!((utf8.text.as_str(), utf8.encoding), ("ab", "utf-8"));
        let le = decoded(&[0xff, 0xfe, 0x61, 0x00, 0x62, 0x00]);
        assert_eq!((le.text.as_str(), le.encoding), ("ab", "utf-16le"));
        let be = decoded(&[0xfe, 0xff, 0x00, 0x61, 0x00, 0x62]);
        assert_eq!((be.text.as_str(), be.encoding), ("ab", "utf-16be"));
    }

    /// UTF-16 without a mark, which is what many Windows tools write. The two
    /// endians are told apart by where the NULs sit.
    #[test]
    fn utf16_is_recognised_without_a_mark() {
        let source = "hello world, this is utf-16 without a mark at all";
        let mut le = Vec::new();
        let mut be = Vec::new();
        for unit in source.encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
            be.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(decoded(&le).encoding, "utf-16le");
        assert_eq!(decoded(&be).encoding, "utf-16be");
        assert_eq!(decoded(&le).text, source);
        assert_eq!(decoded(&be).text, source);
    }

    /// UTF-32 is not supported, and the failure mode to avoid is a plausible
    /// wrong answer: its mark is excluded so it lands as binary instead of as
    /// CJK garbage.
    #[test]
    fn utf32_is_reported_not_misread() {
        let mut le: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x00];
        for ch in "abcd".chars() {
            le.extend_from_slice(&(ch as u32).to_le_bytes());
        }
        assert!(decoded(&le).binary, "utf-32le must not decode as utf-16");
    }

    /// A GBK file is the case this whole ladder exists for: not UTF-8, no mark,
    /// and reading it as UTF-8 gives mojibake.
    #[test]
    fn a_legacy_chinese_file_is_not_read_as_utf8() {
        let source = "第一段中文文本，用于测试编码探测。第二段中文文本，同样用于测试。";
        let (gbk, _, had_errors) = encoding_rs::GBK.encode(source);
        assert!(!had_errors, "the fixture encodes");
        let read = decoded(&gbk);
        assert_ne!(read.encoding, "utf-8", "GBK is not UTF-8");
        assert!(
            read.text.contains("中文"),
            "decoded as {}: {:?}",
            read.encoding,
            read.text
        );
    }

    /// NULs scattered through the sniff window mean this is a binary file that
    /// happens to be called `.txt`.
    #[test]
    fn a_binary_file_says_so() {
        let bytes: Vec<u8> = (0..2000)
            .map(|i| if i % 20 == 0 { 0 } else { b'x' })
            .collect();
        let read = decoded(&bytes);
        assert!(read.binary);
        assert!(read.text.is_empty());
    }

    /// Line breaks: LF, CRLF, CR, and the two Unicode separators — with CRLF
    /// counted once, which is the mistake that doubles every Windows file's
    /// line count.
    #[test]
    fn the_four_line_break_styles_count_as_one_break_each() {
        assert_eq!(line_count(""), 1);
        assert_eq!(line_count("one"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\r\nb"), 2);
        assert_eq!(line_count("a\rb"), 2);
        assert_eq!(line_count("a\u{2028}b\u{2029}c"), 3);
        assert_eq!(line_count("a\nb\nc\n"), 4, "a trailing break opens a line");
    }

    /// The cap is reported, never hidden: a caller that could write the buffer
    /// back needs the flag to refuse.
    #[test]
    fn truncation_is_reported() {
        let dir = std::env::temp_dir().join(format!("trove-text-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        let body = "line\n".repeat(400);
        std::fs::write(&path, &body).unwrap();

        let whole = read(&path, body.len()).expect("read");
        assert!(!whole.truncated);
        assert_eq!(whole.bytes_read, body.len());

        let capped = read(&path, 16).expect("read capped");
        assert!(capped.truncated, "a 16-byte window on a bigger file");
        assert_eq!(capped.text, "line\nline\nline\nl");
        assert_eq!(capped.total_bytes as usize, body.len());
        assert_eq!(capped.line_count, 4);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cap that lands inside a multi-byte character must not demote the file
    /// to a legacy codepage: the cut tail is dropped, the rest stays UTF-8.
    #[test]
    fn a_cap_mid_character_stays_utf8() {
        let dir = std::env::temp_dir().join(format!("trove-text-cut-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cjk.txt");
        std::fs::write(&path, "中文中文中文").unwrap(); // 18 bytes, 3 per character

        let cut = read(&path, 4).expect("read");
        assert_eq!(cut.encoding, "utf-8", "not misreported as a legacy page");
        assert_eq!(
            cut.text, "中",
            "the half character is dropped, not replaced"
        );
        assert!(cut.truncated);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_snippet_is_one_line_and_bounded() {
        let read = decoded(b"first line\nsecond line\n\tthird\n");
        let short = read.snippet();
        assert!(!short.contains('\n') && !short.contains('\t'), "{short:?}");
        assert!(short.starts_with("first line second line"), "{short:?}");

        let long = decoded("x".repeat(CARD_SNIPPET_CHARS + 50).as_bytes());
        let clipped = long.snippet();
        assert_eq!(
            clipped.chars().count(),
            CARD_SNIPPET_CHARS + 1,
            "plus the ellipsis"
        );
        assert!(clipped.ends_with('…'));

        let exact = decoded("y".repeat(CARD_SNIPPET_CHARS).as_bytes());
        assert!(
            !exact.snippet().ends_with('…'),
            "a full-but-not-oversized run"
        );
    }

    /// The list is the single source: an extension in it must also carry a media
    /// type, and the formats Trove draws must stay out.
    #[test]
    fn the_text_list_excludes_what_trove_draws() {
        for ext in ["txt", "md", "json", "rs", "html", "wgsl", "csv", "srt"] {
            assert!(is_text_ext(ext), "{ext} should be text");
            let mime = text_mime(ext);
            assert!(
                mime.starts_with("text/") || mime.starts_with("application/"),
                "{ext} → {mime}"
            );
        }
        for ext in ["png", "jpg", "svg", "psd", "pdf", "glb", "mystery"] {
            assert!(!is_text_ext(ext), "{ext} is not text here");
        }
        assert_eq!(text_mime("json"), "application/json");
        assert_eq!(text_mime("txt"), "text/plain");
    }
}
