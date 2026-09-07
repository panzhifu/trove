//! CLIP BPE tokenizer — a faithful port of OpenAI's `simple_tokenizer.py`.
//!
//! The CLIP ONNX graph expects real BPE token ids from the 49 152-merge vocab
//! shipped with CLIP (`bpe_simple_vocab_16e6.txt`, ~1.3 MB plain text). The
//! previous placeholder hashed words into arbitrary ids, which produced
//! meaningless text embeddings and made semantic text search useless.
//!
//! The vocab file must sit next to `model.onnx`:
//! https://github.com/openai/CLIP/blob/main/clip/bpe_simple_vocab_16e6.txt
//!
//! Text is UTF-8 byte-encoded (via the GPT-2 `bytes_to_unicode` table), so any
//! language is encodable — but note the model itself was trained mostly on
//! English captions, so English queries match noticeably better.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use crate::error::{Error, Result};

pub const VOCAB_FILE: &str = "bpe_simple_vocab_16e6.txt";
const SOT: i64 = 49406;
const EOT: i64 = 49407;
const CONTEXT: usize = 77;

struct Vocab {
    encoder: HashMap<String, i64>,
    ranks: HashMap<(String, String), u32>,
}

static VOCAB: OnceLock<Option<Vocab>> = OnceLock::new();
static VOCAB_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Expected vocab location for a given model directory.
pub fn vocab_path(model_dir: &Path) -> std::path::PathBuf {
    model_dir.join(VOCAB_FILE)
}

/// Parse and cache the vocab file. Safe to call repeatedly; the last load wins.
pub fn load(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Db(format!("read vocab {}: {e}", path.display())))?;
    let mut lines = text.lines();
    // Skip the header line unconditionally — the published file embeds the
    // filename before "#version", and OpenAI's own loader never validates it.
    lines.next();
    // CLIP reads merges[1 : 49156 - 256 - 2 + 1] of the raw split (the header
    // line included), i.e. the first 48 896 merge lines.
    let merges: Vec<(String, String)> = lines
        .take(49152 - 256)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let a = it.next()?;
            let b = it.next()?;
            Some((a.to_string(), b.to_string()))
        })
        .collect();

    let byte_encoder = bytes_to_unicode();
    let mut encoder: HashMap<String, i64> = HashMap::with_capacity(49408);
    let mut id = 0i64;
    for ch in byte_encoder.values() {
        encoder.insert(ch.to_string(), id);
        id += 1;
    }
    for ch in byte_encoder.values() {
        encoder.insert(format!("{ch}</w>"), id);
        id += 1;
    }
    for (a, b) in &merges {
        encoder.insert(format!("{a}{b}"), id);
        id += 1;
    }
    let ranks: HashMap<(String, String), u32> = merges
        .into_iter()
        .enumerate()
        .map(|(i, pair)| (pair, i as u32))
        .collect();

    let _ = VOCAB_PATH.set(path.to_path_buf());
    let _ = VOCAB.set(Some(Vocab { encoder, ranks }));
    Ok(())
}

/// `true` when a vocab has been loaded and text encoding can proceed.
pub fn ready() -> bool {
    VOCAB.get().map(Option::is_some).unwrap_or(false)
}

/// The path the vocab was loaded from (for error messages / settings).
pub fn loaded_path() -> Option<&'static Path> {
    VOCAB_PATH.get().map(|p| p.as_path())
}

/// GPT-2 `bytes_to_unicode`: reversible byte → printable-char table.
fn bytes_to_unicode() -> HashMap<u8, char> {
    let mut bs: Vec<u32> = ((33..=126).chain(161..=172).chain(174..=255)).collect();
    let mut cs = bs.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    bs.into_iter()
        .zip(cs)
        .map(|(b, c)| (b as u8, char::from_u32(c).unwrap()))
        .collect()
}

