//! Tokenizers and pinyin: the text analysis layer the schema registers.

use std::sync::{Arc, OnceLock};

use jieba_rs::Jieba;
use pinyin::ToPinyin;
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Jieba word segmentation for CJK-aware terms. Latin words survive as-is
/// (lowercased), so one tokenizer serves mixed text.
#[derive(Clone)]
pub(super) struct JiebaTokenizer(pub(super) Arc<Jieba>);

pub(super) struct JiebaTokenStream {
    tokens: Vec<Token>,
    ix: usize,
}

impl Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = JiebaTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens: Vec<Token> = Vec::new();
        // `cut_for_search` emits overlapping sub-words in positional order,
        // so the cursor advances one character at a time instead of jumping
        // past each match.
        let mut cursor = 0usize;
        let mut position = 0usize;
        for word in self.0.cut_for_search(text, true) {
            // An empty "word" would index an empty term and, because `find("")`
            // succeeds at offset 0, would also advance the cursor past a byte of
            // real text.
            let Some(first) = word.chars().next() else {
                continue;
            };
            // jieba-rs also hands back the separators it split on — whitespace
            // arrives as a token of its own (the Python implementation filters
            // it, jieba-rs deliberately keeps it). The query side never names
            // one: the box splits on whitespace and `phrase_query_on` trims its
            // tokens. An indexed space would be a position no query can reach
            // that still sits between every adjacent word pair — exactly the
            // position `PhraseQuery` needs to be contiguous — so a quoted
            // phrase could only ever be answered by the gram fallback's grace.
            // Whitespace is not content on either side; skipping it here is
            // what lets the positional leg do its job.
            if word.trim().is_empty() {
                continue;
            }
            let Some(rel) = text[cursor.min(text.len())..].find(word) else {
                continue;
            };
            let from = cursor + rel;
            tokens.push(Token {
                offset_from: from,
                offset_to: from + word.len(),
                position,
                text: word.to_lowercase(),
                position_length: 1,
            });
            position += 1;
            // One character, not the whole word: `cut_for_search` emits
            // overlapping sub-words that start inside the previous one.
            cursor = from + first.len_utf8();
        }
        JiebaTokenStream { tokens, ix: 0 }
    }
}

impl TokenStream for JiebaTokenStream {
    fn advance(&mut self) -> bool {
        if self.ix < self.tokens.len() {
            self.ix += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.ix - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.ix - 1]
    }
}

pub(super) fn jieba() -> Arc<Jieba> {
    static JIEBA: OnceLock<Arc<Jieba>> = OnceLock::new();
    JIEBA.get_or_init(|| Arc::new(Jieba::new())).clone()
}

/// Hanzi → (full pinyin syllables, initials). Non-hanzi characters are
/// skipped — they are already matched through the word and gram fields.
pub(super) fn pinyin_of(text: &str) -> (String, String) {
    let mut full: Vec<&str> = Vec::new();
    let mut abbr = String::new();
    for p in text.to_pinyin().flatten() {
        let syllable = p.plain();
        full.push(syllable);
        abbr.push(syllable.chars().next().unwrap_or_default());
    }
    (full.join(" "), abbr)
}

// ============================ facts extraction ===============================
