//! Browsed-view dispatch: one description of "what the workspace is showing"
//! compiled to the matching paged query.
//!
//! The workspace panel can browse five mutually exclusive views — recently
//! viewed, full-text search, a smart collection, the trash, and the plain
//! (collection / tag / folder filtered) listing. Which view wins and how the
//! grid filters compose with it is a domain rule, so it lives here next to
//! the queries; the UI only builds the [`BrowseContext`] and renders the
//! [`Page`] that comes back.

use rusqlite::Connection;
use uuid::Uuid;

use super::{assets, smart, smart_collections, view_history};
use crate::error::{Error, Result};
use crate::model::{
    AspectPreset, Asset, AssetKind, AssetQuery, AssetSort, Orientation, Page, QueryCondition,
    ResolutionBand,
};
use crate::search::vector::{self, QueryVector, VECTOR_CANDIDATE_CAP, VectorIndex};

/// Which search legs a browse may run.
///
/// The legs degrade downward: the local full-text index is the base, the
/// vector ranking is fused into it, and the AI plan rewrites the query before
/// either runs. A leg that is off — or whose endpoint is unconfigured or
/// failed — contributes nothing, so the search still returns the layers below
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchTiers {
    /// L1: the local Tantivy full-text index.
    pub full_text: bool,
    /// L2: embedding vector search, fused into L1 by reciprocal rank.
    pub semantic: bool,
    /// L3: the LLM query planner. It rewrites the query; it adds no ranking
    /// of its own.
    pub ai: bool,
}

impl Default for SearchTiers {
    /// Local full-text only — the behaviour before the tiers existed.
    fn default() -> Self {
        Self {
            full_text: true,
            semantic: false,
            ai: false,
        }
    }
}

/// How the workspace grid is currently browsing the library.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowseContext {
    /// The browsed collection (`None` = all assets).
    pub collection: Option<Uuid>,
    /// Browse the trash instead of a collection.
    pub in_trash: bool,
    /// Browse the recently-viewed history.
    pub in_recent: bool,
    /// Browse the live results of this smart collection.
    pub smart: Option<Uuid>,
    /// Only assets carrying this tag.
    pub tag: Option<Uuid>,
    /// Only assets imported from this source-path prefix.
    pub folder: Option<String>,
    /// Active full-text search term. Overrides the other views when set.
    pub search: String,
    /// An AI-generated search plan. When set, it drives the Tantivy query
    /// instead of [`Self::search`] — keywords become AND, synonyms become
    /// OR, exclusions become NOT. The two are mutually exclusive: a plan
    /// overrides the raw text.
    pub ai_plan: Option<crate::ai::search_planner::AiSearchPlan>,
    /// The search term's embedding when the caller has one — the vector leg
    /// of the hybrid ranking, scored against the index passed to
    /// [`Self::run`].
    ///
    /// Hybrid ranking is opt-in *by data*, never by a flag: with no vector
    /// here, no index, a vector left over from a different term, or a
    /// model/space that disagrees with the index, the text ranking stands
    /// alone — exactly the pre-hybrid behaviour.
    pub vector: Option<QueryVector>,
    /// Which search legs this browse may run. See [`SearchTiers`]. Defaults
    /// to full-text only, so a caller that does not set it keeps the old
    /// behaviour.
    pub tiers: SearchTiers,
    /// Grid filters. Every view honours them — the trash and the recent
    /// list included — so the filter bar means the same thing wherever it
    /// is shown.
    pub kind: Option<AssetKind>,
    pub is_favorite: bool,
    pub orientation: Option<Orientation>,
    /// Media aspect-ratio preset the dimensions must fall into. Composes
    /// with `orientation` (a 2.35:1 cover is also a landscape).
    pub aspect: Option<AspectPreset>,
    /// Resolution band the longer edge must fall into. Composes with both
    /// shape filters, which are about proportions and say nothing about size.
    pub resolution: Option<ResolutionBand>,
    /// Minimum star rating (unrated assets match nothing).
    pub min_rating: Option<u8>,
    pub ext: Option<String>,
    /// Listing sort (ignored by the live search, which sorts by
    /// relevance).
    pub sort: AssetSort,
    pub sort_desc: bool,
    /// Rows to skip in the window this browse fetches, alongside the `limit`
    /// argument of [`Self::run`].
    ///
    /// The grid pages by fetching the next window and appending, rather than
    /// re-fetching everything up to its cursor: the latter made every page
    /// repeat the whole query *and* re-stat every row it already had, and the
    /// store's window bound turned that into a hard ceiling on how many assets
    /// one could reach at all. Zero — the default — is every caller that has
    /// one window: the CLI, the smart-rule evaluation, the trash.
    pub offset: u64,
}

impl BrowseContext {
    /// Run the paged query for this view. `limit` caps the page (the grid's
    /// pagination cursor); the recent view treats it as an id cap too.
    ///
    /// `vector` is the in-memory index holding the stored embeddings — the
    /// second leg of a hybrid search. It is only consulted when
    /// [`Self::vector`] carries a vector for the *current* term; see that
    /// field for what "no hybrid" means.
    ///
    /// This is the one-shot entry point: it freezes the listing and takes its
    /// first window. A caller that pages — the workspace grid — holds the
    /// [`BrowseSession`] and asks it for windows instead, because the ranking
    /// must not run again per page.
    pub fn run(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
    ) -> Result<Page<Asset>> {
        let window = limit.map(|l| l as usize);
        self.snapshot(conn, text, vector, true)?
            .page(conn, text, self.offset as usize, window)
    }

    /// Like [`run`](Self::run), but skips the exact COUNT where the view
    /// supports it: the returned total is a lower bound. Rapid refreshes
    /// use this and overlay a cached exact total (see the workspace's data
    /// pass); the recent view has no COUNT to skip and the search view
    /// always counts (both are cheap or user-initiated).
    pub fn run_without_count(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
    ) -> Result<Page<Asset>> {
        let window = limit.map(|l| l as usize);
        self.snapshot(conn, text, vector, false)?
            .page(conn, text, self.offset as usize, window)
    }

