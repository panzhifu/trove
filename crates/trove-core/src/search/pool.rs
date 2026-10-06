//! Search execution: turning a parsed request into ranked asset ids, with
//! the gather ceiling that keeps a broad term from flooding a filtered query.

use tantivy::TantivyDocument;
use tantivy::collector::TopDocs;
use tantivy::schema::{Field, Value as _};
use uuid::Uuid;

use crate::error::{Error, Result};

use super::expression;
use super::index::TextIndex;
use super::{CANDIDATE_CAP, MAX_RANKED_POOL};

impl TextIndex {
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
    pub(super) fn gather_cap(docs: u64) -> (usize, bool) {
        // Written as an explicit clamp, not `if docs <= MAX_RANKED_POOL {
        // docs.max(CANDIDATE_CAP) } else { MAX_RANKED_POOL }`. The branchy form
        // lowers to `select(icmp, const, umax)`, which rustc 1.98.0/1.99.0
        // miscompiles at opt-level >= 1: LLVM's InstCombine drops the upper
        // clamp and returns the raw `docs` past the ceiling (a minimal repro
        // returns 200001 where 200000 is required). Clamping lowers to
        // `umin(umax(..), ..)`, which folds correctly, and keeps the crate
        // optimised *and* incremental.
        let over = docs > MAX_RANKED_POOL as u64;
        let width = (docs as usize).clamp(CANDIDATE_CAP, MAX_RANKED_POOL);
        (width, over)
    }

    pub(super) fn pool_at(
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

    pub(super) fn collect_ids(
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
}
