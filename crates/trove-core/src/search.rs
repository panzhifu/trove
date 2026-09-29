//! Embedded full-text search (Tantivy).
//!
//! SQLite stays the single source of truth; this index is a disposable
//! derived artifact under `<root>/search_index`. Mutations reach the index
//! through the `search_queue` outbox table (schema triggers fire on every
//! asset / tag write) drained by [`drain`] — no mutation site has to
//! remember the index, and a lost or outdated index is repaired by
//! re-enqueueing everything and draining again.
//!
//! Fields are indexed three ways per text surface (file name / title /
//! description / tags): jieba **words** (CJK-aware terms + typo-tolerant
//! fuzzy), **2–3 grams** (substring matching), and **pinyin** (full
//! syllables + initials, so `mao` finds 猫). Search composes all of them
//! as ranked `should` clauses under a per-term `must`.
//!
//! Text is one leg of retrieval; the other is semantic — [`vector`] holds
//! the in-memory embedding index over the `asset_embeddings` table.

pub mod expression;
pub mod highlight;
pub mod vector;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use jieba_rs::Jieba;
use pinyin::ToPinyin;
use rusqlite::Connection;
use rusqlite::types::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value as _,
};
use tantivy::tokenizer::{
    LowerCaser, NgramTokenizer, RawTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
    WhitespaceTokenizer,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::model::AssetFacts;
use crate::store::assets;

/// Bump when the schema or the query semantics change incompatibly: the
/// version file beside the index is checked on open and a mismatch wipes
/// the directory for a full rebuild.
///
/// 3: added metadata fact fields (camera, artist, album, font, composite
/// facts) so EXIF and media-tag content is searchable.
/// 4: added per-surface pinyin fields (name_pinyin, title_pinyin,
/// desc_pinyin, tags_pinyin) so field-qualified terms like `tag:mao` can
/// match pinyin within that specific surface.
/// 5: added audio_words / audio_tri fields so `audio:` qualifier has a
/// dedicated index surface for sample rate, channels, bit depth, bitrate.
/// 6: the jieba tokenizer no longer indexes whitespace. jieba-rs emits the
/// separators it splits on as tokens of their own (the Python
/// implementation filters them, jieba-rs deliberately keeps them), while the
/// query side never names one — `build_query` splits on whitespace and
/// `phrase_query_on` trims its tokens. In a v5 index a space token therefore
/// sat between every adjacent word pair, `PhraseQuery` could never fire
/// across one, and quoted phrases were carried by the gram fallback alone.
/// Positions change, so old indexes rebuild.
const INDEX_VERSION: u32 = 6;
/// Heap budget for the index writer, in bytes.
const WRITER_HEAP: usize = 32 * 1024 * 1024;
/// How many ranked candidates one text lookup may contribute before the
/// SQL filters narrow them (search) or they become an `id IN` list
/// (smart rules).
///
/// This is a *user-visible* limit, not just a tuning knob: a broad query
/// matches far more documents than this, and the reported total stops climbing
/// at the cap.
pub const CANDIDATE_CAP: usize = 2000;
/// The ceiling on a gather that has been widened to answer a filtered search.
///
/// The widening itself is not optional — see [`pool_for`](TextIndex::pool_for):
/// filters run *after* ranking, so any pool that stops short of the term's
/// whole result set can hide the one asset that matches both. This number is
/// where "complete" gives way to "bounded": above it the search says so out
/// loud (`Page::truncated`, which the title bar renders as "at least N" and
/// `trove search` reports as a `truncated` field) rather than being quietly
/// short.
///
/// The cost is linear in the pool and dominated by one stored-document read
/// per hit — measured on a 100k-document index, a debug build: 2000 ids in
/// 99 ms, 20000 in 297 ms, all 100000 in 1.02 s. It is paid once per frozen
/// listing ([`crate::store::BrowseSession`]), not once per page.
pub const MAX_RANKED_POOL: usize = 200_000;
/// Registered tokenizer names.
const TOK_JIEBA: &str = "jieba";
const TOK_TRI: &str = "tri";
const TOK_PINYIN: &str = "pinyin";
const TOK_ABBR: &str = "abbr";
const TOK_RAW: &str = "raw";

// ============================ tokenizer ======================================

/// Jieba word segmentation for CJK-aware terms. Latin words survive as-is
/// (lowercased), so one tokenizer serves mixed text.
#[derive(Clone)]
struct JiebaTokenizer(Arc<Jieba>);

struct JiebaTokenStream {
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

fn jieba() -> Arc<Jieba> {
    static JIEBA: OnceLock<Arc<Jieba>> = OnceLock::new();
    JIEBA.get_or_init(|| Arc::new(Jieba::new())).clone()
}

/// Hanzi → (full pinyin syllables, initials). Non-hanzi characters are
/// skipped — they are already matched through the word and gram fields.
fn pinyin_of(text: &str) -> (String, String) {
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

/// Per-category searchable text extracted from an asset's metadata.
///
/// Each field is empty when the asset carries no data for that category.
/// The composite [`composite`](FactTexts::composite) joins them all for the
/// catch-all `Facts` target and for pinyin/abbreviation derivation.
#[derive(Default)]
struct FactTexts {
    camera: String,
    artist: String,
    album: String,
    font: String,
    audio: String,
    embedded_title: String,
}

impl FactTexts {
    /// All categories concatenated, for the composite facts field and for
    /// pinyin/abbreviation derivation.
    fn composite(&self) -> String {
        [
            self.camera.as_str(),
            self.artist.as_str(),
            self.album.as_str(),
            self.font.as_str(),
            self.audio.as_str(),
            self.embedded_title.as_str(),
        ]
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    }
}

/// Build per-category searchable text from an asset's typed metadata.
///
/// Each category collects the human-readable facts a user would type:
///
/// - **camera**: make, model, ISO, aperture, focal length, exposure time
/// - **artist**: embedded artist tag (audio/video)
/// - **album**: embedded album tag
/// - **font**: family, style, weight, glyph count
///
/// Numbers are formatted the way a user would type them (`"ISO 400"`,
/// `"f/2.8"`, `"1/60s"`), so a free-text search for `400` or `2.8` finds the
/// asset without a qualifier.  Source URL is appended to the camera text
/// because it is the closest analogue to "where this came from".
fn extract_fact_texts(facts: &AssetFacts, source_url: Option<&str>) -> FactTexts {
    let mut camera_parts: Vec<String> = Vec::new();
    if let Some(ref s) = facts.photo.make {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.model {
        camera_parts.push(s.clone());
    }
    if let Some(iso) = facts.photo.iso {
        camera_parts.push(format!("ISO {iso}"));
    }
    if let Some(ref s) = facts.photo.aperture_f {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.focal_length_mm {
        camera_parts.push(s.clone());
    }
    if let Some(ref s) = facts.photo.exposure_time {
        camera_parts.push(s.clone());
    }
    if let Some(url) = source_url {
        camera_parts.push(url.to_owned());
    }

    let artist = facts.media.artist.clone().unwrap_or_default();
    let album = facts.media.album.clone().unwrap_or_default();

    let mut font_parts: Vec<String> = Vec::new();
    if let Some(ref s) = facts.font.family {
        font_parts.push(s.clone());
    }
    if let Some(ref s) = facts.font.style {
        font_parts.push(s.clone());
    }
    if let Some(w) = facts.font.weight {
        font_parts.push(w.to_string());
    }
    if let Some(g) = facts.font.glyphs {
        font_parts.push(format!("{g}glyphs"));
    }

    // Audio technical specs: a dedicated `audio:` qualifier scopes to these
    // (see `Target::Audio`), and the composite facts field still carries them
    // so unqualified searches for e.g. "48000" or "24bit" find audio assets.
    let audio_parts: Vec<String> = {
        let mut v = Vec::new();
        if let Some(hz) = facts.audio.sample_rate {
            v.push(format!("{hz} Hz"));
        }
        if let Some(ch) = facts.audio.channels {
            v.push(format!("{ch}c"));
        }
        if let Some(bd) = facts.audio.bit_depth {
            v.push(format!("{bd}bit"));
        }
        if let Some(br) = facts.audio.bitrate {
            v.push(format!("{br}kbps"));
        }
        v
    };

    let embedded_title = facts.media.embedded_title.clone().unwrap_or_default();

    FactTexts {
        camera: camera_parts.join(" "),
        artist,
        album,
        font: font_parts.join(" "),
        audio: audio_parts.join(" "),
        embedded_title,
    }
}

// ============================ index ==========================================

/// The indexed fields, resolved once at open.
#[derive(Clone, Copy)]
struct Fields {
    asset_id: Field,
    name_w: Field,
    title_w: Field,
    desc_w: Field,
    tags_w: Field,
    name_tri: Field,
    title_tri: Field,
    desc_tri: Field,
    tags_tri: Field,
    pinyin: Field,
    abbr: Field,
    // Per-surface pinyin fields for field-qualified pinyin matching
    name_pinyin: Field,
    title_pinyin: Field,
    desc_pinyin: Field,
    tags_pinyin: Field,
    // Metadata fact fields — indexed from the asset's `extra` JSON.
    facts_w: Field,
    facts_tri: Field,
    camera_w: Field,
    camera_tri: Field,
    artist_w: Field,
    artist_tri: Field,
    album_w: Field,
    album_tri: Field,
    font_w: Field,
    font_tri: Field,
    audio_w: Field,
    audio_tri: Field,
}

impl Fields {
    /// The word surfaces — jieba-tokenized, position-aware — that a target
    /// answers on.
    ///
    /// One table with several readers on purpose. `term_query_on`,
    /// `phrase_query_on` and the ranked paths all ask the same question, "which
    /// indexed field is `album:`", and each copy that answers it separately is
    /// one place to forget a new target: `Target::Audio` was added by editing
    /// three of these in lockstep, and the fourth reader failing to compile was
    /// the only thing that caught it.
    fn words_for(&self, target: expression::Target) -> Vec<Field> {
        use expression::Target::*;
        match target {
            All => vec![
                self.name_w,
                self.title_w,
                self.desc_w,
                self.tags_w,
                self.facts_w,
            ],
            Name => vec![self.name_w],
            Title => vec![self.title_w],
            Description => vec![self.desc_w],
            Tags => vec![self.tags_w],
            Facts => vec![self.facts_w],
            Camera => vec![self.camera_w],
            Artist => vec![self.artist_w],
            Album => vec![self.album_w],
            Font => vec![self.font_w],
            Audio => vec![self.audio_w],
        }
    }

    /// The same surfaces in their trigram form, which is what an infix
    /// substring (`ower` → `flower`) is looked up in.
    fn tris_for(&self, target: expression::Target) -> Vec<Field> {
        use expression::Target::*;
        match target {
            All => vec![
                self.name_tri,
                self.title_tri,
                self.desc_tri,
                self.tags_tri,
                self.facts_tri,
            ],
            Name => vec![self.name_tri],
            Title => vec![self.title_tri],
            Description => vec![self.desc_tri],
            Tags => vec![self.tags_tri],
            Facts => vec![self.facts_tri],
            Camera => vec![self.camera_tri],
            Artist => vec![self.artist_tri],
            Album => vec![self.album_tri],
            Font => vec![self.font_tri],
            Audio => vec![self.audio_tri],
        }
    }

    /// The per-surface pinyin field for a target, where one exists.
    ///
    /// Only the four hand-written text surfaces have one. The metadata facts
    /// deliberately do not: a camera model or an artist name reaching a pinyin
    /// match would answer a question nobody asked, which is the same
    /// "wrong answer with no visible cause" the field-scoped pinyin exists to
    /// avoid. `All` uses the concatenated [`pinyin`](Fields::pinyin) instead.
    fn pinyin_for(&self, target: expression::Target) -> Option<Field> {
        use expression::Target::*;
        match target {
            Name => Some(self.name_pinyin),
            Title => Some(self.title_pinyin),
            Description => Some(self.desc_pinyin),
            Tags => Some(self.tags_pinyin),
            All | Facts | Camera | Artist | Album | Font | Audio => None,
        }
    }
}

fn indexed_text(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
    )
}

/// Index options for jieba-tokenized fields that support phrase queries.
/// Phrase queries need position information to enforce term ordering.
fn indexed_text_with_positions(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

fn indexed_basic(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_stored().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::Basic),
    )
}

/// The Tantivy index. Lives in the same single-threaded `Rc` world as the
/// store connection; `Rc` fields keep `Library`'s `Clone` derive valid.
/// How hard [`TextIndex::open_with`] tries to take Tantivy's writer lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriterLock {
    /// Fail when another process holds it (the desktop app's own open).
    Required,
    /// Serve reads from `reader` alone when it is held elsewhere.
    Optional,
}

#[derive(Clone)]
pub struct TextIndex {
    /// `None` for a read-only handle: Tantivy's `INDEX_WRITER_LOCK` belongs
    /// to another process, so searches are answered from `reader` alone.
    /// Every write path goes through [`TextIndex::writer`], which fails
    /// loudly rather than dropping documents on the floor.
    writer: Option<Rc<RefCell<IndexWriter>>>,
    reader: IndexReader,
    f: Fields,
}

impl TextIndex {
    /// Open (or create) the index under `dir`, taking the writer lock. A
    /// missing, corrupted or version-mismatched index is wiped and
    /// recreated — it is rebuilt from SQLite through the queue, so nothing
    /// is lost. Fails when another process already holds the lock.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with(dir, WriterLock::Required)
    }

    /// Open an index for querying that does not require the writer lock, so
    /// a second process (the CLI) can read a library the desktop app has
    /// open. Writes are unavailable in that case and
    /// [`TextIndex::is_writable`] says so.
    ///
    /// The one thing this variant never does is repair: an index that exists
    /// but is outdated or unreadable is left exactly as it is, because the
    /// lock it could not take is a reason to touch nothing — it answers from
    /// an empty index instead, until a writable handle rebuilds it. A
    /// directory holding *no* index is a different matter: there is nothing
    /// to preserve and nobody to disturb, so one is created. That is the
    /// state a freshly created library is in, and refusing to build its index
    /// would leave it permanently unsearchable from the CLI.
    pub fn open_read_only(dir: &Path) -> Result<Self> {
        Self::open_with(dir, WriterLock::Optional)
    }

    fn open_with(dir: &Path, lock: WriterLock) -> Result<Self> {
        let writable = lock == WriterLock::Required;
        // A Tantivy directory identifies itself with a `meta.json`; without
        // one there is no index on disk at all.
        let existing = dir.join("meta.json").is_file();
        let version_file = dir.join("trove-index-version");
        let version_ok = std::fs::read_to_string(&version_file)
            .is_ok_and(|s| s.trim() == INDEX_VERSION.to_string());

        if existing && !version_ok && !writable {
            tracing::debug!(
                dir = %dir.display(),
                "search index is missing or outdated; read-only handle serves an empty index",
            );
            return Self::empty();
        }
        if existing && !version_ok {
            let _ = std::fs::remove_dir_all(dir);
        }

        std::fs::create_dir_all(dir)?;
        let index = match Index::open_in_dir(dir) {
            Ok(index) => index,
            Err(error) => {
                // Unreadable with an index supposedly present: clearing it out
                // is the writable path's repair, not something to do here.
                if !writable && existing {
                    return Err(Error::Db(format!("search index: {error}")));
                }
                let _ = std::fs::remove_dir_all(dir);
                std::fs::create_dir_all(dir)?;
                Index::create_in_dir(dir, Self::schema())
                    .map_err(|e| Error::Db(format!("search index: {e}")))?
            }
        };
        let writer = match index.writer(WRITER_HEAP) {
            Ok(writer) => Some(writer),
            Err(e) => match lock {
                WriterLock::Required => {
                    return Err(Error::Db(format!("search index writer: {e}")));
                }
                WriterLock::Optional => {
                    tracing::debug!(
                        dir = %dir.display(),
                        error = %e,
                        "search index writer held elsewhere; serving reads only",
                    );
                    None
                }
            },
        };
        let this = Self::finish(index, writer);
        // Only a handle that actually owns the writer may stamp the version:
        // a read-only one did not create anything worth recording.
        if this.is_writable() {
            let _ = std::fs::write(version_file, INDEX_VERSION.to_string());
        }
        Ok(this)
    }

    /// An in-memory index (tests).
    pub fn in_ram() -> Result<Self> {
        let index = Index::create_in_ram(Self::schema());
        let writer = index.writer(WRITER_HEAP).expect("in-memory index writer");
        Ok(Self::finish(index, Some(writer)))
    }

    /// A writer-less empty index, the read-only fallback when there is no
    /// usable index on disk: searches match nothing instead of erroring.
    fn empty() -> Result<Self> {
        Ok(Self::finish(Index::create_in_ram(Self::schema()), None))
    }

    /// Whether this handle owns the index writer. `false` means the index
    /// belongs to another process: reads work, writes are refused.
    pub fn is_writable(&self) -> bool {
        self.writer.is_some()
    }

    /// The writer, or an error explaining that the index is read-only. The
    /// single gate every mutating entry point goes through.
    fn writer(&self) -> Result<std::cell::RefMut<'_, IndexWriter>> {
        match self.writer.as_ref() {
            Some(writer) => Ok(writer.borrow_mut()),
            None => Err(Error::Validation(
                "search index is read-only: another process holds its writer lock".into(),
            )),
        }
    }

    fn schema() -> Schema {
        let mut builder = Schema::builder();
        builder.add_text_field("asset_id", indexed_basic(TOK_RAW));
        // Jieba fields need positions for phrase queries
        for name in ["name_words", "title_words", "desc_words", "tags_words"] {
            builder.add_text_field(name, indexed_text_with_positions(TOK_JIEBA));
        }
        for name in ["name_tri", "title_tri", "desc_tri", "tags_tri"] {
            builder.add_text_field(name, indexed_text(TOK_TRI));
        }
        builder.add_text_field("pinyin", indexed_text(TOK_PINYIN));
        builder.add_text_field("pinyin_abbr", indexed_text(TOK_ABBR));
        // Per-surface pinyin fields for field-qualified pinyin matching
        for name in ["name_pinyin", "title_pinyin", "desc_pinyin", "tags_pinyin"] {
            builder.add_text_field(name, indexed_text(TOK_PINYIN));
        }
        // Metadata fact fields: composite + per-category.
        // Fact fields also need positions for phrase queries
        builder.add_text_field("facts_words", indexed_text_with_positions(TOK_JIEBA));
        builder.add_text_field("facts_tri", indexed_text(TOK_TRI));
        for name in [
            "camera_words",
            "artist_words",
            "album_words",
            "font_words",
            "audio_words",
        ] {
            builder.add_text_field(name, indexed_text_with_positions(TOK_JIEBA));
        }
        for name in [
            "camera_tri",
            "artist_tri",
            "album_tri",
            "font_tri",
            "audio_tri",
        ] {
            builder.add_text_field(name, indexed_text(TOK_TRI));
        }
        builder.build()
    }

    /// Register the tokenizers and assemble the handle. `writer` is `None`
    /// for a read-only handle — see [`TextIndex::open_read_only`]; the
    /// tokenizer registry is shared with the searchers either way, so it has
    /// to be filled before the reader exists.
    fn finish(index: Index, writer: Option<IndexWriter>) -> Self {
        index
            .tokenizers()
            .register(TOK_JIEBA, TextAnalyzer::from(JiebaTokenizer(jieba())));
        index.tokenizers().register(
            TOK_TRI,
            TextAnalyzer::builder(NgramTokenizer::new(2, 3, false).expect("valid ngram bounds"))
                .filter(LowerCaser)
                .build(),
        );
        index.tokenizers().register(
            TOK_PINYIN,
            TextAnalyzer::builder(WhitespaceTokenizer::default())
                .filter(LowerCaser)
                .build(),
        );
        index
            .tokenizers()
            .register(TOK_ABBR, TextAnalyzer::from(RawTokenizer::default()));
        index
            .tokenizers()
            .register(TOK_RAW, TextAnalyzer::from(RawTokenizer::default()));

        let schema = index.schema();
        let field = |name: &str| schema.get_field(name).expect("schema field");
        let f = Fields {
            asset_id: field("asset_id"),
            name_w: field("name_words"),
            title_w: field("title_words"),
            desc_w: field("desc_words"),
            tags_w: field("tags_words"),
            name_tri: field("name_tri"),
            title_tri: field("title_tri"),
            desc_tri: field("desc_tri"),
            tags_tri: field("tags_tri"),
            pinyin: field("pinyin"),
            abbr: field("pinyin_abbr"),
            name_pinyin: field("name_pinyin"),
            title_pinyin: field("title_pinyin"),
            desc_pinyin: field("desc_pinyin"),
            tags_pinyin: field("tags_pinyin"),
            facts_w: field("facts_words"),
            facts_tri: field("facts_tri"),
            camera_w: field("camera_words"),
            camera_tri: field("camera_tri"),
            artist_w: field("artist_words"),
            artist_tri: field("artist_tri"),
            album_w: field("album_words"),
            album_tri: field("album_tri"),
            font_w: field("font_words"),
            font_tri: field("font_tri"),
            audio_w: field("audio_words"),
            audio_tri: field("audio_tri"),
        };
        let reader = index.reader().expect("index reader");
        Self {
            writer: writer.map(|w| Rc::new(RefCell::new(w))),
            reader,
            f,
        }
    }

    /// Add or refresh one asset's document from the store row.
    ///
    /// A missing row means the asset is gone, so any document left for it is
    /// deleted rather than skipped: an enqueue that says "live" can coexist
    /// with the delete (the queue allows duplicates, e.g. the `asset_tag`
    /// cascade fires while the asset itself is being deleted), and the row is
    /// the only authority on whether the asset exists.
    pub fn index_asset(&self, conn: &Connection, asset_id: Uuid) -> Result<()> {
        let writer = self.writer()?;
        let Some(a) = assets::get(conn, asset_id)? else {
            self.remove_asset_with(&writer, asset_id);
            return Ok(());
        };
        let tags = assets::tags_for_index(conn, asset_id)?;
        let facts = extract_fact_texts(&a.facts, a.source_url.as_deref());
        self.index_asset_text(
            &writer,
            &asset_id.to_string(),
            &a.file_name,
            a.title.as_deref(),
            a.description.as_deref(),
            &tags,
            &facts,
        );
        Ok(())
    }

    /// Low-level upsert from already-resolved text. Takes the writer as an
    /// argument rather than borrowing it, so [`TextIndex::writer`] stays the
    /// single place a read-only handle is refused.
    ///
    /// `facts` carries per-category metadata text (camera EXIF, artist, album,
    /// font). The composite is derived internally for the catch-all facts
    /// field and for pinyin/abbreviation derivation.
    #[allow(clippy::too_many_arguments)]
    fn index_asset_text(
        &self,
        writer: &IndexWriter,
        id: &str,
        file_name: &str,
        title: Option<&str>,
        description: Option<&str>,
        tags: &str,
        facts: &FactTexts,
    ) {
        let facts_composite = facts.composite();
        let searchable = format!(
            "{} {} {} {} {}",
            file_name,
            title.unwrap_or_default(),
            description.unwrap_or_default(),
            tags,
            facts_composite,
        );
        let (pinyin, abbr) = pinyin_of(&searchable);

        let mut doc = TantivyDocument::new();
        doc.add_text(self.f.asset_id, id);
        doc.add_text(self.f.name_w, file_name);
        doc.add_text(self.f.name_tri, file_name);
        // Per-surface pinyin for field-qualified matching
        let (name_py, _) = pinyin_of(file_name);
        if !name_py.is_empty() {
            doc.add_text(self.f.name_pinyin, &name_py);
        }
        if let Some(title) = title {
            doc.add_text(self.f.title_w, title);
            doc.add_text(self.f.title_tri, title);
            let (title_py, _) = pinyin_of(title);
            if !title_py.is_empty() {
                doc.add_text(self.f.title_pinyin, &title_py);
            }
        }
        if let Some(desc) = description {
            doc.add_text(self.f.desc_w, desc);
            doc.add_text(self.f.desc_tri, desc);
            let (desc_py, _) = pinyin_of(desc);
            if !desc_py.is_empty() {
                doc.add_text(self.f.desc_pinyin, &desc_py);
            }
        }
        doc.add_text(self.f.tags_w, tags);
        doc.add_text(self.f.tags_tri, tags);
        let (tags_py, _) = pinyin_of(tags);
        if !tags_py.is_empty() {
            doc.add_text(self.f.tags_pinyin, &tags_py);
        }
        // Metadata fact fields: composite + per-category.
        if !facts_composite.is_empty() {
            doc.add_text(self.f.facts_w, &facts_composite);
            doc.add_text(self.f.facts_tri, &facts_composite);
        }
        if !facts.camera.is_empty() {
            doc.add_text(self.f.camera_w, &facts.camera);
            doc.add_text(self.f.camera_tri, &facts.camera);
        }
        if !facts.artist.is_empty() {
            doc.add_text(self.f.artist_w, &facts.artist);
            doc.add_text(self.f.artist_tri, &facts.artist);
        }
        if !facts.album.is_empty() {
            doc.add_text(self.f.album_w, &facts.album);
            doc.add_text(self.f.album_tri, &facts.album);
        }
        if !facts.font.is_empty() {
            doc.add_text(self.f.font_w, &facts.font);
            doc.add_text(self.f.font_tri, &facts.font);
        }
        if !facts.audio.is_empty() {
            doc.add_text(self.f.audio_w, &facts.audio);
            doc.add_text(self.f.audio_tri, &facts.audio);
        }
        doc.add_text(self.f.pinyin, &pinyin);
        doc.add_text(self.f.abbr, &abbr);

        writer.delete_term(Term::from_field_text(self.f.asset_id, id));
        writer
            .add_document(doc)
            .expect("document accepted by the writer");
    }

    /// Drop one asset's document (purge).
    pub fn remove_asset(&self, asset_id: Uuid) -> Result<()> {
        let writer = self.writer()?;
        self.remove_asset_with(&writer, asset_id);
        Ok(())
    }

    fn remove_asset_with(&self, writer: &IndexWriter, asset_id: Uuid) {
        writer.delete_term(Term::from_field_text(
            self.f.asset_id,
            &asset_id.to_string(),
        ));
    }

    /// Flush pending changes and publish them to readers.
    pub fn commit(&self) -> Result<()> {
        let mut writer = self.writer()?;
        writer
            .commit()
            .map_err(|e| Error::Db(format!("search index: {e}")))?;
        self.reader
            .reload()
            .map_err(|e| Error::Db(format!("search index: {e}")))?;
        Ok(())
    }

    /// Drop every document (the queue rebuild picks up from here).
    pub fn wipe(&self) -> Result<()> {
        {
            let writer = self.writer()?;
            writer
                .delete_all_documents()
                .map_err(|e| Error::Db(format!("search index: {e}")))?;
        }
        self.commit()
    }

    /// Number of documents currently visible to readers.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Ranked asset ids matching `query`: every term must match (AND);
    /// within a term the word / fuzzy / gram / pinyin alternatives compete
    /// by score.
    pub fn search(&self, query: &str, cap: usize) -> Result<Vec<Uuid>> {
        let searcher = self.reader.searcher();
        let top = searcher
            .search(
                &self.build_query(query),
                &TopDocs::with_limit(cap).order_by_score(),
            )
            .map_err(|e| Error::Db(format!("search index: {e}")))?;
        Self::collect_ids(&searcher, self.f.asset_id, top)
    }

    /// Search a parsed [`expression::Expression`]: `|` groups OR, atoms inside
    /// a group AND, a `-` atom excludes, and a field qualifier restricts which
    /// surface a term may match.
    pub fn search_expression(
        &self,
        expr: &expression::Expression,
        cap: usize,
    ) -> Result<Vec<Uuid>> {
        let searcher = self.reader.searcher();
        let top = searcher
            .search(
                &self.build_expression_query(expr),
                &TopDocs::with_limit(cap).order_by_score(),
            )
            .map_err(|e| Error::Db(format!("search index: {e}")))?;
        Self::collect_ids(&searcher, self.f.asset_id, top)
    }

    /// Search using an AI-generated plan: keywords are ANDed (Must),
    /// synonyms are ORed (Should), and exclusions are negated (MustNot).
    /// Synonyms only boost ranking — a keyword-only match still returns.
    pub fn search_plan(
        &self,
        plan: &crate::ai::search_planner::AiSearchPlan,
        cap: usize,
    ) -> Result<Vec<Uuid>> {
        let searcher = self.reader.searcher();
        let query = self.build_plan_query(plan);
        let top = searcher
            .search(&query, &TopDocs::with_limit(cap).order_by_score())
            .map_err(|e| Error::Db(format!("search index: {e}")))?;
        Self::collect_ids(&searcher, self.f.asset_id, top)
    }

    /// The ranked pool for one search box, gathered wide enough to survive the
    /// filters that are about to be applied to it.
    ///
    /// Returns the ids and whether the pool **ran out** — `true` means the
    /// caller's total is a floor and more matches exist than were seen. Two
    /// facts make this worth one extra gather instead of a permanent
    /// compromise: a pool that comes back exactly full is the visible sign of
    /// running out, and the filters are applied *after* ranking
    /// ([`crate::store::assets::rank_intersect`]), so a saturated pool can hide
    /// a match that satisfies both the term and the filter.
    ///
    /// `plan` replaces the expression's terms when the AI tier answered for a
    /// plain box; `raw` is what an unremarkable box is looked up by, so the
    /// pre-expression path stays byte-identical.
    pub fn pool_for(
        &self,
        raw: &str,
        expr: &expression::Expression,
        plan: Option<&crate::ai::search_planner::AiSearchPlan>,
        narrowed: bool,
    ) -> Result<(Vec<Uuid>, bool)> {
        let first = self.pool_at(raw, expr, plan, CANDIDATE_CAP)?;
        if first.len() < CANDIDATE_CAP {
            return Ok((first, false));
        }
        if !narrowed {
            // Nothing is about to reject rows, so the cap bounds the *listing*,
            // not the answer: every asset the term matches is in the ranking,
            // the caller simply stops showing it at 2000. `truncated` says so.
            return Ok((first, true));
        }
        // With filters in play, gathering to a fixed width can hide a match, so
        // the width that is not a compromise is "everything the term has".
        let (cap, ceiling) = Self::gather_cap(self.num_docs());
        let wide = self.pool_at(raw, expr, plan, cap)?;
        let ran_out = ceiling && wide.len() == cap;
        Ok((wide, ran_out))
    }

    /// `(how wide to gather, whether that width can still hide something)`.
    ///
    /// A library with no more documents than [`MAX_RANKED_POOL`] is gathered in
    /// full, which cannot hide anything — the pool is the whole index, so a
    /// saturated length there only means the *term* matched everything and the
    /// filters are free to reject it. Only past the ceiling does a gather return
    /// less than exists, and only then is `truncated` the honest word.
    fn gather_cap(docs: u64) -> (usize, bool) {
        let docs = docs as usize;
        if docs <= MAX_RANKED_POOL {
            (docs.max(CANDIDATE_CAP), false)
        } else {
            (MAX_RANKED_POOL, true)
        }
    }

    fn pool_at(
        &self,
        raw: &str,
        expr: &expression::Expression,
        plan: Option<&crate::ai::search_planner::AiSearchPlan>,
        cap: usize,
    ) -> Result<Vec<Uuid>> {
        match plan {
            Some(plan) => self.search_plan(plan, cap),
            // A plain box keeps the path it has always used; the equivalence
            // between the two is pinned by a test.
            None if expr.is_plain() => self.search(raw, cap),
            None => self.search_expression(expr, cap),
        }
    }

    fn collect_ids(
        searcher: &tantivy::Searcher,
        asset_id: Field,
        top: Vec<(f32, tantivy::DocAddress)>,
    ) -> Result<Vec<Uuid>> {
        let mut ids = Vec::with_capacity(top.len());
        for (_, addr) in top {
            let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else {
                tracing::warn!(?addr, "ranked document could not be read from the index");
                continue;
            };
            let Some(raw) = doc.get_first(asset_id).and_then(|v| v.as_str()) else {
                tracing::warn!(?addr, "indexed document carries no asset_id");
                continue;
            };
            match Uuid::parse_str(raw) {
                Ok(id) => ids.push(id),
                // Silently dropping this would make the row unfindable with no
                // trace of why, and the index is a disposable derivative — so
                // the only clue that it went stale is a message like this.
                Err(error) => tracing::warn!(
                    raw,
                    %error,
                    "indexed asset_id is not a UUID; document skipped"
                ),
            }
        }
        Ok(ids)
    }

    /// The 2/3-gram alternative for a term: an OR over the gram fields for
    /// short terms, an AND of per-gram ORs for longer ones (a necessary,
    /// well-ranked approximation of the substring). Returned as ONE query
    /// so it competes as a single `should` alternative next to the word /
    /// fuzzy / pinyin paths instead of constraining them.
    ///
    /// The grams keep spaces (`NgramTokenizer` runs over characters), which is
    /// what makes a quoted phrase a real substring match rather than a mere
    /// AND of its words.
    fn gram_query(&self, term: &str, target: expression::Target) -> Box<dyn Query> {
        let tris = self.f.tris_for(target);
        let n = term.chars().count();
        let gram_shoulds = |gram: &str| {
            tris.iter()
                .map(|f| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(*f, gram),
                            IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>,
                    )
                })
                .collect::<Vec<_>>()
        };
        if n == 2 || n == 3 {
            Box::new(BooleanQuery::new(gram_shoulds(term)))
        } else {
            let chars: Vec<char> = term.chars().collect();
            let mut must: Vec<(Occur, Box<dyn Query>)> = chars
                .windows(3)
                .map(|g| {
                    let gram: String = g.iter().collect();
                    (
                        Occur::Must,
                        Box::new(BooleanQuery::new(gram_shoulds(&gram))) as Box<dyn Query>,
                    )
                })
                .collect();
            if must.is_empty() {
                must.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.f.abbr, "\u{0}no-match"),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ));
            }
            Box::new(BooleanQuery::new(must))
        }
    }

    /// One user term → its ranked alternatives, across every surface.
    fn term_query(&self, term: &str) -> Box<dyn Query> {
        self.term_query_on(term, expression::Target::All)
    }

    /// One user term, restricted to `target`'s surfaces.
    ///
    /// A qualified term keeps the word, fuzzy, prefix and gram paths. Pinyin
    /// and abbreviation matching is now field-scoped: `tag:mao` matches pinyin
    /// from tags only, not from the file name or other surfaces.
    fn term_query_on(&self, term: &str, target: expression::Target) -> Box<dyn Query> {
        let lower = term.to_lowercase();
        let n = term.chars().count();
        let words = self.f.words_for(target);
        let global = target == expression::Target::All;

        let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let push_exact = |shoulds: &mut Vec<(Occur, Box<dyn Query>)>, f: Field, boost: f32| {
            shoulds.push((
                Occur::Should,
                Box::new(tantivy::query::BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(f, &lower),
                        IndexRecordOption::WithFreqs,
                    )),
                    boost,
                )) as Box<dyn Query>,
            ));
        };

        if term.chars().all(|c| c.is_ascii_alphanumeric()) {
            for f in &words {
                push_exact(&mut shoulds, *f, 3.0);
            }
            // Typo tolerance: short stems only get one edit to stay precise.
            if n >= 4 {
                let distance = if n >= 8 { 2 } else { 1 };
                for f in &words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(tantivy::query::BoostQuery::new(
                            Box::new(FuzzyTermQuery::new(
                                Term::from_field_text(*f, &lower),
                                distance,
                                true,
                            )),
                            1.0,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            // Word-start prefix ("sun" → "sunset") and pinyin lookups.
            if n >= 2 {
                for f in &words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(
                            Term::from_field_text(*f, &lower),
                            0,
                            false,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            if global {
                // Unqualified: search the global pinyin/abbr fields (all surfaces)
                shoulds.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(
                        Term::from_field_text(self.f.pinyin, &lower),
                        0,
                        false,
                    )) as Box<dyn Query>,
                ));
                shoulds.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(
                        Term::from_field_text(self.f.abbr, &lower),
                        0,
                        false,
                    )) as Box<dyn Query>,
                ));
            } else if n >= 2 {
                // Field-qualified: search per-surface pinyin if available
                let surface_pinyin = self.f.pinyin_for(target);
                if let Some(py_field) = surface_pinyin {
                    shoulds.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(
                            Term::from_field_text(py_field, &lower),
                            0,
                            false,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            // Grams keep infix substrings findable (`ower` → `flower`).
            if n >= 2 {
                shoulds.push((Occur::Should, self.gram_query(&lower, target)));
            }
        } else {
            // CJK / mixed: whole-term word matches plus 2–3 gram
            // substrings; longer terms AND their 3-grams.
            for f in &words {
                push_exact(&mut shoulds, *f, 3.0);
            }
            shoulds.push((Occur::Should, self.gram_query(&lower, target)));
        }
        Box::new(BooleanQuery::new(shoulds))
    }

    /// All terms ANDed; an empty query matches nothing via a sentinel term.
    fn build_query(&self, text: &str) -> Box<dyn Query> {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut must: Vec<(Occur, Box<dyn Query>)> = cleaned
            .split_whitespace()
            .map(|term| (Occur::Must, self.term_query(term)))
            .collect();
        if must.is_empty() {
            must.push((Occur::Must, self.no_match()));
        }
        Box::new(BooleanQuery::new(must))
    }

    /// Rank the documents an [`expression::Expression`] describes.
    ///
    /// Groups OR with each other; inside a group every positive atom is a
    /// `must` and every `-` atom a `must_not`.
    ///
    /// A group holding only negations matches **nothing**. It could instead be
    /// read as "everything but", and that reading is what a search engine with
    /// a query language would do — but it would also mean a mistyped leading
    /// dash widens a query into the whole library. Refusing to widen is the
    /// contract the box has always had (`store::tests::
    /// search_syntax_characters_stay_literal`), so exclusion needs something
    /// positive to exclude from.
    ///
    /// One group of positive atoms builds the same object
    /// [`build_query`](Self::build_query) does, which is what keeps an
    /// unremarkable query behaving exactly as it always did.
    fn build_expression_query(&self, expr: &expression::Expression) -> Box<dyn Query> {
        let groups = expr
            .groups
            .iter()
            .map(|group| self.build_group_query(group))
            .collect::<Vec<_>>();
        match groups.len() {
            0 => self.no_match(),
            // One disjunct needs no OR around it: the group query *is* the
            // query, which is what makes a plain query byte-identical to the
            // path it replaced.
            1 => groups.into_iter().next().expect("one group"),
            _ => Box::new(BooleanQuery::new(
                groups
                    .into_iter()
                    .map(|group| (Occur::Should, group))
                    .collect(),
            )),
        }
    }

    fn build_group_query(&self, group: &expression::Group) -> Box<dyn Query> {
        if group.atoms.iter().all(|atom| atom.negate) {
            return self.no_match();
        }
        let clauses = group
            .atoms
            .iter()
            .map(|atom| {
                let occur = if atom.negate {
                    Occur::MustNot
                } else {
                    Occur::Must
                };
                // Quoted phrases use positional matching; unquoted terms use
                // the regular ranked-alternatives path.
                let query = if atom.quoted && atom.text.contains(' ') {
                    self.phrase_query_on(&atom.text, atom.target)
                } else {
                    self.term_query_on(&atom.text, atom.target)
                };
                (occur, query)
            })
            .collect();
        Box::new(BooleanQuery::new(clauses))
    }

    /// Build a positional phrase query for a quoted span. The phrase is
    /// tokenized with jieba, and the resulting terms must appear in sequence
    /// in the indexed text. Falls back to n-gram matching if the phrase
    /// tokenizes to a single term (no positional information to enforce).
    fn phrase_query_on(&self, phrase: &str, target: expression::Target) -> Box<dyn Query> {
        let lower = phrase.to_lowercase();
        let words = self.f.words_for(target);

        // Tokenize the phrase with jieba to get the sequence of terms
        let jieba = jieba();
        let tokens: Vec<String> = jieba
            .cut(&lower, true)
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .collect();

        // If the phrase tokenizes to a single term, there's no positional
        // constraint to enforce — fall back to the regular term query.
        if tokens.len() <= 1 {
            return self.term_query_on(&lower, target);
        }

        // Build a PhraseQuery for each surface field, OR them together.
        // PhraseQuery requires terms to appear in sequence with correct positions.
        let mut phrase_shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in &words {
            let terms: Vec<Term> = tokens
                .iter()
                .map(|token| Term::from_field_text(*field, token))
                .collect();
            phrase_shoulds.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
        }

        // Also add n-gram matching as a fallback for partial matches
        phrase_shoulds.push((Occur::Should, self.gram_query(&lower, target)));

        Box::new(BooleanQuery::new(phrase_shoulds))
    }

    /// A term that can never have been indexed, standing in for "no results".
    fn no_match(&self) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.f.abbr, "\u{0}no-match"),
            IndexRecordOption::Basic,
        ))
    }

    /// Build a Tantivy query from an AI search plan. Keywords are ANDed
    /// (Must), synonyms are ORed as optional boosts (Should), and
    /// exclusions are negated (MustNot).
    fn build_plan_query(&self, plan: &crate::ai::search_planner::AiSearchPlan) -> Box<dyn Query> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Keywords: all must match (AND).
        for keyword in &plan.keywords {
            clauses.push((Occur::Must, self.term_query(keyword)));
        }

        // Synonyms: optional boost (OR) — only meaningful when at least one
        // synonym matches.
        for synonym in &plan.synonyms {
            clauses.push((Occur::Should, self.term_query(synonym)));
        }

        // A plan can arrive with nothing positive in it: the planner rejects a
        // plan only when keywords, synonyms, exclusions, filters *and* sort are
        // all empty, so one exclusion and no keywords is a valid plan.
        //
        // Measured on tantivy 0.26, a `BooleanQuery` whose only clauses are
        // `MustNot` answers **nothing** — not "every document minus the
        // matches", which is what the Lucene-flavoured reading of that shape
        // predicts. So this guard is not patching a live wrong answer; it is
        // refusing to depend on that fact, and it makes this path and
        // `build_group_query` reach the same verdict for the same shape by the
        // same words rather than by two engines' semantics.
        if clauses.is_empty() {
            return self.no_match();
        }

        // Exclusions: must NOT match.
        for exclusion in &plan.exclusions {
            clauses.push((Occur::MustNot, self.term_query(exclusion)));
        }

        Box::new(BooleanQuery::new(clauses))
    }
}

