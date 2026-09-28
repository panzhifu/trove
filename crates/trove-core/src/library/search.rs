//! Search and its backing: full-text, semantic/vector, embeddings, browse pages, and smart rules.
//!
//! Split out of `library/mod.rs`; the methods are still `impl Library`.

use super::*;

impl Library {
    /// Full-text search across live assets, ordered by relevance. `q` narrows
    /// the ranked set by kind / collection / tags / favorite.
    pub fn search_assets(
        &self,
        text: &str,
        q: &crate::model::AssetQuery,
    ) -> crate::error::Result<crate::model::Page<crate::model::Asset>> {
        let prof = std::env::var_os("TROVE_PROFILE_QUERY").is_some();
        let t0 = std::time::Instant::now();
        let conn = self.store.conn();
        // The same grammar the desktop box speaks, so `trove search` and the
        // grid agree: qualifiers filter, terms rank.
        let expr = crate::search::expression::parse(text).into_expression();
        let has_terms = expr.groups.iter().any(|g| !g.atoms.is_empty());
        let mut q = q.clone();
        if !expr.filters.is_empty() {
            q.conditions = crate::model::QueryCondition::fold(
                std::mem::take(&mut q.conditions)
                    .into_iter()
                    .chain(expr.filters.iter().cloned())
                    .collect(),
            );
        }
        if !has_terms {
            // Nothing to rank. Qualifiers alone are a filtered listing, so
            // they still answer; a box holding only syntax characters has
            // neither, which is the empty answer the old splitter gave for an
            // empty box and stays the safe one.
            if q.conditions.is_empty() {
                return Ok(crate::model::Page::new(0, Vec::new()));
            }
            return assets::query(conn, &q);
        }
        // Pending outbox rows flush before the lookup, so a just-committed
        // mutation is visible to the same search.
        self.drain_search_queue()?;
        let t_drain = t0.elapsed();
        // The same pool sizing the grid uses: a query that is about to be
        // filtered down is gathered wider, because the intersection below runs
        // *after* the cap and can otherwise drop a match on the floor.
        let (candidates, ran_out) =
            self.text_index
                .pool_for(text, &expr, None, q.rejects_rows())?;
        let t_index = t0.elapsed();
        // Free text is entirely the index's business; `q` only carries the
        // structural filters, so it goes straight into the SQL intersection.
        let (total, ids) = assets::rank_intersect(conn, &candidates, &q)?;
        let t_rank = t0.elapsed();
        let page = assets::page_assets(&ids, &q, conn)?;
        let t_page = t0.elapsed();
        // The whole query against the slow-query threshold; the drain inside
        // it warns separately through the outbox's own slow-drain log.
        crate::metrics::note_query(t_page);
        if prof {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "[q] cand={} hits={} drain={:.2} index={:.2} rank={:.2} page={:.2}",
                candidates.len(),
                total,
                ms(t_drain),
                ms(t_index - t_drain),
                ms(t_rank - t_index),
                ms(t_page - t_rank),
            );
        }
        let mut result = crate::model::Page::new(total, page);
        // The pool ran out, so `total` is a floor: the library holds at least
        // this many matches, possibly more the caller never saw.
        result.truncated = ran_out;
        Ok(result)
    }

    // -- AI embeddings ---------------------------------------------------------

    /// `(embedded, total)` — how many live assets carry a vector under
    /// `model`, out of all live assets. The settings page's coverage line.
    pub fn embedding_coverage(&self, model: &str) -> Result<(u64, u64)> {
        crate::store::embeddings::coverage(
            self.store.conn(),
            model,
            crate::model::EmbeddingSpace::Text,
        )
    }

    /// Delete every vector stored under `model`, across spaces — the
    /// "switched provider, start over" button. Returns rows removed.
    pub fn delete_embeddings(&self, model: &str) -> Result<u64> {
        self.vector_index.borrow_mut().take();
        crate::store::embeddings::delete_model(self.store.conn(), model)
    }

    /// Start an embedding backfill on a background thread: every live asset
    /// whose source fingerprint moved (or that has no vector yet) is
    /// embedded through `provider` and stored. One backfill at a time
    /// (mutual exclusion is per [`crate::tasks::TaskKind`]); progress and
    /// lifecycle events come off [`Self::tasks`].
    pub fn start_embedding_backfill(
        &self,
        provider: std::sync::Arc<dyn crate::ai::EmbeddingProvider>,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::embed::EmbedOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = crate::tasks::embed::EmbedOptions {
            db_path: self.root.join("library.db"),
            data_root: self.root.clone(),
            cache_root: self.cache.clone(),
        };
        let label = format!("embedding backfill ({})", provider.id());
        // Retried, and at low priority. Every way this job can fail as a whole
        // is in its opening — open the database, set the pragmas, list assets —
        // and each of those is transient when the CLI holds the same library:
        // retrying two steps later gets a lock rather than a dead job. Past that
        // point a per-asset failure is recorded and the run continues, so
        // retrying cannot re-encode what already has an embedding.
        //
        // Low priority because it is a backfill: the user asked for it once and
        // it outranks nothing they are waiting on.
        self.tasks.start_with_retry_and_priority(
            crate::tasks::TaskKind::EmbeddingBackfill,
            label,
            crate::tasks::RetryPolicy::times(2),
            crate::tasks::TaskPriority::Low,
            move || {
                let options = options.clone();
                let provider = provider.clone();
                Box::new(move |ctx| crate::tasks::embed::run(&options, provider.as_ref(), ctx))
            },
        )
    }

    // -- image sequences -----------------------------------------------------

    /// Group `ids` into one image sequence at `fps` frames per second.
    ///
    /// The frames stay ordinary assets — nothing is copied, moved or rewritten
    /// — and this records only which of them form a run and in what order, so
    /// the listing rule can hide every frame but the first. Every refusal names
    /// the thing that went wrong rather than reporting a count: fewer than
    /// three frames, a frame already in another run, a selection spanning more
    /// than one directory, or frames whose dimensions disagree (a run of mixed
    /// sizes animates as a flicker, so it is a mistake rather than a shot).
    ///
    /// Not recorded on the undo stack, and it does not need to be: dissolving
    /// removes the group rows and touches nothing else, so one click reverses
    /// exactly what this did.
    pub fn create_sequence(&self, ids: &[Uuid], fps: f64) -> Result<Uuid> {
        crate::store::sequences::create(self.store.conn(), ids, fps)
    }

    /// Dissolve the named sequences, leaving their frames as the individual
    /// assets they were before grouping.
    pub fn dissolve_sequences(&self, ids: &[Uuid]) -> Result<usize> {
        crate::store::sequences::dissolve(self.store.conn(), ids)
    }

    /// Dissolve every sequence that has one of `asset_ids` as a frame.
    ///
    /// The selection the user makes is of *assets*, and a run is identified by
    /// its own id, so this is the shape the grid's context menu needs: select
    /// any frame — the visible card or a hidden member — and its whole run goes.
    pub fn dissolve_for_assets(&self, asset_ids: &[Uuid]) -> Result<usize> {
        let conn = self.store.conn();
        let mut ids: Vec<Uuid> = Vec::new();
        for asset_id in asset_ids {
            if let Some(m) = crate::store::sequences::membership(conn, *asset_id)?
                && !ids.contains(&m.sequence_id)
            {
                ids.push(m.sequence_id);
            }
        }
        crate::store::sequences::dissolve(conn, &ids)
    }

    /// Change a run's frame rate. The store's `CHECK` bounds it at 1…240 and
    /// says so in words the UI can show.
    pub fn set_sequence_fps(&self, id: Uuid, fps: f64) -> Result<()> {
        crate::store::sequences::set_fps(self.store.conn(), id, fps)
    }

    /// The run this asset is a frame of, if it is one — with its position and
    /// the full frame order, which is what a card carousel or a player walks.
    pub fn sequence_of(
        &self,
        asset_id: Uuid,
    ) -> Result<Option<crate::store::sequences::Membership>> {
        crate::store::sequences::membership(self.store.conn(), asset_id)
    }

    // -- AI analysis ---------------------------------------------------------

    /// Start a multimodal analysis run on a background thread: every live
    /// asset whose fingerprint does not already describe this run is handed
    /// to `provider`, and the description / tags / rating it returns are
    /// written back. Words the library does not have yet are filed under the
    /// configured parent tag.
    ///
    /// One run at a time (mutual exclusion is per [`crate::tasks::TaskKind`]);
    /// progress and lifecycle events come off [`Self::tasks`].
    pub fn start_ai_analysis(
        &self,
        provider: std::sync::Arc<dyn crate::ai::vendor::VendorAdapter>,
        request: crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::ai_analysis::AiAnalysisOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = self.ai_analysis_options(&request);
        let label = format!("ai analysis ({})", provider.model_version());
        // Retried, and cheap to retry: the run is idempotent through the record
        // it writes per asset (see `tasks::ai_analysis`'s module doc — "a second
        // run skips every asset whose fingerprint already matches, which makes
        // re-running free"). So a retry after the job died re-asks nothing that
        // was already answered, and the only failures that reach here are the
        // opening ones — a locked database, unreadable vocabulary — which are
        // exactly the transient kind.
        self.tasks.start_with_retry(
            crate::tasks::TaskKind::AiAnalysis,
            label,
            crate::tasks::RetryPolicy::times(2),
            move || {
                let options = options.clone();
                let provider = provider.clone();
                Box::new(move |ctx| {
                    crate::tasks::ai_analysis::run(&options, provider.as_ref(), ctx)
                })
            },
        )
    }

    /// Detach everything a previous analysis run added.
    ///
    /// The undo stack below is in memory and belongs to whichever process
    /// filled it, so a background run cannot lean on it; the record the run
    /// writes onto each asset instead is what this reads. No provider is
    /// involved — taking tags back asks no model anything. Descriptions and
    /// ratings are left in place: their previous values are not recorded.
    pub fn start_ai_analysis_undo(
        &self,
        request: crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> std::result::Result<
        (
            crate::tasks::TaskId,
            std::sync::mpsc::Receiver<crate::tasks::ai_analysis::UndoOutcome>,
        ),
        crate::tasks::StartError,
    > {
        let options = self.ai_analysis_options(&request);
        // Retried too, because taking tags back is a repair: detaching a tag
        // that is already detached and clearing a marker that is already clear
        // both do nothing, so a second attempt cannot overshoot.
        self.tasks.start_with_retry(
            crate::tasks::TaskKind::AiAnalysis,
            "ai analysis undo",
            crate::tasks::RetryPolicy::times(2),
            move || {
                let options = options.clone();
                Box::new(move |ctx| crate::tasks::ai_analysis::undo(&options, ctx))
            },
        )
    }

    /// The settings a run would use, resolved against this library's files
    /// and the stored analysis configuration.
    ///
    /// Public so a caller can show what is about to happen — and so a dry run
    /// and the real run agree on exactly which assets are in scope.
    pub fn ai_analysis_options(
        &self,
        request: &crate::tasks::ai_analysis::AiAnalysisRunRequest,
    ) -> crate::tasks::ai_analysis::AiAnalysisOptions {
        let config = crate::config::AppConfig::load();
        let analysis = config.ai_analysis.clone().unwrap_or_default();
        crate::tasks::ai_analysis::AiAnalysisOptions::resolve(
            request,
            self.root.join("library.db"),
            self.root.clone(),
            self.cache.clone(),
            &analysis,
            config.language.as_deref(),
        )
    }

    /// Pure semantic search: embed `query` with `provider`, score the model's
    /// stored vectors by cosine, and narrow the top candidates with `q`'s
    /// structural filters — the same rank-intersect-then-page pipeline the
    /// full-text search uses. An empty query is an empty page, not a scan.
    ///
    /// The workspace does **not** go through here: its search is hybrid,
    /// fusing the text and vector rankings with
    /// [`crate::search::vector::reciprocal_rank_fusion`] (see
    /// `store::browse`), which needs no provider at query time because the
    /// app fetches the query vector ahead of the call. This entry point is
    /// the "vectors only, no text leg" answer — the natural backing for a
    /// mode switch or a CLI query, and the reference for what the fused
    /// ranking started from.
    pub fn semantic_search(
        &self,
        provider: &dyn crate::ai::EmbeddingProvider,
        query: &str,
        q: &crate::model::AssetQuery,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        let started = std::time::Instant::now();
        let query = query.trim();
        if query.is_empty() {
            return Ok(crate::model::Page::new(0, Vec::new()));
        }
        // One query vector, from the same provider that produced the rows —
        // the model identity is the whole comparability contract.
        let vector = provider
            .embed_texts(std::slice::from_ref(&query.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::error::Error::Validation(
                    "embedding provider returned no vector for the query".into(),
                )
            })?;

        let conn = self.store.conn();
        let index = self.cached_vector_index(provider.id(), provider.asset_space());
        let candidates =
            index.search(conn, &vector, crate::search::vector::VECTOR_CANDIDATE_CAP)?;
        let ranked: Vec<Uuid> = candidates.into_iter().map(|m| m.asset_id).collect();
        let (total, ids) = assets::rank_intersect(conn, &ranked, q)?;
        let page = assets::page_assets(&ids, q, conn)?;
        crate::metrics::note_vector_search();
        crate::metrics::note_query(started.elapsed());
        Ok(crate::model::Page::new(total, page))
    }

    /// The cached in-memory index for one model+space, rebuilt when the
    /// identity changes. Drift *inside* one model (a backfill finishing, an
    /// asset deleted) is the index's own fingerprint check, not this cache's.
    ///
    /// Public because the workspace's hybrid ranking needs exactly the index
    /// [`Self::semantic_search`] uses, while holding a query vector fetched
    /// earlier rather than a provider.
    pub fn cached_vector_index(
        &self,
        model: &str,
        space: crate::model::EmbeddingSpace,
    ) -> crate::search::vector::VectorIndex {
        let mut cached = self.vector_index.borrow_mut();
        let stale = match cached.as_ref() {
            Some((cached_model, cached_space, _)) => {
                cached_model != model || *cached_space != space
            }
            None => true,
        };
        if stale {
            *cached = Some((
                model.to_string(),
                space,
                crate::search::vector::VectorIndex::new(model, space),
            ));
        }
        match cached.as_ref() {
            Some((_, _, index)) => index.clone(),
            None => unreachable!("populated immediately above"),
        }
    }

    // -- smart collections ----------------------------------------------------

    /// Create a smart collection from a validated `NewSmartCollection`.
    /// The condition tree is compiled once here (runnability against the
    /// current schema is a storage concern, so it is not part of the model's
    /// own validation).
    pub fn create_smart_collection(
        &self,
        input: &crate::model::NewSmartCollection,
    ) -> Result<crate::model::SmartCollection> {
        input.validate()?;
        smart::validate(&input.query)?;
        smart_collections::create(self.store.conn(), input)
    }

    pub fn list_smart_collections(&self) -> Result<Vec<crate::model::SmartCollection>> {
        smart_collections::list(self.store.conn())
    }

    pub fn get_smart_collection(&self, id: Uuid) -> Result<Option<crate::model::SmartCollection>> {
        smart_collections::get(self.store.conn(), id)
    }

    pub fn rename_smart_collection(&self, id: Uuid, name: &str) -> Result<()> {
        smart_collections::rename(self.store.conn(), id, name)
    }

    pub fn delete_smart_collection(&self, id: Uuid) -> Result<()> {
        smart_collections::delete(self.store.conn(), id)
    }

    /// Move a smart collection under `new_parent` (another smart collection
    /// or a regular collection) at `position`; cycles are refused.
    pub fn move_smart_collection(
        &self,
        id: Uuid,
        new_parent: Option<Uuid>,
        position: i64,
    ) -> Result<()> {
        let conn = self.store.conn();
        if smart_collections::get(conn, id)?.is_none() {
            return Err(crate::Error::NotFound("smart_collection"));
        }
        smart_collections::move_to(conn, id, new_parent, position)
    }

    /// Reorder a smart collection to `position` among its siblings, shifting
    /// others to make room. The parent is unchanged.
    pub fn reorder_smart_collection(&self, id: Uuid, position: i64) -> Result<()> {
        let conn = self.store.conn();
        smart_collections::reorder_to(conn, id, position)
    }

    /// How many live assets `node` matches: the badge beside each saved search.
    ///
    /// The rule comes in rather than an id, because the sidebar holds every
    /// smart collection already — looking one row up per badge would be a query
    /// per pixel of chrome.
    pub fn count_smart_rule(&self, node: &crate::model::SmartNode) -> Result<u64> {
        let page = smart::evaluate(self.store.conn(), Some(self.text_index()), node, None, 0)?;
        Ok(page.total)
    }

    /// Freeze a browse into a session: the answer set is decided once here, so
    /// the pages taken from it afterwards cannot disagree about what matches.
    pub fn browse_snapshot(
        &self,
        browse: &crate::store::BrowseContext,
        vector: Option<&crate::search::vector::VectorIndex>,
        count_total: bool,
    ) -> Result<crate::store::BrowseSession> {
        browse.snapshot(self.store.conn(), self.text_index(), vector, count_total)
    }

    /// One window of rows from a frozen browse, in its order.
    pub fn browse_page(
        &self,
        session: &crate::store::BrowseSession,
        offset: usize,
        window: Option<usize>,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        session.page(self.store.conn(), self.text_index(), offset, window)
    }

    /// How many assets each filter choice would return, for the session's whole
    /// answer set rather than the page on screen.
    pub fn browse_facets(
        &self,
        session: &crate::store::BrowseSession,
    ) -> Result<crate::store::facets::FacetCounts> {
        session.compute_facets(self.store.conn())
    }

    /// Evaluate a stored smart collection live, materialising the matching
    /// assets as a paged list. `kind` / `favorite` are extra grid filters
    /// AND-ed onto the tree (the toolbar filters compose with smart
    /// collections too).
    pub fn evaluate_smart_collection(
        &self,
        id: Uuid,
        page: smart::SmartPage,
    ) -> Result<crate::model::Page<crate::model::Asset>> {
        let conn = self.store.conn();
        let Some(smart_collection) = smart_collections::get(conn, id)? else {
            return Err(crate::Error::NotFound("smart_collection"));
        };
        let Some(node) = smart_collection.query.node() else {
            return Err(crate::Error::Validation(
                "smart collection rule is unreadable by this build".into(),
            ));
        };
        let ids = smart::evaluate_filtered(conn, Some(self.text_index()), node, page)?;
        let items = assets::by_ids(conn, &ids.items)?;
        Ok(crate::model::Page::new(ids.total, items))
    }
}