    /// Freeze this browse into a listing the caller can page through.
    ///
    /// Nothing here takes a window: a ranked listing gathers its whole id order
    /// once, and a set is windowed by SQL each time it is asked.
    pub fn snapshot(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        vector: Option<&VectorIndex>,
        count: bool,
    ) -> Result<BrowseSession> {
        // Every paged browse feeds the query metrics; past the slow threshold
        // the note itself logs the warn.
        let started = std::time::Instant::now();
        let result = self.snapshot_inner(conn, text, vector, count);
        crate::metrics::note_query(started.elapsed());
        result
    }

    /// The vector leg of a hybrid search, fused with the text ranking.
    ///
    /// `None` means "there is nothing to fuse", and the caller then uses the
    /// text ranking as it stands. Every reason to decline is a data mismatch
    /// rather than an error: no query vector, no index, a vector computed for
    /// a term the user has already typed past, or a model/space that
    /// disagrees with the index — the same comparability contract the store
    /// enforces on the write side.
    fn fused_candidates(
        &self,
        conn: &Connection,
        index: Option<&VectorIndex>,
        text_ranked: &[Uuid],
    ) -> Result<Option<Vec<Uuid>>> {
        let (Some(query), Some(index)) = (self.vector.as_ref(), index) else {
            return Ok(None);
        };
        if query.text != self.search.trim() || query.vector.is_empty() {
            return Ok(None);
        }
        if index.model() != query.model || index.space() != query.space {
            return Ok(None);
        }
        // A failure here must not sink the search: the text ranking is
        // already a complete answer, so a broken vector leg degrades to it.
        let hits = match index.search(conn, &query.vector, VECTOR_CANDIDATE_CAP) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(%error, "vector leg of a hybrid search failed; text ranking stands");
                return Ok(None);
            }
        };
        if hits.is_empty() {
            return Ok(None);
        }
        crate::metrics::note_vector_search();
        let ranked: Vec<Uuid> = hits.into_iter().map(|m| m.asset_id).collect();
        Ok(Some(vector::reciprocal_rank_fusion(text_ranked, &ranked)))
    }

    fn snapshot_inner(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        vector: Option<&VectorIndex>,
        count: bool,
    ) -> Result<BrowseSession> {
        // The box is read as an expression before anything else: its
        // qualifiers are *filters* and only its terms *rank*. So `ext:png`
        // alone is a narrowed browse rather than a search, while `猫 ext:png`
        // searches for 猫 among the pngs.
        //
        // The filters are read out by value, not taken: `Expression::is_plain`
        // consults them, and emptying the field would make a qualified query
        // look plain enough to hand to the old path with its raw `ext:png`
        // still in the text — a search for a word nobody typed.
        let expr = crate::search::expression::parse(&self.search).into_expression();
        let has_terms = expr.groups.iter().any(|g| !g.atoms.is_empty());
        let has_filters = !expr.filters.is_empty();
        let mut conditions = expr.filters.clone();
        // Nothing that describes *where* an asset came from narrows the trash:
        // the browsed collection, the folder prefix and a typed `path:` are all
        // containers the user cannot see while the trash is open, so a lingering
        // one would silently shrink a list whose whole point is to hold
        // everything deleted — a wrong answer with no visible cause.
        if self.in_trash {
            conditions.retain(|c| !matches!(c, QueryCondition::Path { .. }));
        }
        let conditions = conditions.as_slice();
        // A box that is non-empty yet yielded nothing to rank *and* nothing to
        // filter on contained nothing but syntax characters. It stays a search,
        // which then matches nothing: falling back to the plain browse would let
        // a stray `"` empty the search box and list the whole library.
        let noise_only = !self.search.trim().is_empty() && !has_terms && !has_filters;
        let search_active = (self.tiers.full_text || self.tiers.semantic)
            && !self.in_trash
            && !self.in_recent
            && (has_terms || noise_only);

        if self.in_recent {
            // Recently viewed: ids ordered by last view time. That order lives
            // in the history table, so the whole list is the listing — the same
            // candidate-driven shape the text ranking uses, and the same reason
            // it is worth freezing (history is capped, so this is bounded).
            let ids = view_history::recent_ids(conn, view_history::HISTORY_CAP)?;
            let q = self.filter_query(conditions);
            let (_, ranked) = assets::rank_intersect(conn, &ids, &q)?;
            Ok(BrowseSession::ranked(ranked))
        } else if search_active {
            // L3: the plan participates only when its tier is on *and* the box
            // holds a plain term. Expression syntax is the user having already
            // stated the structure they meant; handing `tag:猫 | 狗` to the
            // planner would discard it.
            let plan = self
                .tiers
                .ai
                .then_some(self.ai_plan.as_ref())
                .flatten()
                .filter(|_| expr.is_plain());

            // The filters are assembled *before* the pool is gathered: whether
            // they reject rows decides how wide the gather has to be, and a plan
            // contributes conditions of its own, so it goes in first.
            let mut q = self.filter_query(conditions);
            if let Some(plan) = plan {
                apply_plan_filters(plan, &mut q);
            }
            let narrowed = q.rejects_rows();

            // L1: the local full-text ranking. With the tier off this is an
            // empty list, and the vector leg (if any) stands alone — RRF of
            // one list is that list.
            let (text_ranked, ran_out) = if self.tiers.full_text {
                text.pool_for(&self.search, &expr, plan, narrowed)?
            } else {
                (Vec::new(), false)
            };

            // L2: fuse the vector ranking in, but only when the tier is on, the
            // caller supplied a vector for this exact term, and the box is one
            // sentence at all — a query with a qualifier or a disjunction has
            // no single text to embed, so the leg declines rather than rank
            // against a paraphrase the user did not type.
            let candidates = if self.tiers.semantic && expr.is_plain() {
                match self.fused_candidates(conn, vector, &text_ranked)? {
                    Some(fused) => fused,
                    None => text_ranked,
                }
            } else {
                text_ranked
            };

            let (_, ranked) = assets::rank_intersect(conn, &candidates, &q)?;
            // `total` counted a pool that had run out, so it is a floor: what
            // the library holds is at least this, and possibly more.
            Ok(BrowseSession::ranked(ranked).with_truncated(ran_out))
        } else if let Some(sid) = self.smart {
            let Some(sc) = smart_collections::get(conn, sid)? else {
                return Err(Error::NotFound("smart_collection"));
            };
            let node = smart::node_from_json(&sc.query)?;
            let kind = self.kind;
            let favorite = self.is_favorite.then_some(true);
            let filters = self.smart_grid_filters(conditions);
            // The rule tree is a set, so SQL windows it; only its total is
            // gathered here, once, instead of a COUNT per page.
            let total = if count {
                Some(smart::count(
                    conn,
                    Some(text),
                    &node,
                    kind,
                    favorite,
                    Some(&filters),
                )?)
            } else {
                None
            };
            Ok(BrowseSession {
                total,
                truncated: false,
                listing: Listing::Smart {
                    node,
                    kind,
                    favorite,
                    filters,
                },
            })
        } else {
            let q = self.filter_query(conditions);
            let total = if count {
                Some(assets::count(conn, &q)?)
            } else {
                None
            };
            Ok(BrowseSession {
                total,
                truncated: false,
                listing: Listing::Set(q),
            })
        }
    }
}

