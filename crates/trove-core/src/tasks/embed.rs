//! The embedding backfill: walk the library, embed what changed, store it.
//!
//! The job owns a private SQLite connection (same pattern as the import:
//! transactions need `&mut`, and the UI store hides its connection behind a
//! `RefCell`), works in batches of [`BATCH`] assets per provider request and
//! per transaction, and skips every asset whose [`crate::ai::source_hash`]
//! still matches what is stored — so re-running a finished backfill costs
//! one table scan and zero API calls.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::JobContext;
use crate::ai::{EmbeddingProvider, asset_embed_text, source_hash};
use crate::model::{Asset, EmbeddingSpace, NewEmbedding};
use crate::store::{Store, embeddings};

/// Assets per provider request and per transaction. The provider splits it
/// further if its own limit is lower; 64 keeps a failed request's retry
/// small and the write-lock windows short.
const BATCH: usize = 64;
/// Same courtesy the import extends: a backend writer must not error the
/// UI's reads.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the job's connection points — the library's `library.db`.
#[derive(Debug, Clone)]
pub struct EmbedOptions {
    pub db_path: PathBuf,
}

/// What one settled run did. `error` is set when the run stopped early (the
/// provider refused, the database broke) rather than trickling per-asset
/// failures, which land in `failed` and keep the run going.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct EmbedOutcome {
    pub embedded: u64,
    pub skipped: u64,
    pub failed: u64,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Run one backfill to completion against `provider`. Synchronous and