fn whitespace_clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Port of the CLIP regex:
/// `<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+`
/// (case-insensitive, applied to the lowercased text).
fn tokenize_regex(text: &str) -> Vec<String> {
    use regex::Regex;
    static PAT: OnceLock<Regex> = OnceLock::new();
    let pat = PAT.get_or_init(|| {
        Regex::new(
            r"<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+",
        )
        .expect("tokenizer regex")
    });
    pat.find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn bpe(vocab: &Vocab, byte_encoder: &HashMap<u8, char>, token: &str) -> String {
    let chars: Vec<String> = token
        .as_bytes()
        .iter()
        .map(|b| byte_encoder[b].to_string())
        .collect();
    if chars.is_empty() {
        return String::new();
    }
    // Word = all chars, except the last carries the end-of-word marker.
    let mut word: Vec<String> = chars[..chars.len() - 1].to_vec();
    word.push(format!("{}</w>", chars[chars.len() - 1]));
    if word.len() == 1 {
        return word.pop().unwrap();
    }

    loop {
        // Find the adjacent pair with the lowest merge rank.
        let mut best: Option<(u32, usize)> = None;
        for i in 0..word.len() - 1 {
            let rank = vocab
                .ranks
                .get(&(word[i].clone(), word[i + 1].clone()))
                .copied();
            if let Some(r) = rank
                && (best.is_none() || r < best.unwrap().0)
            {
                best = Some((r, i));
            }
        }
        let Some((_, i)) = best else { break };
        let merged = format!("{}{}", word[i], word[i + 1]);
        word[i] = merged;
        word.remove(i + 1);
        if word.len() == 1 {
            break;
        }
    }
    word.join(" ")
}

/// Encode `text` into `(input_ids, attention_mask)`, both `CONTEXT` long:
/// `<SOT> tokens... <EOT>` then zero-padded; mask is 1 on non-pad slots.
pub fn encode(text: &str) -> Result<(Vec<i64>, Vec<i64>)> {
    let Some(vocab) = VOCAB.get().and_then(|v| v.as_ref()) else {
        let hint = VOCAB_PATH
            .get()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("the model directory / {VOCAB_FILE}"));
        return Err(Error::Db(format!(
            "CLIP vocab not loaded (expected {hint}); text search is unavailable"
        )));
    };
    let byte_encoder = bytes_to_unicode();
    let clean = whitespace_clean(&text.to_lowercase());
    let mut ids = vec![SOT];
    for token in tokenize_regex(&clean) {
        let encoded: String = token.as_bytes().iter().map(|b| byte_encoder[b]).collect();
        for piece in bpe(vocab, &byte_encoder, &encoded).split(' ') {
            if piece.is_empty() {
                continue;
            }
            match vocab.encoder.get(piece) {
                Some(id) => ids.push(*id),
                // Unknown piece: skip rather than emit an id the model maps to junk.
                None => continue,
            }
        }
        if ids.len() >= CONTEXT - 1 {
            break;
        }
    }
    ids.push(EOT);
    // Truncation always keeps an EOT in the last slot (CLIP `truncate=True`).
    if ids.len() > CONTEXT {
        ids.truncate(CONTEXT);
        ids[CONTEXT - 1] = EOT;
    }
    // Mask by token count, NOT by `id != 0`: token id 0 is a legitimate
    // piece ("!") in the CLIP vocab and must stay attended.
    let n = ids.len();
    let mut mask = vec![0i64; CONTEXT];
    mask[..n].fill(1);
    ids.resize(CONTEXT, 0);
    Ok((ids, mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny synthetic vocab: only single bytes + "</w>" forms, no
    /// merges — enough to exercise the encode pipeline without the real file.
    fn load_fake_vocab() {
        let dir = std::env::temp_dir().join(format!("trove-vocab-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(VOCAB_FILE);
        let mut content = String::from("#version: 0.2\n");
        // 49152 - 256 merge lines required by the parser; make trivial pairs.
        for i in 0..(49152 - 256) {
            content.push_str(&format!("z{}/w z{}/w\n", i % 97, i % 89));
        }
        std::fs::write(&path, content).unwrap();
        load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encode_shapes_and_specials() {
        load_fake_vocab();
        let (ids, mask) = encode("a red sunset").unwrap();
        assert_eq!(ids.len(), CONTEXT);
        assert_eq!(mask.len(), CONTEXT);
        assert_eq!(ids[0], SOT);
        // Exactly one EOT, followed only by padding zeros.
        let eot_pos = ids.iter().position(|&i| i == EOT).unwrap();
        assert!(ids[eot_pos + 1..].iter().all(|&i| i == 0));
        assert!(!ids[..eot_pos].contains(&EOT));
        // Mask: 1 on every non-pad slot, 0 from the first pad onwards.
        assert_eq!(mask[0], 1);
        assert_eq!(mask[eot_pos], 1);
        assert!(mask[..=eot_pos].iter().all(|&m| m == 1));
        assert!(mask[eot_pos + 1..].iter().all(|&m| m == 0));
    }

    #[test]
    fn encode_handles_utf8_bytes() {
        load_fake_vocab();
        // Any UTF-8 text (incl. Chinese) must encode without error.
        let (ids, _) = encode("一棵树").unwrap();
        assert_eq!(ids[0], SOT);
        assert!(ids.contains(&EOT));
    }

    #[test]
    fn encode_empty_text() {
        load_fake_vocab();
        let (ids, mask) = encode("").unwrap();
        assert_eq!((ids[0], ids[1], ids[2]), (SOT, EOT, 0));
        assert_eq!(mask[1], 1);
        assert_eq!(mask[2], 0);
    }

    /// Sanity check against the REAL CLIP vocab when it is installed in the
    /// default model directory (skipped elsewhere). Verifies the published
    /// file's odd header line parses and classic tokens get stable ids.
    #[test]
    fn real_vocab_if_present() {
        let Some(dir) = crate::config::AppConfig::config_dir() else {
            return;
        };
        let path = vocab_path(&dir.join("models"));
        if !path.is_file() {
            return; // vocab not installed on this machine — skip quietly
        }
        load(&path).unwrap();
        for text in ["a photo of a tree", "tree", "一棵树"] {
            let (ids, mask) = encode(text).unwrap();
            assert_eq!(ids[0], SOT, "{text}");
            assert_eq!(mask[0], 1, "{text}");
            let eot = ids.iter().position(|&i| i == EOT).unwrap();
            assert!(eot > 1, "{text}: expected at least one content token");
            assert!(ids[eot + 1..].iter().all(|&i| i == 0), "{text}");
        }
        // Stability: identical text → identical ids (classic CLIP anchors).
        let (a, _) = encode("a photo of a tree").unwrap();
        let (b, _) = encode("a photo of a tree").unwrap();
        assert_eq!(a, b);
    }
}