// ============================ outbox drain ===================================

/// How many outbox rows one pass of [`drain`] consumes before committing.
///
/// The batch is what the drain's remaining cost is spent on: the queued deletes
/// are batched into one statement, but Tantivy's commit (flush + reader reload)
/// is paid once per batch, so a small batch multiplies a fixed ~150 ms by the
/// number of batches. Measured on 20k assets (`search_smoke --micro 20000`,
/// release):
///
/// | batch | ms/row |
/// |-------|--------|
/// | 500   | 0.509  |
/// | 2000  | 0.139  |
/// | 8000  | 0.045  |
///
/// with a floor of 0.034 ms/row for indexing alone, i.e. 8000 is where the
/// commit overhead stops mattering. It stays honest about the other two bounds:
/// the batch delete binds one parameter per row (8000 ≪ SQLite's 32766 ceiling)
/// and holds the write lock for ~9 ms, well inside the 5 s `busy_timeout` a
/// concurrent backend writer (imports own a second connection) may wait, and a
/// crash mid-batch only costs re-indexing those rows, since the outbox rows are
/// still there.
const DRAIN_BATCH: i64 = 8_000;

/// A drain pass that runs longer than this escalates its log line from
/// debug to warn — the index's closest thing to a slow-query log. One batch
/// of [`DRAIN_BATCH`] lands well under it; a warn means a backlog big enough
/// that the UI thread paid real time on the read path that triggered it.
const SLOW_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Rows waiting in the `search_queue` outbox — how far the index is behind
/// the database. Non-zero is routine in a second process (the rows belong to
/// whoever owns the writer) and is what a read-only CLI handle reports, since
/// it cannot drain them itself.
pub fn pending_count(conn: &Connection) -> Result<u64> {
    let count = crate::store::rows::query_count(conn, "SELECT COUNT(*) FROM search_queue", vec![])?;
    Ok(count.max(0) as u64)
}