/// self-contained: tests call it directly, [`super::TaskManager`] runs it
/// on a thread.
pub fn run(
    options: &EmbedOptions,
    provider: &dyn EmbeddingProvider,
    ctx: &JobContext,
) -> Result<EmbedOutcome, String> {
    let started = Instant::now();
    // Open through the store once so pending schema migrations apply, then
    // reopen a plain connection for the job.
    Store::open(&options.db_path).map_err(|e| format!("open library database: {e}"))?;
    let mut conn =
        Connection::open(&options.db_path).map_err(|e| format!("open library database: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("set busy timeout: {e}"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -16000;",
    )
    .map_err(|e| format!("set connection pragmas: {e}"))?;

    let mut outcome = EmbedOutcome::default();
    let model = provider.id().to_string();
    let space = provider.asset_space();

    // A provider that knows its own width is checked before any request is
    // paid for; an unknown-width one is checked as soon as the first batch
    // comes back.
    if let Some(dim) = provider.dim()
        && let Err(e) = check_stored_dims(&conn, &model, space, dim)
    {
        outcome.error = Some(e);
        return Ok(outcome);
    }

    // Build the work list: every live asset whose recomputed fingerprint
    // differs from what is stored. Assets that match are counted and left
    // alone — this is what makes a second run cheap.
    ctx.set_summary("scanning library".into());
    let candidates = embeddings::embeddable_assets(&conn, &model, space)
        .map_err(|e| format!("list assets: {e}"))?;
    let mut work: Vec<(Asset, String, String)> = Vec::new();
    for (asset, stored) in candidates {
        if ctx.cancelled() {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        let tag_names: Vec<String> = crate::store::tags::for_asset(&conn, asset.id)
            .map_err(|e| format!("load tags: {e}"))?
            .into_iter()
            .map(|t| t.name)
            .collect();
        let text = asset_embed_text(&asset, &tag_names);
        let hash = source_hash(&text);
        if stored.as_deref() == Some(hash.as_str()) {
            outcome.skipped += 1;
        } else {
            work.push((asset, text, hash));
        }
    }

    let total = work.len() as u64;
    ctx.set_total(total);
    let mut done: u64 = 0;
    for batch in work.chunks(BATCH) {
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }

        let texts: Vec<String> = batch.iter().map(|(_, text, _)| text.clone()).collect();
        let vectors = match provider.embed_texts(&texts) {
            Ok(v) => v,
            Err(e) => {
                outcome.error = Some(format!("embedding provider failed: {e}"));
                return Ok(outcome);
            }
        };
        if vectors.len() != batch.len() {
            outcome.error = Some(format!(
                "embedding provider returned {} vectors for {} inputs",
                vectors.len(),
                batch.len()
            ));
            return Ok(outcome);
        }
        if provider.dim().is_none()
            && let Some(dim) = vectors.first().map(Vec::len)
            && let Err(e) = check_stored_dims(&conn, &model, space, dim)
        {
            outcome.error = Some(e);
            return Ok(outcome);
        }

        let tx = conn
            .transaction()
            .map_err(|e| format!("begin batch: {e}"))?;
        for ((asset, _, hash), vector) in batch.iter().zip(vectors) {
            let embedding = NewEmbedding {
                asset_id: asset.id,
                model: model.clone(),
                space,
                vector,
                source_hash: hash.clone(),
            };
            match embeddings::upsert(&tx, &embedding) {
                Ok(()) => outcome.embedded += 1,
                // One bad row must not sink the batch; the fingerprint
                // still mismatches, so the next run retries it.
                Err(e) => {
                    outcome.failed += 1;
                    tracing::warn!(asset = %asset.file_name, error = %e, "embedding row rejected");
                }
            }
        }
        tx.commit().map_err(|e| format!("commit batch: {e}"))?;

        done += batch.len() as u64;
        ctx.progress(done, total);
        ctx.set_summary(format!("{done} / {total}"));
    }

    crate::metrics::note_embedding_run(
        outcome.embedded as usize,
        outcome.failed as usize,
        started.elapsed(),
    );
    Ok(outcome)
}

/// Refuse to mix vector widths under one model identity: rows written by an
/// earlier, differently-configured provider would sit beside new vectors
/// they can never be scored against (the search skips them by length), so
/// the run stops with a "delete the model's vectors" instruction instead.
fn check_stored_dims(
    conn: &Connection,
    model: &str,
    space: EmbeddingSpace,
    dim: usize,
) -> Result<(), String> {
    let dims: Vec<i64> = crate::store::rows::query_map(
        conn,
        "SELECT DISTINCT dim FROM asset_embeddings WHERE model = ?1 AND space = ?2",
        vec![model.to_owned().into(), space.as_str().to_owned().into()],
        |row| crate::store::rows::int(row, 0),
    )
    .map_err(|e| e.to_string())?;
    if dims.iter().any(|&d| d != dim as i64) {
        return Err(format!(
            "stored vectors for model {model:?} use dimensions {dims:?}, \
             but the provider produces {dim}: delete the model's vectors and re-embed"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::MockProvider;
    use crate::model::test_asset;
    use crate::store::assets;
    use uuid::Uuid;

    /// A JobContext without a TaskManager: progress goes nowhere, the
    /// cancellation flag never fires. Tests only need the handle.
    fn bare_ctx() -> JobContext {
        JobContext::for_tests(false)
    }

    fn library_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-embed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed(dir: &std::path::Path, count: usize) {
        let store = Store::open(&dir.join("library.db")).unwrap();
        for i in 0..count {
            let asset = test_asset(
                &format!("asset-{i}.png"),
                crate::model::AssetKind::Image,
                Uuid::new_v4(),
            );
            assets::insert(store.conn(), &asset).unwrap();
        }
    }

    #[test]
    fn embeds_every_asset_and_reports_the_outcome() {
        let dir = library_dir();
        seed(&dir, 10);
        let options = EmbedOptions {
            db_path: dir.join("library.db"),
        };
        let ctx = bare_ctx();
        let outcome = run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();
        assert_eq!(outcome.embedded, 10);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.cancelled);
        assert!(outcome.error.is_none());

        // The vectors are really there, normalized, under the mock's identity.
        let store = Store::open(&dir.join("library.db")).unwrap();
        let (embedded, total) =
            embeddings::coverage(store.conn(), "mock", EmbeddingSpace::Text).unwrap();
        assert_eq!((embedded, total), (10, 10));
        let (id, _) = embeddings::snapshot(store.conn(), "mock", EmbeddingSpace::Text)
            .unwrap()
            .remove(0);
        let vector = embeddings::get(store.conn(), id, "mock", EmbeddingSpace::Text)
            .unwrap()
            .unwrap();
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit vector, got norm {norm}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_run_skips_everything() {
        let dir = library_dir();
        seed(&dir, 5);
        let options = EmbedOptions {
            db_path: dir.join("library.db"),
        };
        let ctx = bare_ctx();
        run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();
        let outcome = run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();
        assert_eq!(outcome.embedded, 0);
        assert_eq!(outcome.skipped, 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edited_metadata_reembeds_but_unchanged_does_not() {
        let dir = library_dir();
        seed(&dir, 2);
        let options = EmbedOptions {
            db_path: dir.join("library.db"),
        };
        let ctx = bare_ctx();
        run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();

        // Retitle one asset: only its fingerprint moves.
        let store = Store::open(&dir.join("library.db")).unwrap();
        let page = assets::query(store.conn(), &crate::model::AssetQuery::default()).unwrap();
        assets::update(
            store.conn(),
            page.items[0].id,
            &crate::model::AssetPatch {
                title: Some(Some("A new title".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let outcome = run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();
        assert_eq!(outcome.embedded, 1, "only the edited asset re-embeds");
        assert_eq!(outcome.skipped, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dim_conflict_is_a_fatal_error_not_a_mixed_write() {
        let dir = library_dir();
        seed(&dir, 3);
        let options = EmbedOptions {
            db_path: dir.join("library.db"),
        };
        let ctx = bare_ctx();
        run(&options, &MockProvider::new("mock", 8), &ctx).unwrap();

        // The same model name, a different width: refused up front.
        let outcome = run(&options, &MockProvider::new("mock", 16), &ctx).unwrap();
        let error = outcome.error.expect("dim conflict must surface");
        assert!(
            error.contains("dimensions") && error.contains("delete"),
            "{error}"
        );
        assert_eq!(outcome.embedded, 0);

        // A different model name is a different identity and just works.
        let outcome = run(&options, &MockProvider::new("mock-wide", 16), &ctx).unwrap();
        assert_eq!(outcome.embedded, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trashed_assets_are_not_embedded() {
        let dir = library_dir();
        seed(&dir, 4);
        let store = Store::open(&dir.join("library.db")).unwrap();
        let page = assets::query(store.conn(), &crate::model::AssetQuery::default()).unwrap();
        assets::set_trashed(store.conn(), page.items[0].id, true).unwrap();

        let options = EmbedOptions {
            db_path: dir.join("library.db"),
        };
        let outcome = run(&options, &MockProvider::new("mock", 8), &bare_ctx()).unwrap();
        assert_eq!(outcome.embedded, 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