/// One browsed listing, frozen so its pages cost a slice instead of a query.
///
/// The grid used to re-run the whole browse for every window it appended: for a
/// ranked view that meant the text leg and its `id IN (…)` intersection again,
/// per page, for an answer that cannot change inside one listing. A session is
/// owned by the view that opened it and dropped when that view changes — it is
/// a snapshot, not a cache, and the caller decides when it goes stale.
#[derive(Debug, Clone)]
pub struct BrowseSession {
    /// `None` when the listing was frozen without counting: the total is then
    /// whatever the caller already knows, never this session's silence read as
    /// an empty library.
    total: Option<u64>,
    /// `total` is a floor — the ranked pool ran out before it had seen
    /// everything matching.
    truncated: bool,
    listing: Listing,
}

#[derive(Debug, Clone)]
enum Listing {
    /// The listing *is* this ordered id list: a page is a slice of it.
    Ranked(Vec<Uuid>),
    /// The listing is a set, so SQL windows it. Gathering every id of a
    /// hundred-thousand-row library to serve one 200-row page would cost more
    /// than the page itself does, which is why sets are not frozen.
    Set(AssetQuery),
    /// A smart collection: the rule tree and the narrowing on top of it, both
    /// windowed by SQL.
    Smart {
        node: crate::model::SmartNode,
        kind: Option<AssetKind>,
        favorite: Option<bool>,
        filters: AssetQuery,
    },
}

impl BrowseSession {
    fn ranked(ids: Vec<Uuid>) -> Self {
        let total = ids.len() as u64;
        Self {
            total: Some(total),
            truncated: false,
            listing: Listing::Ranked(ids),
        }
    }

    fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// The exact total, or `None` if this listing was frozen without counting.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// Whether the listing is known to be deeper than what it reports.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// How many rows the listing holds, when it holds them itself. `None` for a
    /// set, whose size SQL decides per window.
    pub fn len(&self) -> Option<usize> {
        match &self.listing {
            Listing::Ranked(ids) => Some(ids.len()),
            _ => None,
        }
    }

    /// Whether a ranked listing is empty; `false` for a set, which has to be
    /// asked before it can answer.
    pub fn is_empty(&self) -> bool {
        self.len().is_some_and(|len| len == 0)
    }

    /// Materialize one window of the listing.
    ///
    /// `text` is needed because a smart rule's own text condition is answered by
    /// the index at query time; the other two listings ignore it.
    pub fn page(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        offset: usize,
        window: Option<usize>,
    ) -> Result<Page<Asset>> {
        let items = match &self.listing {
            Listing::Ranked(ids) => {
                if offset >= ids.len() {
                    Vec::new()
                } else {
                    let end = window.map_or(ids.len(), |w| offset.saturating_add(w).min(ids.len()));
                    assets::by_ids(conn, &ids[offset..end])?
                }
            }
            Listing::Set(q) => {
                let q = AssetQuery {
                    limit: window.map(|w| w as u32),
                    offset: offset as u64,
                    ..q.clone()
                };
                assets::query_without_count(conn, &q)?.items
            }
            Listing::Smart {
                node,
                kind,
                favorite,
                filters,
            } => {
                let page = smart::SmartPage {
                    kind: *kind,
                    favorite: *favorite,
                    limit: window.map(|w| w as u32),
                    offset: offset as u64,
                    filters: Some(filters.clone()),
                };
                let ids = smart::evaluate_filtered_without_count(conn, Some(text), node, page)?;
                assets::by_ids(conn, &ids.items)?
            }
        };
        Ok(Page {
            // A session that did not count reports the window it filled, which
            // is the same lower bound `run_without_count` always returned.
            total: self.total.unwrap_or(items.len() as u64),
            items,
            truncated: self.truncated,
        })
    }
}

impl BrowseContext {
    /// The structured filters of this browse, as an [`AssetQuery`].
    ///
    /// One definition shared by every dispatch that has to honour them — the
    /// SQL browse, the text ranking and the recent list — so a filter cannot
    /// mean one thing in one view and something else in the next. The
    /// candidate-driven paths bring their own ids and use this for its
    /// conditions only; paging is never part of it, since a session answers
    /// that for every listing the same way.
    ///
    /// `conditions` are the ones the search box's field-qualifier grammar
    /// stated (`ext:png`, `-kind:video`, `path:/data`, `rating:3`). They ride
    /// the same conjunction as the toolbar filters, for the same reason: a
    /// qualified filter and a clicked filter have to narrow alike.
    ///
    /// Two filters stay off in the trash: the current collection and the
    /// source folder. Neither has a control the user can see while the trash
    /// is open, so a lingering one would silently narrow a list whose whole
    /// point is to hold everything deleted — a wrong answer with no visible
    /// cause.
    fn filter_query(&self, conditions: &[QueryCondition]) -> AssetQuery {
        let container = !self.in_trash;
        AssetQuery {
            collection_id: if container { self.collection } else { None },
            source_path_prefix: if container { self.folder.clone() } else { None },
            tag_ids: self.tag.map(|t| vec![t]).unwrap_or_default(),
            kind: self.kind,
            is_favorite: self.is_favorite.then_some(true),
            orientation: self.orientation,
            aspect: self.aspect,
            resolution: self.resolution,
            min_rating: self.min_rating,
            ext: self.ext.clone(),
            conditions: conditions.to_vec(),
            is_trashed: self.in_trash,
            sort: self.sort,
            sort_desc: self.sort_desc,
            // Paging belongs to the session, never to this: a listing stores
            // the set, and each window says where in it it starts.
            ..Default::default()
        }
    }

