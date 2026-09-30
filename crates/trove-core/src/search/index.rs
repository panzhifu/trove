//! The index handle: opening (with the writer-lock discipline), the write
//! paths that feed it from SQLite, and the lifecycle around them.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use rusqlite::Connection;
use tantivy::tokenizer::{
    LowerCaser, NgramTokenizer, RawTokenizer, TextAnalyzer, WhitespaceTokenizer,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::media::text;
use crate::media::thumb;
use crate::store::assets;

use super::facts::{FactTexts, extract_fact_texts};
use super::schema::Fields;
use super::tokenizer::jieba;
use super::tokenizer::{JiebaTokenizer, pinyin_of};
use super::{INDEX_VERSION, WRITER_HEAP};
use super::{TOK_ABBR, TOK_JIEBA, TOK_PINYIN, TOK_RAW, TOK_TRI};

/// The Tantivy index. Lives in the same single-threaded `Rc` world as the
/// store connection; `Rc` fields keep `Library`'s `Clone` derive valid.
///
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
    pub(super) writer: Option<Rc<RefCell<IndexWriter>>>,
    pub(super) reader: IndexReader,
    pub(super) f: Fields,
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
    pub(super) fn writer(&self) -> Result<std::cell::RefMut<'_, IndexWriter>> {
        match self.writer.as_ref() {
            Some(writer) => Ok(writer.borrow_mut()),
            None => Err(Error::Validation(
                "search index is read-only: another process holds its writer lock".into(),
            )),
        }
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
            body_w: field("body_words"),
            body_tri: field("body_tri"),
            color_w: field("color_words"),
            color_tri: field("color_tri"),
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
        self.index_asset_in(conn, asset_id, None)
    }

    /// [`index_asset`] with the library root, which unlocks the one surface
    /// that lives in a file rather than the row: a text asset's body.
    pub fn index_asset_in(
        &self,
        conn: &Connection,
        asset_id: Uuid,
        root: Option<&Path>,
    ) -> Result<()> {
        let writer = self.writer()?;
        let Some(a) = assets::get(conn, asset_id)? else {
            self.remove_asset_with(&writer, asset_id);
            return Ok(());
        };
        let tags = assets::tags_for_index(conn, asset_id)?;
        let facts = extract_fact_texts(&a.facts, a.source_url.as_deref());
        let body = text_body(&a, root);
        self.index_asset_text(
            &writer,
            &asset_id.to_string(),
            &a.file_name,
            a.title.as_deref(),
            a.description.as_deref(),
            &tags,
            &facts,
            &body,
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
    pub(super) fn index_asset_text(
        &self,
        writer: &IndexWriter,
        id: &str,
        file_name: &str,
        title: Option<&str>,
        description: Option<&str>,
        tags: &str,
        facts: &FactTexts,
        body: &str,
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
        // The colour space: the profile's own name on the word surface, the
        // compact form on the gram surface — see `color_compact`.
        if !facts.color.is_empty() {
            doc.add_text(self.f.color_w, &facts.color);
            doc.add_text(self.f.color_tri, super::facts::color_compact(&facts.color));
        }
        // The text body, when the asset has a readable file: what a `.md`
        // note *says* becomes searchable, not just what it is called.
        if !body.is_empty() {
            doc.add_text(self.f.body_w, body);
            doc.add_text(self.f.body_tri, body);
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
}

/// A text-family asset's indexed body: the file's own beginning, decoded by
/// the same encoding ladder the viewer uses. Everything that can go wrong
/// here (no root, unresolvable blob, missing file, undecodable bytes)
/// degrades to an empty body — the asset stays indexed by its name and
/// facts, which is what every caller before this feature ever saw.
fn text_body(asset: &crate::model::Asset, root: Option<&Path>) -> String {
    let Some(root) = root else {
        return String::new();
    };
    if !text::is_text_ext(&asset.ext) {
        return String::new();
    }
    let Some(blob) = thumb::blob_path(root, asset) else {
        return String::new();
    };
    text::read(&blob, text::MAX_INDEX_BYTES)
        .map(|c| c.text)
        .unwrap_or_default()
}
