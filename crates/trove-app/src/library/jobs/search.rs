//! Search-time AI calls fired off a committed query: fetch the semantic
//! vector for the search term and ask the planner for a structured plan. Both
//! are silent by design (a failed leg just leaves the search as it was, so it
//! is logged rather than toasted) and both drop a stale response if the user
//! moved on while the request was in flight.

use gpui_kit::*;

use crate::library::LibraryController;

/// Fetch the embedding for the just-committed search term (the search box
/// submits on Enter, so there is nothing to debounce), letting the grid's
/// next data pass fuse a vector leg into the text ranking.
///
/// Silent by design: no endpoint configured, an unreachable server, or a bad
/// model name all leave the search exactly as it was before hybrid ranking —
/// which is not worth a toast on every keystroke-submitted search. The
/// failure is logged instead.
///
/// The response is dropped unless the search term is still the committed one,
/// so a slow call can never paint an answer to a question the user has moved
/// past.
pub fn request_query_embedding_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let text = controller.read(cx).search_text.trim().to_string();
    if text.is_empty() {
        return;
    }
    // The semantic tier must be on: a disabled or unconfigured leg is a no-op
    // and the search stays text-only.
    if !controller.read(cx).search_tiers.semantic {
        return;
    }
    let already_have_it = controller
        .read(cx)
        .query_vector
        .as_ref()
        .is_some_and(|query| query.text == text);
    if already_have_it {
        return;
    }
    let Some(endpoint) = trove_core::config::AppConfig::load().semantic_endpoint() else {
        return;
    };
    let provider: std::sync::Arc<dyn trove_core::ai::EmbeddingProvider> =
        match trove_core::ai::OpenAICompatible::new(&endpoint) {
            Ok(provider) => std::sync::Arc::new(provider),
            Err(error) => {
                tracing::warn!(%error, "query embedding skipped: endpoint is misconfigured");
                return;
            }
        };
    let model = endpoint.model.trim().to_string();
    let space = provider.asset_space();
    let controller = controller.clone();

    cx.spawn(async move |cx| {
        let asked = text.clone();
        let result: Result<Vec<f32>, String> = cx
            .background_executor()
            .spawn(async move {
                provider
                    .embed_texts(std::slice::from_ref(&asked))
                    .map(|mut vectors| vectors.pop().unwrap_or_default())
                    .map_err(|error| error.to_string())
            })
            .await;
        let vector = match result {
            Ok(vector) if !vector.is_empty() => vector,
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(%error, "query embedding failed; searching with text only");
                return;
            }
        };
        controller.update(cx, |ctl, cx| {
            if ctl.search_text.trim() != text {
                return; // the user moved on while the request was in flight
            }
            ctl.query_vector = Some(trove_core::search::vector::QueryVector {
                text,
                model,
                space,
                vector,
            });
            // Re-run the query so the fused ranking replaces the text-only
            // one that is on screen right now.
            ctl.generation += 1;
            cx.notify();
        });
    })
    .detach();
}

/// Ask the planner for a structured plan for the just-committed search term,
/// when the AI tier is on.
///
/// Silent by design, exactly like [`request_query_embedding_app`]: an
/// unreachable planner, a bad model or a reply the validator rejects all
/// leave the search exactly as it was — the raw term is already a complete
/// answer, so the failure is logged rather than toasted.
///
/// The response is dropped unless the term is still the committed one, so a
/// slow planner can never repaint an answer to a question the user has moved
/// past.
pub fn request_ai_plan_app(controller: &Entity<LibraryController>, cx: &mut App) {
    let text = controller.read(cx).search_text.trim().to_string();
    if text.is_empty() {
        return;
    }
    if !controller.read(cx).search_tiers.ai {
        return;
    }
    let config = trove_core::config::AppConfig::load().search.ai;
    if !config.is_configured() {
        return;
    }
    let provider: std::sync::Arc<dyn trove_core::ai::vendor::VendorAdapter> = match config
        .vendor
        .parse::<trove_core::ai::vendor::VendorId>()
        .map_err(|error| error.to_string())
        .and_then(|vendor| {
            trove_core::ai::vendor::build_adapter(
                vendor,
                &config.base_url,
                &config.api_key,
                &config.model,
            )
            .map_err(|error| error.to_string())
        }) {
        Ok(adapter) => std::sync::Arc::from(adapter),
        Err(error) => {
            tracing::warn!(%error, "AI search plan skipped: endpoint is misconfigured");
            return;
        }
    };
    let controller = controller.clone();

    cx.spawn(async move |cx| {
        let asked = text.clone();
        let result: Result<trove_core::ai::search_planner::AiSearchPlan, String> = cx
            .background_executor()
            .spawn(async move {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                trove_core::ai::search_planner::plan(provider.as_ref(), &asked, &cancel)
                    .map_err(|error| error.to_string())
            })
            .await;
        let plan = match result {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(%error, "AI search plan failed; searching with the raw term");
                return;
            }
        };
        controller.update(cx, |ctl, cx| {
            if ctl.search_text.trim() != text {
                return; // the user moved on while the request was in flight
            }
            ctl.ai_plan = Some(plan);
            // Re-run the query so the planned search replaces the raw-term
            // one that is on screen right now.
            ctl.generation += 1;
            cx.notify();
        });
    })
    .detach();
}