    /// The grid filters a smart listing has to apply inside its own statement,
    /// as a query with every container predicate dropped.
    ///
    /// A smart collection *is* the container, so narrowing it by the open
    /// folder, tag or collection would redefine which assets the collection
    /// holds — that is a different question than "of these, which ones fit the
    /// toolbar filters". `kind` and `favorite` are missing for the same reason:
    /// [`smart::SmartPage`] carries them as its own two terms, and stating them
    /// twice would let one of the two drift.
    fn smart_grid_filters(&self, conditions: &[QueryCondition]) -> AssetQuery {
        AssetQuery {
            orientation: self.orientation,
            aspect: self.aspect,
            resolution: self.resolution,
            min_rating: self.min_rating,
            ext: self.ext.clone(),
            conditions: conditions.to_vec(),
            ..Default::default()
        }
    }
}

/// Apply an AI search plan's structured filters to an [`AssetQuery`]. Only
/// the fields the query understands are mapped; numeric filters (width,
/// height, duration) fall through to post-filtering in the search path.
fn apply_plan_filters(plan: &crate::ai::search_planner::AiSearchPlan, q: &mut AssetQuery) {
    for filter in &plan.filters {
        match filter.field {
            crate::ai::search_planner::PlanFilterField::Format => {
                if !filter.values.is_empty() {
                    // A plan names extensions the way a user types them, which
                    // is a set — and `AssetQuery::ext` holds one value, so
                    // joining them produced `ext = 'png,jpg'` and matched
                    // nothing. They go where a typed `ext:png` goes instead,
                    // and `fold` merges them into one `IN` list.
                    for ext in &filter.values {
                        q.conditions.push(QueryCondition::Ext {
                            values: vec![ext.trim().trim_start_matches('.').to_lowercase()],
                            negate: filter.exclude,
                        });
                    }
                    q.conditions = QueryCondition::fold(std::mem::take(&mut q.conditions));
                }
            }
            crate::ai::search_planner::PlanFilterField::Rating => {
                // Take the highest minimum rating from the plan.
                let min_from_plan = filter
                    .values
                    .iter()
                    .filter_map(|v| v.parse::<u8>().ok())
                    .max();
                if let Some(min) = min_from_plan {
                    q.min_rating = Some(q.min_rating.map_or(min, |existing| existing.max(min)));
                }
            }
            crate::ai::search_planner::PlanFilterField::Favorite => {
                let want_favorite = filter
                    .values
                    .first()
                    .is_some_and(|v| v == "true" || v == "1");
                if want_favorite && !filter.exclude {
                    q.is_favorite = Some(true);
                }
            }
            // Tag, Width, Height, DurationMs need separate handling — tags
            // require UUID resolution, dimensions need SQL expressions. These
            // are applied as post-filters in the search path.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use crate::store::Store;

    /// The grammar has to survive the whole path — parse, rank, then filter —
    /// not just the parser and the index in isolation.
    #[test]
    fn the_search_box_grammar_narrows_a_browse() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut png = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        png.title = Some("sunset shot".into());
        let mut jpg = test_asset("b.jpg", AssetKind::Image, Uuid::new_v4());
        jpg.title = Some("sunset frame".into());
        let mut clip = test_asset("c.mp4", AssetKind::Video, Uuid::new_v4());
        clip.title = Some("sunset reel".into());
        for asset in [&png, &jpg, &clip] {
            assets::insert(conn, asset).unwrap();
        }
        let idx = crate::search::TextIndex::in_ram().unwrap();
        for asset in [&png, &jpg, &clip] {
            idx.index_asset(conn, asset.id).unwrap();
        }
        idx.commit().unwrap();

        let ids = |box_text: &str| {
            let ctx = BrowseContext {
                search: box_text.into(),
                ..Default::default()
            };
            let mut page: Vec<Uuid> = ctx
                .run(conn, &idx, None, None)
                .unwrap()
                .items
                .into_iter()
                .map(|a| a.id)
                .collect();
            page.sort();
            page
        };

        // Unqualified: all three titles match "sunset".
        assert_eq!(ids("sunset").len(), 3);
        // A qualifier alone is a filter over the browse, not a search.
        assert_eq!(ids("ext:jpg"), vec![jpg.id]);
        // A qualifier and a term compose: rank, then narrow.
        assert_eq!(ids("sunset ext:jpg"), vec![jpg.id]);
        // Several values of one qualifier OR; a second, negated one subtracts.
        let mut either = vec![jpg.id, png.id];
        either.sort();
        assert_eq!(ids("sunset ext:jpg ext:png"), either);
        let mut not_video = vec![jpg.id, png.id];
        not_video.sort();
        assert_eq!(ids("sunset -kind:video"), not_video);
        // The disjunction is a union of two narrowed sets.
        let mut union = vec![clip.id, png.id];
        union.sort();
        assert_eq!(ids("ext:mp4 | ext:png"), union);
        // A box of nothing but syntax matches nothing rather than falling back
        // to the full listing.
        assert!(ids("\"").is_empty());
        assert!(ids("--").is_empty());
    }

    /// `path:` describes where an asset came from, so it means nothing while the
    /// trash is open — the same rule the folder prefix already follows.
    /// Several formats in one plan filter are a set. `AssetQuery::ext` holds a
    /// single value, so the first attempt at this joined them and every such
    /// plan answered with an empty grid.
    #[test]
    fn a_plan_naming_several_formats_asks_for_any_of_them() {
        use crate::ai::search_planner::{AiSearchPlan, PlanFilter, PlanFilterField};
        let plan = AiSearchPlan {
            keywords: vec!["sunset".into()],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![PlanFilter {
                field: PlanFilterField::Format,
                values: vec!["png".into(), ".JPG".into()],
                ranges: vec![],
                exclude: false,
            }],
            sort: None,
        };
        let mut q = AssetQuery::default();
        apply_plan_filters(&plan, &mut q);
        assert_eq!(
            q.conditions,
            vec![QueryCondition::Ext {
                values: vec!["png".into(), "jpg".into()],
                negate: false
            }],
            "folded into one list, and normalised like a typed `ext:`"
        );
    }

    #[test]
    fn a_path_qualifier_stays_out_of_the_trash() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut asset = test_asset("gone.png", AssetKind::Image, Uuid::new_v4());
        asset.facts.source_path = Some("/data/gone.png".into());
        assets::insert(conn, &asset).unwrap();
        assets::set_trashed(conn, asset.id, true).unwrap();
        let idx = crate::search::TextIndex::in_ram().unwrap();

        let browse = |search: &str| {
            let ctx = BrowseContext {
                search: search.into(),
                in_trash: true,
                ..Default::default()
            };
            ctx.run(conn, &idx, None, None).unwrap().total
        };

        assert_eq!(browse(""), 1, "the trash holds the asset");
        assert_eq!(
            browse("path:/nowhere"),
            1,
            "a `path:` the user cannot see must not empty the trash"
        );
        // Every other qualifier still applies in the trash — it describes the
        // asset, not a container.
        assert_eq!(browse("path:/data ext:jpg"), 0);
    }

    #[test]
    fn hybrid_search_fuses_a_vector_leg_into_the_text_ranking() {
        use crate::model::{EmbeddingSpace, NewEmbedding};
        use crate::store::embeddings;

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut shot = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        shot.title = Some("sunset shot".into());
        assets::insert(conn, &shot).unwrap();
        let mut beach = test_asset("b.png", AssetKind::Image, Uuid::new_v4());
        beach.title = Some("beach walk".into());
        assets::insert(conn, &beach).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, shot.id).unwrap();
        idx.index_asset(conn, beach.id).unwrap();
        idx.commit().unwrap();

        // Only `beach` carries a vector, and it points straight at the query
        // — a hit the text leg cannot see on its own.
        embeddings::upsert(
            conn,
            &NewEmbedding {
                asset_id: beach.id,
                model: "test-model".into(),
                space: EmbeddingSpace::Text,
                vector: vec![1.0, 0.0],
                source_hash: "h".into(),
            },
        )
        .unwrap();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        let query = |text: &str, model: &str| QueryVector {
            text: text.into(),
            model: model.into(),
            space: EmbeddingSpace::Text,
            vector: vec![1.0, 0.0],
        };
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext {
                search: "sunset".into(),
                // This test is about the fused ranking, so the vector tier is
                // on; the default is off (full-text only).
                tiers: SearchTiers {
                    semantic: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            mutate(&mut c);
            c
        };

        // Text alone: only the asset whose title matches.
        let page = ctx(&|_| {}).run(conn, &idx, None, Some(&index)).unwrap();
        assert_eq!(page.total, 1, "no query vector ⇒ the text ranking stands");
        assert_eq!(page.items[0].id, shot.id);

        // With the term's vector, the second leg pulls its own hit in.
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("sunset", "test-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 2, "the vector leg contributes its own hit");
        let ids: Vec<Uuid> = page.items.iter().map(|a| a.id).collect();
        assert!(ids.contains(&shot.id) && ids.contains(&beach.id), "{ids:?}");

        // A vector for a term the user has typed past is ignored …
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("bicycle", "test-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1, "a stale query vector must not fuse");

        // … and so is one from another model (the comparability contract).
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("sunset", "other-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1, "a model mismatch must not fuse");
    }

    #[test]
    fn search_tiers_gate_each_leg() {
        use crate::ai::search_planner::AiSearchPlan;
        use crate::model::{EmbeddingSpace, NewEmbedding};
        use crate::store::embeddings;

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut shot = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        shot.title = Some("sunset shot".into());
        assets::insert(conn, &shot).unwrap();
        let mut beach = test_asset("b.png", AssetKind::Image, Uuid::new_v4());
        beach.title = Some("beach walk".into());
        assets::insert(conn, &beach).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, shot.id).unwrap();
        idx.index_asset(conn, beach.id).unwrap();
        idx.commit().unwrap();

        // The vector points at `beach` only.
        embeddings::upsert(
            conn,
            &NewEmbedding {
                asset_id: beach.id,
                model: "test-model".into(),
                space: EmbeddingSpace::Text,
                vector: vec![1.0, 0.0],
                source_hash: "h".into(),
            },
        )
        .unwrap();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        let vector = QueryVector {
            text: "sunset".into(),
            model: "test-model".into(),
            space: EmbeddingSpace::Text,
            vector: vec![1.0, 0.0],
        };

        // L1 only (the default): the text match alone, even though a vector
        // for this exact term is available.
        let page = BrowseContext {
            search: "sunset".into(),
            vector: Some(vector.clone()),
            ..Default::default()
        }
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1, "semantic off ⇒ the text ranking stands");
        assert_eq!(page.items[0].id, shot.id);

        // L1 + L2: the vector leg adds its own hit.
        let page = BrowseContext {
            search: "sunset".into(),
            vector: Some(vector.clone()),
            tiers: SearchTiers {
                full_text: true,
                semantic: true,
                ai: false,
            },
            ..Default::default()
        }
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 2, "semantic on ⇒ fused");

        // L2 only: full-text off, the vector ranking stands alone.
        let page = BrowseContext {
            search: "sunset".into(),
            vector: Some(vector),
            tiers: SearchTiers {
                full_text: false,
                semantic: true,
                ai: false,
            },
            ..Default::default()
        }
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, beach.id, "vector alone ranks its own hit");

        // L3 off ignores a plan that is present …
        let plan = AiSearchPlan {
            keywords: vec!["beach".into()],
            ..Default::default()
        };
        let page = BrowseContext {
            search: "sunset".into(),
            ai_plan: Some(plan.clone()),
            ..Default::default()
        }
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(
            page.items[0].id, shot.id,
            "ai off ⇒ the raw term drives the search"
        );

        // … and uses it when the tier is on.
        let page = BrowseContext {
            search: "sunset".into(),
            ai_plan: Some(plan),
            tiers: SearchTiers {
                full_text: true,
                semantic: false,
                ai: true,
            },
            ..Default::default()
        }
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(
            page.items[0].id, beach.id,
            "ai on ⇒ the plan drives the search"
        );
    }

    /// A filter typed into the box, and a filter clicked on the bar, narrow a
    /// smart collection *inside* its statement.
    ///
    /// Applied to the rows a page already returned, they shrink the list
    /// without shrinking the COUNT: the header then reports a set the grid can
    /// never deliver, and paging toward that number re-runs a query whose answer
    /// no longer moves. Both routes are asserted because they entered the view
    /// through different fields and only one of them was ever applied.
    #[test]
    fn a_narrowing_filter_counts_against_a_smart_collections_total() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let png = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        let jpg = test_asset("b.jpg", AssetKind::Image, Uuid::new_v4());
        let mp4 = test_asset("c.mp4", AssetKind::Video, Uuid::new_v4());
        for asset in [&png, &jpg, &mp4] {
            assets::insert(conn, asset).unwrap();
        }
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let sc = smart_collections::create(
            conn,
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "images".into(),
                query: serde_json::json!({
                    "op": "match", "field": "kind", "value": "image"
                }),
                position: 0,
            },
        )
        .unwrap();
        let browse = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext {
                smart: Some(sc.id),
                ..Default::default()
            };
            mutate(&mut c);
            c.run(conn, &idx, None, None).unwrap()
        };

        assert_eq!(browse(&|_| {}).total, 2, "the collection holds two");

        // A typed qualifier: the total follows the list.
        let page = browse(&|c: &mut BrowseContext| c.search = "ext:jpg".into());
        assert_eq!(page.total, 1, "the total counts what can be shown");
        assert_eq!(page.items[0].id, jpg.id);

        // A toolbar filter: same rule, different field.
        let page = browse(&|c: &mut BrowseContext| c.ext = Some("png".into()));
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, png.id);

        // A window past the narrowed set is empty, which is what lets the grid
        // stop paging instead of asking again for rows that are not there.
        let page = BrowseContext {
            smart: Some(sc.id),
            search: "ext:jpg".into(),
            offset: 1,
            ..Default::default()
        }
        .run(conn, &idx, Some(1), None)
        .unwrap();
        assert!(page.items.is_empty(), "one row, and it is behind us");
    }

    /// Consecutive windows of one listing cover it exactly once.
    ///
    /// The grid appends a window instead of re-fetching everything up to its
    /// cursor, so a row that fell between two windows would simply never appear,
    /// and a row in both would appear twice — both invisible in a view that
    /// scrolls. The smart branch is included because it windows through its own
    /// `LIMIT`, not through the shared one.
    #[test]
    fn consecutive_windows_of_a_browse_cover_the_listing_once() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        for ix in 0..5 {
            let asset = test_asset(&format!("a{ix}.png"), AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &asset).unwrap();
        }
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let sc = smart_collections::create(
            conn,
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "images".into(),
                query: serde_json::json!({
                    "op": "match", "field": "kind", "value": "image"
                }),
                position: 0,
            },
        )
        .unwrap();

        let windowed = |offset: u64, limit: u32, smart: bool| {
            let ctx = BrowseContext {
                smart: smart.then_some(sc.id),
                offset,
                ..Default::default()
            };
            ctx.run(conn, &idx, Some(limit), None)
                .unwrap()
                .items
                .into_iter()
                .map(|a| a.id)
                .collect::<Vec<_>>()
        };
        for smart in [false, true] {
            let whole = windowed(0, 100, smart);
            let mut stitched = Vec::new();
            for offset in [0u64, 2, 4] {
                stitched.extend(windowed(offset, 2, smart));
            }
            assert_eq!(
                stitched,
                whole,
                "windows of a {} listing must meet without gaps or repeats",
                if smart { "smart" } else { "plain" }
            );
        }
    }

    /// A window bigger than the store will fetch is refused by name.
    ///
    /// It used to be clamped in silence, which read as a smaller library.
    #[test]
    fn a_window_bigger_than_the_store_serves_is_an_error() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let error = BrowseContext::default()
            .run(conn, &idx, Some(assets::MAX_PAGE + 1), None)
            .unwrap_err();
        assert!(
            error.to_string().contains("20000"),
            "the refusal names the bound: {error}"
        );
    }

    /// A session's pages come from the listing it froze, not from wherever the
    /// library has since got to.
    ///
    /// This is the property the grid needed: appending a page must not shuffle
    /// the rows already on screen, and it is what makes the second page cost a
    /// slice instead of a re-ranking. The write lands in the *index* as well, so
    /// a fresh snapshot has to see it — otherwise this test would only be
    /// proving that a later read is stale, which is not the point.
    #[test]
    fn a_session_pages_the_listing_it_froze() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let mut seeded = Vec::new();
        for ix in 0..3 {
            let asset = test_asset(&format!("sunset{ix}.png"), AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &asset).unwrap();
            idx.index_asset(conn, asset.id).unwrap();
            seeded.push(asset.id);
        }
        idx.commit().unwrap();

        let search = BrowseContext {
            search: "sunset".into(),
            ..Default::default()
        };
        let session = search
            .snapshot(conn, &idx, None, true)
            .expect("a ranked listing freezes");
        assert_eq!(session.total(), Some(3));

        // A fourth match arrives after the freeze.
        let late = test_asset("sunset3.png", AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &late).unwrap();
        idx.index_asset(conn, late.id).unwrap();
        idx.commit().unwrap();

        let first = session.page(conn, &idx, 0, Some(2)).unwrap();
        let second = session.page(conn, &idx, 2, Some(2)).unwrap();
        let ids: Vec<Uuid> = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|a| a.id)
            .collect();
        assert_eq!(ids.len(), 3, "the frozen listing held three rows");
        assert!(
            !ids.contains(&late.id),
            "the row added after the freeze is not in it"
        );
        assert_eq!(first.total, 3, "the total is the frozen listing's");

        // … and a fresh snapshot sees four, which is what says the assertion
        // above is about the freeze and not about a stale read.
        let fresh = search.snapshot(conn, &idx, None, true).unwrap();
        assert_eq!(fresh.total(), Some(4));
    }

    /// A session frozen without counting reports a lower bound, never a zero
    /// read as an empty library.
    #[test]
    fn an_uncounted_session_says_it_did_not_count() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        for ix in 0..4 {
            let asset = test_asset(&format!("a{ix}.png"), AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &asset).unwrap();
        }
        let counted = BrowseContext::default()
            .snapshot(conn, &idx, None, true)
            .unwrap();
        assert_eq!(counted.total(), Some(4));

        let uncounted = BrowseContext::default()
            .snapshot(conn, &idx, None, false)
            .unwrap();
        assert_eq!(uncounted.total(), None, "it was never told to count");
        // The page still answers, and its total is the window it filled — the
        // same lower bound `run_without_count` has always returned.
        let page = uncounted.page(conn, &idx, 0, Some(2)).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn dispatches_plain_search_smart_trash_and_recent() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut img = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        img.title = Some("sunset shot".into());
        assets::insert(conn, &img).unwrap();
        let doc = test_asset("b.txt", AssetKind::Document, Uuid::new_v4());
        assets::insert(conn, &doc).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, img.id).unwrap();
        idx.commit().unwrap();
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext::default();
            mutate(&mut c);
            c
        };

        // Plain: everything, newest first.
        let page = ctx(&|_| {}).run(conn, &idx, None, None).unwrap();
        assert_eq!(page.total, 2);

        // Search overrides the plain view.
        let page = ctx(&|c: &mut BrowseContext| c.search = "sunset".into())
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // Smart collection wins over the search text.
        let sc = smart_collections::create(
            conn,
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "docs".into(),
                query: serde_json::json!({
                    "op": "match", "field": "kind", "value": "document"
                }),
                position: 0,
            },
        )
        .unwrap();
        // The live search overrides the smart collection (the controller
        // clears the smart selection when a search starts, so this pairing
        // resolves to the search view — matching the UI contract).
        let page = ctx(&|c: &mut BrowseContext| {
            c.search = "sunset".into();
            c.smart = Some(sc.id);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // With no search text, the smart collection drives the view.
        let page = ctx(&|c: &mut BrowseContext| c.smart = Some(sc.id))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, doc.id);

        // The trash honours the grid filters like every other view — that is
        // what makes the filter bar safe to show there. Being ignored was the
        // old contract, and the bar was hidden to match.
        assets::set_trashed(conn, img.id, true).unwrap();

        let page = ctx(&|c: &mut BrowseContext| c.in_trash = true)
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(page.total, 1, "only the deleted asset");
        assert_eq!(page.items[0].id, img.id);
        assert!(page.items[0].trashed_at.is_some());

        // A kind filter that matches the deleted asset still finds it …
        let page = ctx(&|c: &mut BrowseContext| {
            c.in_trash = true;
            c.kind = Some(AssetKind::Image);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // … and one that does not rules it out instead of being dropped.
        for excluded in [AssetKind::Font, AssetKind::Document, AssetKind::Video] {
            let page = ctx(&|c: &mut BrowseContext| {
                c.in_trash = true;
                c.kind = Some(excluded);
            })
            .run(conn, &idx, None, None)
            .unwrap();
            assert_eq!(page.total, 0, "{excluded:?} must not match a deleted image");
        }

        let page = ctx(&|c: &mut BrowseContext| {
            c.in_trash = true;
            c.is_favorite = true;
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 0, "the deleted asset is not a favourite");

        // Tags narrow the trash as well.
        let keep = crate::store::tags::ensure_named(conn, "keep").unwrap();
        crate::store::tags::add_to_asset(conn, img.id, keep.id).unwrap();
        let page = ctx(&|c: &mut BrowseContext| {
            c.in_trash = true;
            c.tag = Some(keep.id);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // Orientation / min-rating / extension filters (plain view).
        let mut square = test_asset("c.png", AssetKind::Image, Uuid::new_v4());
        (square.width, square.height) = (Some(64), Some(64));
        assets::insert(conn, &square).unwrap();
        let mut rated = test_asset("d.png", AssetKind::Image, Uuid::new_v4());
        rated.rating = Some(4);
        rated.ext = "jpg".into();
        assets::insert(conn, &rated).unwrap();

        let page = ctx(&|c: &mut BrowseContext| c.orientation = Some(Orientation::Square))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![square.id]
        );

        let page = ctx(&|c: &mut BrowseContext| c.min_rating = Some(4))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![rated.id]
        );

        let page = ctx(&|c: &mut BrowseContext| c.ext = Some("JPG".into()))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![rated.id]
        );

        // Recent view orders by last view (the timestamps are RFC 3339
        // strings; separate the records so their order is unambiguous).
        view_history::record(conn, doc.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        view_history::record(conn, img.id).unwrap();
        let page = ctx(&|c: &mut BrowseContext| c.in_recent = true)
            .run(conn, &idx, None, None)
            .unwrap();
        // img was trashed above: readers hide trashed rows, so only doc shows.
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![doc.id]
        );

        // The recent list narrows under the same filters as every other view,
        // and keeps its own view-time order while doing so.
        for id in [square.id, rated.id] {
            std::thread::sleep(std::time::Duration::from_millis(5));
            view_history::record(conn, id).unwrap();
        }
        let recent = |mutate: &dyn Fn(&mut BrowseContext)| {
            ctx(&|c: &mut BrowseContext| {
                c.in_recent = true;
                mutate(c);
            })
            .run(conn, &idx, None, None)
            .unwrap()
        };
        let ids = |page: &Page<Asset>| -> Vec<Uuid> { page.items.iter().map(|a| a.id).collect() };

        let page = recent(&|_: &mut BrowseContext| {});
        assert_eq!(
            ids(&page),
            vec![rated.id, square.id, doc.id],
            "newest view first"
        );

        let page = recent(&|c: &mut BrowseContext| c.kind = Some(AssetKind::Document));
        assert_eq!(ids(&page), vec![doc.id], "kind narrows the recent list");

        let page = recent(&|c: &mut BrowseContext| c.min_rating = Some(4));
        assert_eq!(ids(&page), vec![rated.id], "so does the rating floor");

        let page = recent(&|c: &mut BrowseContext| c.ext = Some("JPG".into()));
        assert_eq!(ids(&page), vec![rated.id]);

        let page = recent(&|c: &mut BrowseContext| c.orientation = Some(Orientation::Square));
        assert_eq!(ids(&page), vec![square.id]);
        assert_eq!(page.total, 1, "the total follows the filters, not the pool");
    }

    #[test]
    fn aspect_preset_filters_plain_and_smart_views() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext::default();
            mutate(&mut c);
            c
        };
        // One asset per preset band, plus a no-dimensions row that must
        // match nothing.
        let sizes: &[(&str, Option<(u32, u32)>)] = &[
            ("cover.png", Some((900, 383))),      // 2.3499… → WechatCover
            ("wide.png", Some((1920, 1080))),     // 1.777…  → VideoWide
            ("vertical.png", Some((1080, 1920))), // 0.5625 → VideoVertical
            ("photo.png", Some((640, 480))),      // 1.333…  → PhotoLandscape
            ("portrait.png", Some((480, 640))),   // 0.75    → PhotoPortrait
            ("square.png", Some((64, 64))),       // 1.0     → Square
            ("nodims.png", None),
        ];
        let mut ids = std::collections::HashMap::new();
        for (name, dims) in sizes {
            let mut a = test_asset(name, AssetKind::Image, Uuid::new_v4());
            (a.width, a.height) = dims
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));
            assets::insert(conn, &a).unwrap();
            ids.insert(name.to_string(), a.id);
            idx.index_asset(conn, a.id).unwrap();
        }
        idx.commit().unwrap();

        let expected =
            |names: &[&str]| -> Vec<Uuid> { names.iter().map(|n| ids[*n]).collect::<Vec<_>>() };
        for (preset, names) in [
            (AspectPreset::WechatCover, vec!["cover.png"]),
            (AspectPreset::VideoWide, vec!["wide.png"]),
            (AspectPreset::VideoVertical, vec!["vertical.png"]),
            (AspectPreset::PhotoLandscape, vec!["photo.png"]),
            (AspectPreset::PhotoPortrait, vec!["portrait.png"]),
            (AspectPreset::Square, vec!["square.png"]),
        ] {
            let want = expected(&names);
            // Plain view: the SQL ratio-band CASE.
            let page = ctx(&|c: &mut BrowseContext| c.aspect = Some(preset))
                .run(conn, &idx, None, None)
                .unwrap();
            assert_eq!(
                page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
                want,
                "{preset:?} via SQL"
            );
            // Smart view: the in-memory mirror.
            let sc = smart_collections::create(
                conn,
                &crate::model::NewSmartCollection {
                    parent_id: None,
                    name: format!("smart-{preset:?}"),
                    query: serde_json::json!({
                        "op": "match", "field": "kind", "value": "image"
                    }),
                    position: 0,
                },
            )
            .unwrap();
            let page = ctx(&|c: &mut BrowseContext| {
                c.smart = Some(sc.id);
                c.aspect = Some(preset);
            })
            .run(conn, &idx, None, None)
            .unwrap();
            assert_eq!(
                page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
                want,
                "{preset:?} in memory"
            );
        }

        // The presets compose with the orientation filter.
        let page = ctx(&|c: &mut BrowseContext| {
            c.aspect = Some(AspectPreset::WechatCover);
            c.orientation = Some(Orientation::Landscape);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            expected(&["cover.png"])
        );
    }

    /// The resolution bands are a third, independent size question: the two
    /// shape filters compare proportions, and these compare the longer edge.
    #[test]
    fn resolution_bands_filter_by_the_longer_edge() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext::default();
            mutate(&mut c);
            c
        };
        // One asset per band, a portrait whose *height* decides it, a row on the
        // 2240 boundary, and a no-dimensions row that must match nothing.
        let sizes: &[(&str, Option<(u32, u32)>)] = &[
            ("hd.png", Some((1920, 1080))),   // 1920 → 1K
            ("qhd.png", Some((2560, 1440))),  // 2560 → 2K
            ("edge.png", Some((2240, 1400))), // 2240 → 2K, the first pixel of it
            ("uhd.png", Some((3840, 2160))),  // 3840 → 4K
            ("tall.jpg", Some((2000, 4000))), // portrait: the height decides
            ("nodims.png", None),
        ];
        let mut ids = std::collections::HashMap::new();
        for (name, dims) in sizes {
            let mut a = test_asset(name, AssetKind::Image, Uuid::new_v4());
            (a.width, a.height) = dims
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));
            assets::insert(conn, &a).unwrap();
            ids.insert(name.to_string(), a.id);
            idx.index_asset(conn, a.id).unwrap();
        }
        idx.commit().unwrap();
        let expected =
            |names: &[&str]| -> Vec<Uuid> { names.iter().map(|n| ids[*n]).collect::<Vec<_>>() };

        for (band, names) in [
            (ResolutionBand::OneK, vec!["hd.png"]),
            (ResolutionBand::TwoK, vec!["qhd.png", "edge.png"]),
            (ResolutionBand::FourK, vec!["uhd.png", "tall.jpg"]),
        ] {
            let want = expected(&names);
            let page = ctx(&|c: &mut BrowseContext| c.resolution = Some(band))
                .run(conn, &idx, None, None)
                .unwrap();
            assert_eq!(
                page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
                want,
                "{band:?} via SQL"
            );
            assert_eq!(page.total as usize, want.len(), "{band:?} counted itself");

            // A smart listing honours the same band, because the grid filters
            // go inside its statement rather than after its page.
            let sc = smart_collections::create(
                conn,
                &crate::model::NewSmartCollection {
                    parent_id: None,
                    name: format!("smart-{band:?}"),
                    query: serde_json::json!({
                        "op": "match", "field": "kind", "value": "image"
                    }),
                    position: 0,
                },
            )
            .unwrap();
            let page = ctx(&|c: &mut BrowseContext| {
                c.smart = Some(sc.id);
                c.resolution = Some(band);
            })
            .run(conn, &idx, None, None)
            .unwrap();
            // Compared as a set: a smart listing's own order is the rule's, not
            // the browse sort the plain view above used.
            assert_eq!(
                page.items
                    .iter()
                    .map(|a| a.id)
                    .collect::<std::collections::HashSet<_>>(),
                want.into_iter().collect::<std::collections::HashSet<_>>(),
                "{band:?} inside a smart collection"
            );
        }

        // Size and shape are different questions, so they compose: a 16:9 frame
        // that is also 4K is the only one left.
        let page = ctx(&|c: &mut BrowseContext| {
            c.aspect = Some(AspectPreset::VideoWide);
            c.resolution = Some(ResolutionBand::FourK);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            expected(&["uhd.png"])
        );
    }
}
