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

mod drain;
mod facts;
mod index;
mod pool;
mod query;
mod schema;
mod tokenizer;

pub use drain::{drain, pending_count};
pub use index::TextIndex;

#[cfg(test)]
mod tests;

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
