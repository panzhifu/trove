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
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, Query, TermQuery};
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
use crate::store::assets;

/// Bump when the schema or the query semantics change incompatibly: the
/// version file beside the index is checked on open and a mismatch wipes
/// the directory for a full rebuild.
///
/// 2: tantivy 0.22 -> 0.26, which changes the on-disk index format. Without
/// the bump the wipe would still happen (via `Index::open_in_dir` failing),
/// but only after a failed open; this makes the rebuild deterministic.
const INDEX_VERSION: u32 = 2;
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
            cursor = from + word.chars().next().map_or(1, |c| c.len_utf8()).max(1);
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
}

fn indexed_text(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
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
        for name in ["name_words", "title_words", "desc_words", "tags_words"] {
            builder.add_text_field(name, indexed_text(TOK_JIEBA));
        }
        for name in ["name_tri", "title_tri", "desc_tri", "tags_tri"] {
            builder.add_text_field(name, indexed_text(TOK_TRI));
        }
        builder.add_text_field("pinyin", indexed_text(TOK_PINYIN));
        builder.add_text_field("pinyin_abbr", indexed_text(TOK_ABBR));
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
        self.index_asset_text(
            &writer,
            &asset_id.to_string(),
            &a.file_name,
            a.title.as_deref(),
            a.description.as_deref(),
            &tags,
        );
        Ok(())
    }

    /// Low-level upsert from already-resolved text. Takes the writer as an
    /// argument rather than borrowing it, so [`TextIndex::writer`] stays the
    /// single place a read-only handle is refused.
    fn index_asset_text(
        &self,
        writer: &IndexWriter,
        id: &str,
        file_name: &str,
        title: Option<&str>,
        description: Option<&str>,
        tags: &str,
    ) {
        let searchable = format!(
            "{} {} {} {}",
            file_name,
            title.unwrap_or_default(),
            description.unwrap_or_default(),
            tags
        );
        let (pinyin, abbr) = pinyin_of(&searchable);

        let mut doc = TantivyDocument::new();
        doc.add_text(self.f.asset_id, id);
        doc.add_text(self.f.name_w, file_name);
        doc.add_text(self.f.name_tri, file_name);
        if let Some(title) = title {
            doc.add_text(self.f.title_w, title);
            doc.add_text(self.f.title_tri, title);
        }
        if let Some(desc) = description {
            doc.add_text(self.f.desc_w, desc);
            doc.add_text(self.f.desc_tri, desc);
        }
        doc.add_text(self.f.tags_w, tags);
        doc.add_text(self.f.tags_tri, tags);
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

    fn collect_ids(
        searcher: &tantivy::Searcher,
        asset_id: Field,
        top: Vec<(f32, tantivy::DocAddress)>,
    ) -> Result<Vec<Uuid>> {
        let mut ids = Vec::with_capacity(top.len());
        for (_, addr) in top {
            let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else {
                continue;
            };
            if let Some(s) = doc.get_first(asset_id).and_then(|v| v.as_str())
                && let Ok(id) = Uuid::parse_str(s)
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The 2/3-gram alternative for a term: an OR over the gram fields for
    /// short terms, an AND of per-gram ORs for longer ones (a necessary,
    /// well-ranked approximation of the substring). Returned as ONE query
    /// so it competes as a single `should` alternative next to the word /
    /// fuzzy / pinyin paths instead of constraining them.
    fn gram_query(&self, term: &str) -> Box<dyn Query> {
        let tris = [
            self.f.name_tri,
            self.f.title_tri,
            self.f.desc_tri,
            self.f.tags_tri,
        ];
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

    /// One user term → its ranked alternatives. Every alternative is a
    /// `should` clause; the term itself becomes a `must` of that group.
    fn term_query(&self, term: &str) -> Box<dyn Query> {
        let lower = term.to_lowercase();
        let n = term.chars().count();
        let words = [self.f.name_w, self.f.title_w, self.f.desc_w, self.f.tags_w];

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
            for f in words {
                push_exact(&mut shoulds, f, 3.0);
            }
            // Typo tolerance: short stems only get one edit to stay precise.
            if n >= 4 {
                let distance = if n >= 8 { 2 } else { 1 };
                for f in words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(tantivy::query::BoostQuery::new(
                            Box::new(FuzzyTermQuery::new(
                                Term::from_field_text(f, &lower),
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
                for f in words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(
                            Term::from_field_text(f, &lower),
                            0,
                            false,
                        )) as Box<dyn Query>,
                    ));
                }
            }
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
            // Grams keep infix substrings findable (`ower` → `flower`).
            if n >= 2 {
                shoulds.push((Occur::Should, self.gram_query(&lower)));
            }
        } else {
            // CJK / mixed: whole-term word matches plus 2–3 gram
            // substrings; longer terms AND their 3-grams.
            for f in words {
                push_exact(&mut shoulds, f, 3.0);
            }
            shoulds.push((Occur::Should, self.gram_query(&lower)));
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
            must.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f.abbr, "\u{0}no-match"),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        Box::new(BooleanQuery::new(must))
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

        // Exclusions: must NOT match.
        for exclusion in &plan.exclusions {
            clauses.push((Occur::MustNot, self.term_query(exclusion)));
        }

        if clauses.is_empty() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f.abbr, "\u{0}no-match"),
                    IndexRecordOption::Basic,
                )),
            ));
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

    /// A throwaway index directory, named so parallel tests never collide.
    fn temp_index_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "trove-index-test-{}",
            crate::model::new_id().simple()
        ))
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
        );
        idx.commit().unwrap();
        assert_eq!(idx.num_docs(), 1);
        assert!(!idx.search("flower", 10).unwrap().is_empty());
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
