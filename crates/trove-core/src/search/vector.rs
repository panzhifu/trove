//! Vector similarity search: an in-memory cache of one model's embeddings,
//! scored by brute-force dot product.
//!
//! The `asset_embeddings` table is the source of truth; this index is a
//! disposable view of it, exactly as the Tantivy index is a disposable view
//! of the asset rows. Every search re-checks a cheap table fingerprint
//! (row count + last update) and reloads on drift, so a background backfill
//! or a deleted asset is picked up without any cross-thread invalidation.
//!
//! Brute force is deliberate at this stage: a desktop library holds thousands
//! to low-hundreds-of-thousands of assets, and a dot product over unit
//! vectors is a few tens of milliseconds at the top of that range. The
//! interface (query in, ranked [`VectorMatch`] out) is what an ANN structure
//! (HNSW, usearch) would implement later, should a library grow past it.

use std::cell::RefCell;
use std::rc::Rc;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::model::{EmbeddingSpace, VectorMatch, normalized};
use crate::store::embeddings;

/// How many ranked candidates one vector lookup contributes before the SQL
/// filters narrow them — same role as text search's [`crate::search::CANDIDATE_CAP`],
/// and generous for the same reason: semantic queries ("a red car in the
/// snow") can plausibly match a large fraction of the library.
pub const VECTOR_CANDIDATE_CAP: usize = 1000;

/// One model+space's vectors held in memory, refreshed on drift.
///
/// Cheap to clone and thread-confined like [`crate::search::TextIndex`]:
/// the UI thread owns it, and interior mutability hides the reloads.
#[derive(Clone)]
pub struct VectorIndex {
    inner: Rc<RefCell<State>>,
}

struct State {
    model: String,
    space: EmbeddingSpace,
    /// `(asset_id, unit_vector)` pairs straight out of the table.
    entries: Vec<(uuid::Uuid, Vec<f32>)>,
    /// The table fingerprint these entries were loaded from.
    fingerprint: (u64, String),
    /// Set once the entries have actually been loaded; an unloaded index
    /// (empty library, no backfill yet) is empty rather than broken.
    loaded: bool,
}

impl VectorIndex {
    /// An index for one model+space that loads itself on first use.
    pub fn new(model: impl Into<String>, space: EmbeddingSpace) -> Self {
        Self {
            inner: Rc::new(RefCell::new(State {
                model: model.into(),
                space,
                entries: Vec::new(),
                fingerprint: (0, String::new()),
                loaded: false,
            })),
        }
    }

    /// The model identity this index serves (matches `asset_embeddings.model`).
    pub fn model(&self) -> String {
        self.inner.borrow().model.clone()
    }

    pub fn space(&self) -> EmbeddingSpace {
        self.inner.borrow().space
    }

    /// How many vectors are cached. Triggers a load/reload if the table has
    /// drifted, so the answer is always current.
    pub fn len(&self, conn: &Connection) -> Result<usize> {
        self.ensure_fresh(conn)?;
        Ok(self.inner.borrow().entries.len())
    }

    /// True when the table holds no vectors for this model+space.
    pub fn is_empty(&self, conn: &Connection) -> Result<bool> {
        Ok(self.len(conn)? == 0)
    }

    /// The `cap` closest assets to `query`, scored by cosine similarity.
    ///
    /// `query` must be the same dimension as the stored vectors and comes
    /// from the same model that named this index — a query vector is never
    /// comparable across models or spaces. Returns matches sorted by
    /// descending score; the caller narrows them with the SQL filters.
    pub fn search(
        &self,
        conn: &Connection,
        query: &[f32],
        cap: usize,
    ) -> Result<Vec<VectorMatch>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_fresh(conn)?;
        let query = normalized(query)?;

        let state = self.inner.borrow();
        let mut matches: Vec<VectorMatch> = Vec::new();
        for (asset_id, vector) in &state.entries {
            if vector.len() != query.len() {
                // A row written by a differently-configured provider; the
                // backfill refuses to mix dims, so this only survives a
                // hand-edited table. Skip it rather than mis-score it.
                continue;
            }
            let score = query.iter().zip(vector).map(|(q, v)| q * v).sum::<f32>();
            matches.push(VectorMatch {
                asset_id: *asset_id,
                score,
            });
        }
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(cap);
        Ok(matches)
    }

    /// Reload from the table unconditionally (tests, explicit invalidation).
    pub fn reload(&self, conn: &Connection) -> Result<()> {
        let (model, space) = {
            let state = self.inner.borrow();
            (state.model.clone(), state.space)
        };
        let entries = embeddings::snapshot(conn, &model, space)?;
        let fingerprint = embeddings::fingerprint(conn, &model, space)?;
        let mut state = self.inner.borrow_mut();
        state.entries = entries;
        state.fingerprint = fingerprint;
        state.loaded = true;
        Ok(())
    }

    /// Load once, or reload when the table's fingerprint has moved.
    fn ensure_fresh(&self, conn: &Connection) -> Result<()> {
        let (model, space) = {
            let state = self.inner.borrow();
            (state.model.clone(), state.space)
        };
        let fp = embeddings::fingerprint(conn, &model, space)?;
        let stale = {
            let state = self.inner.borrow();
            !state.loaded || state.fingerprint != fp
        };
        if stale {
            self.reload(conn)?;
        }
        Ok(())
    }
}