/// Flush the `search_queue` outbox into the index: upsert rows whose assets
/// still exist, drop documents for purged ones. Cheap when the queue is empty
/// (one small SELECT), so every search can afford to call it. Lives on the
/// store connection, so both [`crate::library::Library`] and tests drive it.
///
/// A pass that moved rows logs its size and duration — debug normally, warn
/// past [`SLOW_DRAIN`]. These lines are the outbox's only queue-depth signal.
///
/// This is the only place the search index learns about asset or tag writes —
/// the `search_queue` triggers fill the outbox and nothing else touches the
/// index.
///
/// Ordering inside a batch is deliberate: the Tantivy commit lands *before*
/// the queue rows are dropped, so a crash in between leaves the rows queued
/// and the next drain redoes them (indexing is idempotent, and
/// [`TextIndex::index_asset`] also drops a doc whose row has vanished). The
/// converse order would lose index updates silently.
pub fn drain(conn: &Connection, index: &TextIndex) -> Result<()> {
    // A read-only handle owns no writer, so it cannot move rows out of the
    // outbox. Leaving them queued is the point: the next writable open (the
    // app, or a CLI command that got the lock) drains the same backlog.
    if !index.is_writable() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut rows: u64 = 0;
    loop {
        let pending: Vec<(i64, Uuid, bool)> = crate::store::rows::query_map(
            conn,
            "SELECT rowid, asset_id, deleted FROM search_queue LIMIT ?1",
            vec![Value::Integer(DRAIN_BATCH)],
            |row| {
                Ok((
                    crate::store::rows::int(row, 0)?,
                    crate::store::rows::req_uuid(row, 1)?,
                    crate::store::rows::int(row, 2)? != 0,
                ))
            },
        )?;
        if pending.is_empty() {
            break;
        }
        rows += pending.len() as u64;
        let full_batch = pending.len() as i64 == DRAIN_BATCH;

        // `search_queue` has no UNIQUE constraint (duplicate rows are
        // harmless), so the same asset can appear twice in one batch. Collapse
        // it to a single action — `deleted` is AND-ed, so a lone "live" row
        // wins — which keeps exactly one Tantivy op per asset per batch and
        // makes the outcome independent of row order.
        let mut actions: HashMap<Uuid, bool> = HashMap::with_capacity(pending.len());
        for (_, id, deleted) in &pending {
            actions
                .entry(*id)
                .and_modify(|d| *d &= *deleted)
                .or_insert(*deleted);
        }
        for (id, deleted) in &actions {
            if *deleted {
                index.remove_asset(*id)?;
            } else {
                index.index_asset(conn, *id)?;
            }
        }
        index.commit()?;

        // One transaction and one statement for the whole batch. Deleting the
        // rows one by one let each delete autocommit — a fsync each, ~5 ms per
        // row on the 10k benchmark, and ~95% of the drain's total cost. The
        // delete targets the exact rowids consumed, so a row a concurrent
        // writer enqueues for the same asset is left for the next pass.
        //
        // `unchecked_transaction` rather than `Store::transaction` because
        // this function only has a bare `&Connection` — the backend import task
        // drains through its own connection.
        let tx = conn.unchecked_transaction()?;
        let mut sql = String::from("DELETE FROM search_queue WHERE rowid IN (");
        let mut args: Vec<Value> = Vec::with_capacity(pending.len());
        for (i, (rowid, _, _)) in pending.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(Value::Integer(*rowid));
        }
        sql.push(')');
        crate::store::rows::execute(&tx, &sql, args)?;
        tx.commit()?;

        if !full_batch {
            break;
        }
    }
    if rows > 0 {
        let elapsed = started.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let slow = elapsed >= SLOW_DRAIN;
        crate::metrics::note_drain(rows, elapsed, slow);
        if slow {
            tracing::warn!(rows, elapsed_ms, "slow search outbox drain");
        } else {
            tracing::debug!(rows, elapsed_ms, "search outbox drained");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TextIndex;
    use super::{CANDIDATE_CAP, FactTexts, MAX_RANKED_POOL, expression};

    /// A throwaway index directory, named so parallel tests never collide.
    fn temp_index_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "trove-index-test-{}",
            crate::model::new_id().simple()
        ))
    }

    /// How much an uncapped ranked gather costs. Not a pass/fail test — it is
    /// the measurement behind `pool_for`'s shape, kept so the next round can
    /// re-take it instead of re-guessing it:
    /// `cargo test -p trove-core bench_uncapped_gather -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_uncapped_gather() {
        use std::time::Instant;
        let idx = TextIndex::in_ram().unwrap();
        let writer = idx.writer().unwrap();
        for i in 0..100_000usize {
            idx.index_asset_text(
                &writer,
                &uuid::Uuid::new_v4().to_string(),
                &format!("photo-{i:06}-cat.jpg"),
                None,
                None,
                "",
                &FactTexts::default(),
            );
        }
        drop(writer);
        idx.commit().unwrap();
        println!("docs {}", idx.num_docs());
        for cap in [2_000usize, 20_000, 100_000] {
            let t = Instant::now();
            let hits = idx.search("cat", cap).unwrap();
            println!("cap {cap:6} -> {} hits in {:?}", hits.len(), t.elapsed());
        }
    }

    /// The ceiling decision, which is the only place `truncated` is decided for
    /// a filtered search.
    #[test]
    fn a_filtered_gather_is_capped_only_above_the_ceiling() {
        // Below the ceiling the pool is the whole index, so a saturated length
        // is not a hidden match — it means the term matched everything.
        assert_eq!(TextIndex::gather_cap(0), (CANDIDATE_CAP, false));
        assert_eq!(TextIndex::gather_cap(12), (CANDIDATE_CAP, false));
        assert_eq!(TextIndex::gather_cap(50_000), (50_000, false));
        assert_eq!(
            TextIndex::gather_cap(MAX_RANKED_POOL as u64),
            (MAX_RANKED_POOL, false)
        );
        // Past it, the gather is narrower than what exists and says so.
        assert_eq!(
            TextIndex::gather_cap(MAX_RANKED_POOL as u64 + 1),
            (MAX_RANKED_POOL, true)
        );
    }

    /// The guarantee a filtered gather exists to keep: every asset that matches
    /// the term *and* the filter is in the answer, including one the ranking
    /// placed past the fast path's width.
    ///
    /// The weak match is the point — 2004 documents repeat the word and one
    /// mentions it once, so the favourite ranks last and a pool that stopped at
    /// [`CANDIDATE_CAP`] would return a confident zero for `fav:yes`.
    #[test]
    fn a_filtered_search_finds_the_match_that_ranked_past_the_fast_path() {
        use crate::model::{AssetKind, AssetQuery};
        use crate::store::assets;

        let store = crate::store::Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = TextIndex::in_ram().unwrap();
        let writer = idx.writer().unwrap();

        let mut strong = Vec::new();
        for i in 0..2004 {
            let mut a = crate::model::test_asset(
                &format!("kittens-kittens-{i:04}-kittens.png"),
                AssetKind::Image,
                crate::model::new_id(),
            );
            a.description = Some("kittens kittens".into());
            assets::insert(conn, &a).unwrap();
            idx.index_asset_text(
                &writer,
                &a.id.to_string(),
                &a.file_name,
                None,
                a.description.as_deref(),
                "",
                &FactTexts::default(),
            );
            strong.push(a.id);
        }
        // The one asset that both matches the term (weakly, so it ranks last)
        // and carries the filter.
        let mut last =
            crate::model::test_asset("photo-last.png", AssetKind::Image, crate::model::new_id());
        last.description = Some("kittens".into());
        last.is_favorite = true;
        assets::insert(conn, &last).unwrap();
        idx.index_asset_text(
            &writer,
            &last.id.to_string(),
            &last.file_name,
            None,
            last.description.as_deref(),
            "",
            &FactTexts::default(),
        );
        drop(writer);
        idx.commit().unwrap();
        assert!(!strong.is_empty());

        let expr = expression::parse("kittens").into_expression();
        let (pool, ran_out) = idx.pool_for("kittens", &expr, None, true).unwrap();
        assert_eq!(
            pool.len(),
            2005,
            "a filtered gather takes the term's whole result set, not a fixed width"
        );
        assert!(
            !ran_out,
            "nothing was left behind, so the total is a count and not a floor"
        );
        assert_eq!(
            pool.last(),
            Some(&last.id),
            "the setup lost: the weak match no longer ranks last, so this test proves nothing"
        );

        let q = AssetQuery {
            is_favorite: Some(true),
            ..AssetQuery::live()
        };
        let (total, kept) = assets::rank_intersect(conn, &pool, &q).unwrap();
        assert_eq!(kept, vec![last.id]);
        assert_eq!(total, 1, "the asset past the fast path was not found");
    }

    /// The in-RAM index accepts documents and serves them after commit.
    #[test]
    fn in_ram_roundtrip() {
        let idx = TextIndex::in_ram().unwrap();
        idx.index_asset_text(
            &idx.writer().unwrap(),
            "11111111-1111-1111-1111-111111111111",
            "flower.png",
            None,
            None,
            "",
            &FactTexts::default(),
        );
        idx.commit().unwrap();
        assert_eq!(idx.num_docs(), 1);
        assert!(!idx.search("flower", 10).unwrap().is_empty());
    }

    /// One document to index: `(asset_id, file_name, title, description, tags)`.
    type Case = (
        &'static str,
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        &'static str,
    );

    /// A small library: one asset per surface that carries a word nothing else
    /// in the set carries, so a field qualifier's answer is unambiguous.
    fn sample_index() -> TextIndex {
        let idx = TextIndex::in_ram().unwrap();
        let cases: [Case; 4] = [
            (
                "aaaa0000-0000-0000-0000-000000000001",
                "zong.png",
                None,
                None,
                "",
            ),
            (
                "aaaa0000-0000-0000-0000-000000000002",
                "b.png",
                Some("zong"),
                None,
                "",
            ),
            (
                "aaaa0000-0000-0000-0000-000000000003",
                "c.png",
                None,
                Some("zong"),
                "",
            ),
            (
                "aaaa0000-0000-0000-0000-000000000004",
                "d.png",
                None,
                None,
                "zong",
            ),
        ];
        {
            let writer = idx.writer().unwrap();
            for (id, name, title, desc, tags) in cases {
                idx.index_asset_text(&writer, id, name, title, desc, tags, &FactTexts::default());
            }
            // The writer must be released before the reader may see the commit.
            drop(writer);
        }
        idx.commit().unwrap();
        idx
    }

    fn ids_of(idx: &TextIndex, query: &str) -> Vec<String> {
        idx.search(query, 50)
            .unwrap()
            .into_iter()
            .map(|u| u.to_string())
            .collect()
    }

    fn ids_of_expr(idx: &TextIndex, query: &str) -> Vec<String> {
        let expr = crate::search::expression::parse(query).into_expression();
        idx.search_expression(&expr, 50)
            .unwrap()
            .into_iter()
            .map(|u| u.to_string())
            .collect()
    }

    /// The whole point of `is_plain`: a query with no new syntax in it must
    /// rank exactly the way the splitter that predates this module did.
    #[test]
    fn a_plain_expression_answers_exactly_what_the_old_path_did() {
        let idx = sample_index();
        for query in ["zong", "b", "zong b", "png"] {
            assert_eq!(
                ids_of_expr(&idx, query),
                ids_of(&idx, query),
                "{query} diverged from the pre-expression path"
            );
        }
    }

    #[test]
    fn a_field_qualifier_reaches_one_surface_only() {
        let idx = sample_index();
        // Unqualified, `zong` is found wherever it lives.
        assert_eq!(ids_of(&idx, "zong").len(), 4);
        // Qualified, it answers with the one asset that carries it there.
        assert_eq!(
            ids_of_expr(&idx, "name:zong"),
            vec!["aaaa0000-0000-0000-0000-000000000001"]
        );
        assert_eq!(
            ids_of_expr(&idx, "title:zong"),
            vec!["aaaa0000-0000-0000-0000-000000000002"]
        );
        assert_eq!(
            ids_of_expr(&idx, "tag:zong"),
            vec!["aaaa0000-0000-0000-0000-000000000004"]
        );
    }

    #[test]
    fn a_bar_unions_two_groups_and_a_dash_subtracts() {
        let idx = sample_index();
        let union = ids_of_expr(&idx, "name:zong | tag:zong");
        assert_eq!(union.len(), 2, "{union:?}");
        assert!(union.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
        assert!(union.contains(&"aaaa0000-0000-0000-0000-000000000004".to_string()));

        // `png` is on all four; excluding the first-name asset leaves three.
        let minus = ids_of_expr(&idx, "png -name:zong");
        assert_eq!(minus.len(), 3, "{minus:?}");
        assert!(!minus.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
    }

    /// A leading dash is a typo's worth of distance from "show me everything",
    /// so an exclusion with nothing to exclude from answers with nothing. The
    /// box has never widened a match on syntax characters, and this is where
    /// that property could have been lost.
    #[test]
    fn an_exclusion_without_something_positive_matches_nothing() {
        let idx = sample_index();
        assert_eq!(ids_of(&idx, "png").len(), 4);
        assert!(ids_of_expr(&idx, "-name:zong").is_empty());
        // With a positive term to anchor it, the same exclusion subtracts.
        let minus = ids_of_expr(&idx, "png -name:zong");
        assert_eq!(minus.len(), 3, "{minus:?}");
        assert!(!minus.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
    }

    /// The same shape on the AI-planned path, which reaches `BooleanQuery`
    /// through a different door than the hand-typed one above.
    ///
    /// The planner accepts a plan carrying one exclusion and no keywords — it
    /// only rejects a plan where keywords, synonyms, exclusions, filters *and*
    /// sort are all empty. Tantivy 0.26 already answers nothing for an
    /// all-`MustNot` query (verified by disabling the guard and re-running this
    /// assertion: it passes either way), so this pins the *decision*, not a live
    /// wrong answer — it is what keeps the two paths agreeing if that engine
    /// behaviour ever changes.
    #[test]
    fn a_plan_with_nothing_positive_matches_nothing() {
        use crate::ai::search_planner::AiSearchPlan;
        let idx = sample_index();
        assert_eq!(
            idx.search_plan(&AiSearchPlan::default(), 100)
                .unwrap()
                .len(),
            0
        );

        // A term that matches nothing, so an accidental "match all minus X"
        // anywhere in this path would show up as four rows rather than zero.
        let exclusions_only = AiSearchPlan {
            exclusions: vec!["nothing-carries-this".into()],
            ..Default::default()
        };
        assert_eq!(
            ids_of(&idx, "png").len(),
            4,
            "the sample library has four rows to accidentally return"
        );
        assert!(
            idx.search_plan(&exclusions_only, 100).unwrap().is_empty(),
            "an exclusions-only plan returned the whole library"
        );

        // With something positive to subtract from, the exclusion still applies.
        let positive = AiSearchPlan {
            keywords: vec!["png".into()],
            ..Default::default()
        };
        assert_eq!(idx.search_plan(&positive, 100).unwrap().len(), 4);
        let subtract = AiSearchPlan {
            keywords: vec!["png".into()],
            exclusions: vec!["png".into()],
            ..Default::default()
        };
        assert!(idx.search_plan(&subtract, 100).unwrap().is_empty());
    }

    #[test]
    fn a_quoted_phrase_finds_the_substring_it_describes() {
        let idx = TextIndex::in_ram().unwrap();
        {
            let writer = idx.writer().unwrap();
            idx.index_asset_text(
                &writer,
                "bbbb0000-0000-0000-0000-000000000001",
                "summer 2024 beach.png",
                None,
                None,
                "",
                &FactTexts::default(),
            );
            idx.index_asset_text(
                &writer,
                "bbbb0000-0000-0000-0000-000000000002",
                "summer beach.png",
                None,
                None,
                "",
                &FactTexts::default(),
            );
            drop(writer);
        }
        idx.commit().unwrap();

        // Quoted, the words stay one span, so the grams of the whole phrase
        // have to be present: only the first file carries "summer 2024".
        let hits = ids_of_expr(&idx, "\"summer 2024\"");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0], "bbbb0000-0000-0000-0000-000000000001");
    }

    /// A metadata qualifier answers on its own surface and nowhere else. The
    /// decoy carries the very same tokens in its description, so the
    /// unqualified term finds both — and the qualified one only the facts.
    /// This is the v3 fact surface (camera, artist, album, font), which
    /// `extract_fact_texts` fills from the asset's mined metadata.
    #[test]
    fn a_camera_qualifier_answers_only_the_camera_surface() {
        let idx = TextIndex::in_ram().unwrap();
        let camera = "cccc0000-0000-0000-0000-000000000001";
        let decoy = "cccc0000-0000-0000-0000-000000000002";
        let artist = "cccc0000-0000-0000-0000-000000000003";
        let album = "cccc0000-0000-0000-0000-000000000004";
        let font = "cccc0000-0000-0000-0000-000000000005";
        {
            let writer = idx.writer().unwrap();
            idx.index_asset_text(
                &writer,
                camera,
                "canon-eos.png",
                None,
                None,
                "",
                &FactTexts {
                    camera: "Canon EOS R5 ISO 400 f/2.8 1/60s".into(),
                    ..Default::default()
                },
            );
            idx.index_asset_text(
                &writer,
                decoy,
                "decoy.png",
                None,
                Some("canon ryuichi async inter"),
                "",
                &FactTexts::default(),
            );
            idx.index_asset_text(
                &writer,
                artist,
                "artist.png",
                None,
                None,
                "",
                &FactTexts {
                    artist: "Ryuichi Sakamoto".into(),
                    ..Default::default()
                },
            );
            idx.index_asset_text(
                &writer,
                album,
                "album.png",
                None,
                None,
                "",
                &FactTexts {
                    album: "Async".into(),
                    ..Default::default()
                },
            );
            idx.index_asset_text(
                &writer,
                font,
                "font.png",
                None,
                None,
                "",
                &FactTexts {
                    font: "Inter Bold 400".into(),
                    ..Default::default()
                },
            );
            drop(writer);
        }
        idx.commit().unwrap();

        let ids = |query: &str| ids_of_expr(&idx, query);
        assert_eq!(ids("camera:canon"), vec![camera.to_string()]);
        assert_eq!(ids("make:canon"), vec![camera.to_string()], "alias");
        assert_eq!(ids("model:r5"), vec![camera.to_string()], "alias");
        assert_eq!(ids("camera:iso"), vec![camera.to_string()]);
        assert_eq!(ids("artist:ryuichi"), vec![artist.to_string()]);
        assert_eq!(ids("artist:sakamoto"), vec![artist.to_string()]);
        assert_eq!(ids("album:async"), vec![album.to_string()]);
        assert_eq!(ids("font:inter"), vec![font.to_string()]);
        assert_eq!(ids("family:inter"), vec![font.to_string()], "alias");
        // Unqualified, the token is found wherever it lives — the facts
        // surface included, and the description of the decoy beside it.
        let mut both = ids("canon");
        both.sort();
        assert_eq!(both, vec![camera.to_string(), decoy.to_string()]);
        let mut both = ids("ryuichi");
        both.sort();
        assert_eq!(both, vec![decoy.to_string(), artist.to_string()]);
        let mut both = ids("async");
        both.sort();
        assert_eq!(both, vec![decoy.to_string(), album.to_string()]);
        let mut both = ids("inter");
        both.sort();
        assert_eq!(both, vec![decoy.to_string(), font.to_string()]);
        // A fact token nothing else carries is found by the plain term:
        // the composite facts surface is one of the unqualified surfaces.
        assert_eq!(ids("iso"), vec![camera.to_string()]);
    }

    /// Field-scoped pinyin: `tag:mao` answers from the tags' own pinyin
    /// field, and the same syllable in a file name does not answer a tag ask
    /// — nor the reverse. That is the v4 split: before it, pinyin lived only
    /// on the four surfaces concatenated, so a qualified ask either missed
    /// entirely or could be answered by the wrong surface.
    #[test]
    fn a_qualified_pinyin_stays_on_its_own_surface() {
        let idx = TextIndex::in_ram().unwrap();
        let named = "bbbb0000-0000-0000-0000-000000000001";
        let tagged = "bbbb0000-0000-0000-0000-000000000002";
        let catnamed = "bbbb0000-0000-0000-0000-000000000003";
        let titled = "bbbb0000-0000-0000-0000-000000000004";
        {
            let writer = idx.writer().unwrap();
            idx.index_asset_text(
                &writer,
                named,
                "照片.png",
                None,
                None,
                "",
                &FactTexts::default(),
            );
            idx.index_asset_text(
                &writer,
                tagged,
                "b.png",
                None,
                None,
                "猫",
                &FactTexts::default(),
            );
            idx.index_asset_text(
                &writer,
                catnamed,
                "猫.png",
                None,
                None,
                "",
                &FactTexts::default(),
            );
            idx.index_asset_text(
                &writer,
                titled,
                "c.png",
                Some("海边"),
                None,
                "",
                &FactTexts::default(),
            );
            drop(writer);
        }
        idx.commit().unwrap();

        let ids = |query: &str| ids_of_expr(&idx, query);
        // `tag:mao` answers from the tags' pinyin alone: the file name that
        // carries the very same syllable must not answer a tag ask.
        assert_eq!(ids("tag:mao"), vec![tagged.to_string()]);
        assert_eq!(ids("name:mao"), vec![catnamed.to_string()]);
        assert_eq!(ids("name:zhao"), vec![named.to_string()]);
        assert_eq!(ids("name:pian"), vec![named.to_string()]);
        assert_eq!(ids("title:hai"), vec![titled.to_string()]);
        // …and the converse: the title's pinyin does not answer a tag ask.
        assert!(ids("tag:hai").is_empty(), "{:?}", ids("tag:hai"));
        // Unqualified, the syllable is found on every surface that carries it.
        let mut both = ids("mao");
        both.sort();
        assert_eq!(both, vec![tagged.to_string(), catnamed.to_string()]);
    }

    /// A pool that comes back exactly full is gathered again, wider, when
    /// structured filters are about to reject rows from it.
    ///
    /// The cap is applied *before* the SQL narrowing, so every asset past the
    /// cap is invisible to a filtered search — including one that matches both
    /// the term and the filter, which is the answer the caller came for. The
    /// counts here are what that looks like from the outside: `CANDIDATE_CAP +
    /// 5` matching documents, and the two asks that differ only in whether
    /// something will reject rows afterwards.
    #[test]
    fn a_saturated_pool_is_gathered_wider_only_when_it_will_be_filtered() {
        let count = CANDIDATE_CAP + 5;
        let idx = TextIndex::in_ram().unwrap();
        {
            let writer = idx.writer().unwrap();
            for _ in 0..count {
                idx.index_asset_text(
                    &writer,
                    &crate::model::new_id().to_string(),
                    "flood.png",
                    None,
                    None,
                    "",
                    &FactTexts::default(),
                );
            }
            drop(writer);
        }
        idx.commit().unwrap();
        assert_eq!(idx.num_docs() as usize, count);

        let expr = expression::parse("flood").into_expression();
        let (pooled, ran_out) = idx.pool_for("flood", &expr, None, true).unwrap();
        assert!(
            pooled.len() > CANDIDATE_CAP,
            "a filtered ask reached past the cap and found {}",
            pooled.len()
        );
        assert!(
            !ran_out,
            "the library holds {count} and the pool saw all of them"
        );

        // With nothing to reject rows, the wider gather would buy nothing: the
        // caller shows a page out of this pool either way. What it must get is
        // the honest flag, so the number beside the grid reads as a floor.
        let (plain, ran_out) = idx.pool_for("flood", &expr, None, false).unwrap();
        assert_eq!(plain.len(), CANDIDATE_CAP);
        assert!(ran_out, "a saturated unfiltered pool says so");
    }

    /// A held writer lock is an error, not a panic — the desktop app and the
    /// CLI are allowed to be open on the same library at the same time.
    #[test]
    fn open_refuses_a_held_writer_lock_without_panicking() {
        let dir = temp_index_dir();
        let owner = TextIndex::open(&dir).expect("the first open owns the index");
        assert!(owner.is_writable());

        let second = TextIndex::open(&dir);
        assert!(
            second.is_err(),
            "a second writable handle must be refused while the first holds the lock",
        );

        // ... and the read-only constructor is what the second process uses.
        let reader = TextIndex::open_read_only(&dir).expect("read-only open succeeds");
        assert!(!reader.is_writable());
        assert!(reader.commit().is_err(), "reads-only handle refuses writes");
        assert!(reader.search("anything", 10).unwrap().is_empty());

        drop(owner);
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read-only handle builds an index when there is none at all — the
    /// state a CLI finds a freshly created library in — but never repairs one
    /// that exists, because that directory may belong to another process.
    #[test]
    fn open_read_only_builds_only_an_absent_index() {
        let fresh = temp_index_dir();
        let idx = TextIndex::open_read_only(&fresh).unwrap();
        assert!(idx.is_writable(), "there was no index to conflict with");
        assert_eq!(idx.num_docs(), 0);
        drop(idx);
        assert!(fresh.join("meta.json").is_file(), "an index was created");
        let _ = std::fs::remove_dir_all(&fresh);

        let stale = temp_index_dir();
        drop(TextIndex::open(&stale).unwrap());
        std::fs::write(stale.join("trove-index-version"), "99").unwrap();
        let before = directory_entries(&stale);

        let reader = TextIndex::open_read_only(&stale).unwrap();
        assert!(!reader.is_writable());
        assert_eq!(reader.num_docs(), 0);
        assert_eq!(
            before,
            directory_entries(&stale),
            "a read-only open must leave an existing index untouched",
        );
        let _ = std::fs::remove_dir_all(&stale);
    }

    fn directory_entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
