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
    pub fn search(&self, conn: &Connection, query: &[f32], cap: usize) -> Result<Vec<VectorMatch>> {
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

// -- hybrid ranking ---------------------------------------------------------

/// A search term's embedding, held by the caller for as long as that term is
/// the current one.
///
/// It carries the term it was computed for, because embedding a query is an
/// HTTP round trip: the workspace renders the text ranking immediately and
/// re-runs the query when the vector lands, and a response that arrives
/// after the user moved on must be dropped rather than fused into an answer
/// to a different question. That comparison is why this is a struct and not
/// a bare `Vec<f32>`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryVector {
    /// The search term this vector embeds (trimmed, matching the search box's
    /// own normalization).
    pub text: String,
    /// Model identity the vector came from; must equal the index's.
    pub model: String,
    /// The space the *stored* rows live in — for a text embedder, `Text`.
    pub space: EmbeddingSpace,
    /// The query vector, un-normalized ([`VectorIndex::search`] normalizes).
    pub vector: Vec<f32>,
}

/// Reciprocal-rank-fusion constant from the original paper (Cormack et al.,
/// 2009). 60 is what most implementations ship: large enough that the top
/// rank does not run away with the result, small enough that rank 1 still
/// clearly outweighs rank 10.
pub const RRF_K: f64 = 60.0;

/// Fuse two rank-ordered id lists into one, by reciprocal rank.
///
/// Each list contributes `1 / (RRF_K + rank)` per id (rank counted from 1),
/// an id present in both lists collects both terms, and the result is ordered
/// by that sum. Scores are deliberately not used: BM25 and cosine similarity
/// live on different scales, and calibrating them against each other is
/// precisely the tuning problem RRF removes.
///
/// Ties break on the id, so the order is deterministic — a hash map's
/// iteration order is not, and the grid pages through this list across
/// frames.
pub fn reciprocal_rank_fusion(text: &[uuid::Uuid], vector: &[uuid::Uuid]) -> Vec<uuid::Uuid> {
    use std::collections::HashMap;

    let mut scores: HashMap<uuid::Uuid, f64> = HashMap::with_capacity(text.len() + vector.len());
    for list in [text, vector] {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }

    let mut ranked: Vec<(uuid::Uuid, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.into_iter().map(|(id, _)| id).collect()
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
    use crate::model::test_asset;
    use crate::model::{EmbeddingSpace, NewEmbedding};
    use crate::store::Store;
    use crate::store::assets;
    use crate::store::embeddings;
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
    fn rrf_lets_agreement_beat_a_single_first_place() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        // `a` is second in both legs while `b` and `c` each lead one. Two
        // modest votes outweigh one strong one — the property that makes RRF
        // worth using instead of blending normalized scores.
        let fused = reciprocal_rank_fusion(&[b, a], &[c, a]);
        assert_eq!(fused[0], a, "{fused:?}");
        assert_eq!(fused.len(), 3);
        assert!(fused.contains(&b) && fused.contains(&c));
    }

    #[test]
    fn rrf_orders_deterministically_and_keeps_the_union() {
        let ids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
        let text = &ids[..5];
        let vector = &ids[3..8];
        let fused = reciprocal_rank_fusion(text, vector);

        assert_eq!(fused.len(), 8, "the union, with no duplicates");
        assert_eq!(fused, reciprocal_rank_fusion(text, vector), "stable order");
        // Ids in both legs outrank the tails that appear in only one.
        for id in &ids[3..5] {
            let pos = fused.iter().position(|x| x == id).unwrap();
            assert!(pos < 5, "shared id ranked too low at {pos}");
        }
    }

    #[test]
    fn rrf_of_a_single_leg_is_that_leg() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(reciprocal_rank_fusion(&[a, b], &[]), vec![a, b]);
        assert_eq!(reciprocal_rank_fusion(&[], &[b, a]), vec![b, a]);
        assert!(reciprocal_rank_fusion(&[], &[]).is_empty());
    }

    #[test]
    fn ranks_by_cosine_and_truncates() {
        let (store, a, b, c) = seed();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        assert_eq!(
            index.len(store.conn()).unwrap(),
            3,
            "lazy-loaded on first use"
        );

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
            image_index
                .search(store.conn(), &[1.0, 0.0], 10)
                .unwrap()
                .is_empty(),
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
        assert!(
            index
                .search(store.conn(), &[1.0, 0.0, 0.0], 5)
                .unwrap()
                .is_empty()
        );
    }
}