/// Reject a query whose dimension disagrees with what a model's rows carry —
/// the caller-facing guard the backfill enforces on the write side.
pub fn check_dim(query: &[f32], stored: usize) -> Result<()> {
    if query.len() != stored {
        return Err(Error::Validation(format!(
            "query vector has {} dimensions, model rows have {stored}",
            query.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EmbeddingSpace, NewEmbedding};
    use crate::store::Store;
    use crate::store::assets;
    use crate::store::embeddings;
    use crate::model::test_asset;
    use uuid::Uuid;

    fn emb(asset_id: Uuid, vector: Vec<f32>) -> NewEmbedding {
        NewEmbedding {
            asset_id,
            model: "test-model".into(),
            space: EmbeddingSpace::Text,
            vector,
            source_hash: "h".into(),
        }
    }

    /// e1 = [1, 0], e2 = [0.6, 0.8], e3 = [-1, 0] (normalized).
    fn seed() -> (Store, uuid::Uuid, uuid::Uuid, uuid::Uuid) {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mk = |name: &str| {
            let a = test_asset(name, crate::model::AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &a).unwrap();
            a.id
        };
        let (a, b, c) = (mk("a.png"), mk("b.png"), mk("c.png"));
        embeddings::upsert(conn, &emb(a, vec![1.0, 0.0])).unwrap();
        embeddings::upsert(conn, &emb(b, vec![0.6, 0.8])).unwrap();
        embeddings::upsert(conn, &emb(c, vec![-1.0, 0.0])).unwrap();
        (store, a, b, c)
    }

    #[test]
    fn ranks_by_cosine_and_truncates() {
        let (store, a, b, c) = seed();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        assert_eq!(index.len(store.conn()).unwrap(), 3, "lazy-loaded on first use");

        // A query pointing at +x: exact match first, opposite vector last.
        let hits = index.search(store.conn(), &[1.0, 0.0], 10).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].asset_id, a);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
        assert_eq!(hits[1].asset_id, b, "0.8 cosine ranks second");
        assert!((hits[1].score - 0.6).abs() < 1e-6);
        assert_eq!(hits[2].asset_id, c);
        assert!((hits[2].score + 1.0).abs() < 1e-6);

        // The cap truncates from the bottom.
        let hits = index.search(store.conn(), &[1.0, 0.0], 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].asset_id, a);

        // Query vectors need not be normalized on the way in.
        let hits = index.search(store.conn(), &[9.0, 0.0], 1).unwrap();
        assert_eq!(hits[0].asset_id, a);
    }

    #[test]
    fn picks_up_table_drift_without_external_invalidation() {
        let (store, _a, _b, _c) = seed();
        let conn = store.conn();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        assert_eq!(index.len(conn).unwrap(), 3);

        // A backfill lands a new row; the index has not been told.
        let d = test_asset("d.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &d).unwrap();
        embeddings::upsert(conn, &emb(d.id, vec![0.0, 1.0])).unwrap();
        assert_eq!(
            index.len(conn).unwrap(),
            4,
            "the fingerprint check reloads on the next touch"
        );

        // Deleting the asset cascades its row away — also picked up.
        assets::delete(conn, d.id).unwrap();
        assert_eq!(index.len(conn).unwrap(), 3);
    }

    #[test]
    fn spaces_do_not_leak_into_each_other() {
        let (store, a, _b, _c) = seed();
        let image_index = VectorIndex::new("test-model", EmbeddingSpace::Image);
        assert!(
            image_index.search(store.conn(), &[1.0, 0.0], 10).unwrap().is_empty(),
            "only text rows were written"
        );
        // …and an image row is invisible to the text index until written
        // under the text space.
        embeddings::upsert(
            store.conn(),
            &NewEmbedding {
                space: EmbeddingSpace::Image,
                ..emb(a, vec![1.0, 0.0])
            },
        )
        .unwrap();
        assert_eq!(image_index.len(store.conn()).unwrap(), 1);
    }

    #[test]
    fn empty_index_and_empty_query_are_empty_not_errors() {
        let store = Store::in_memory().unwrap();
        let index = VectorIndex::new("nothing", EmbeddingSpace::Text);
        assert!(index.is_empty(store.conn()).unwrap());
        assert!(index.search(store.conn(), &[], 5).unwrap().is_empty());
        assert!(index.search(store.conn(), &[1.0], 5).unwrap().is_empty());
    }

    #[test]
    fn dim_mismatch_is_a_clean_error() {
        let (store, _a, _b, _c) = seed();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        assert!(check_dim(&[1.0, 0.0], 2).is_ok());
        assert!(check_dim(&[1.0, 0.0, 0.0], 2).is_err());
        // A 3-dim query scores nothing (every row is skipped by length).
        assert!(index.search(store.conn(), &[1.0, 0.0, 0.0], 5).unwrap().is_empty());
    }
}
