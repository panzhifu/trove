//! The Tantivy schema: every text surface, its tokenizers, and the field
//! handles the rest of the module addresses them by.

use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions};

use super::TOK_ABBR;
use super::TOK_JIEBA;
use super::TOK_PINYIN;
use super::TOK_RAW;
use super::TOK_TRI;
use super::expression;

/// The indexed fields, resolved once at open.
#[derive(Clone, Copy)]
pub(super) struct Fields {
    pub(super) asset_id: Field,
    pub(super) name_w: Field,
    pub(super) title_w: Field,
    pub(super) desc_w: Field,
    pub(super) tags_w: Field,
    pub(super) name_tri: Field,
    pub(super) title_tri: Field,
    pub(super) desc_tri: Field,
    pub(super) tags_tri: Field,
    pub(super) pinyin: Field,
    pub(super) abbr: Field,
    // Per-surface pinyin fields for field-qualified pinyin matching
    pub(super) name_pinyin: Field,
    pub(super) title_pinyin: Field,
    pub(super) desc_pinyin: Field,
    pub(super) tags_pinyin: Field,
    // Metadata fact fields — indexed from the asset's `extra` JSON.
    pub(super) facts_w: Field,
    pub(super) facts_tri: Field,
    pub(super) camera_w: Field,
    pub(super) camera_tri: Field,
    pub(super) artist_w: Field,
    pub(super) artist_tri: Field,
    pub(super) album_w: Field,
    pub(super) album_tri: Field,
    pub(super) font_w: Field,
    pub(super) font_tri: Field,
    pub(super) audio_w: Field,
    pub(super) audio_tri: Field,
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
    pub(super) fn words_for(&self, target: expression::Target) -> Vec<Field> {
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
    pub(super) fn tris_for(&self, target: expression::Target) -> Vec<Field> {
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
    pub(super) fn pinyin_for(&self, target: expression::Target) -> Option<Field> {
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

pub(super) fn indexed_text(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
    )
}

/// Index options for jieba-tokenized fields that support phrase queries.
/// Phrase queries need position information to enforce term ordering.
pub(super) fn indexed_text_with_positions(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

pub(super) fn indexed_basic(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_stored().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::Basic),
    )
}

impl super::TextIndex {
    pub(super) fn schema() -> Schema {
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
}
