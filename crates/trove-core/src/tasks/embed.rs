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

/// Where the job's connection points, and where the library's files are.
#[derive(Debug, Clone)]
pub struct EmbedOptions {
    /// The library's `library.db`.
    pub db_path: PathBuf,
    /// Library root — where a stored asset's blob lives, needed to generate a
    /// thumbnail when the provider embeds images.
    pub data_root: PathBuf,
    /// Cache root — where thumbnails live.
    pub cache_root: PathBuf,
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

/// What one asset contributes to the index.
///
/// Decided by the provider's space: a text embedder files the asset's
/// metadata, a multimodal one files its image. For a CLIP-style provider the
/// two encoders share one space, so a text query is comparable with either.
enum Input {
    Text(String),
    Image(PathBuf),
}

/// One asset queued for the next batches.
struct Work {
    asset: Asset,
    input: Input,
    /// Fingerprint of the input that produced the vector; a change means the
    /// stored row is stale.
    hash: String,
}

/// The thumbnail an image-space embedder should read, generated on demand.
fn thumbnail_for(options: &EmbedOptions, asset: &Asset) -> Option<PathBuf> {
    crate::media::thumb::ensure_for_asset(&options.cache_root, &options.data_root, asset)
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
    let mut work: Vec<Work> = Vec::new();
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
        // What goes into the vector depends on the provider's space. An
        // image-space asset with no usable thumbnail falls back to its
        // metadata text: a CLIP-style text encoder lands in the same space as
        // its image encoder, so the asset stays searchable rather than being
        // dropped.
        let (input, hash) = match space {
            EmbeddingSpace::Image => {
                match (thumbnail_for(options, &asset), asset.content_hash.clone()) {
                    (Some(path), Some(hash)) => (Input::Image(path), hash),
                    _ => {
                        let text = asset_embed_text(&asset, &tag_names);
                        let hash = source_hash(&text);
                        (Input::Text(text), hash)
                    }
                }
            }
            EmbeddingSpace::Text => {
                let text = asset_embed_text(&asset, &tag_names);
                let hash = source_hash(&text);
                (Input::Text(text), hash)
            }
        };
        if stored.as_deref() == Some(hash.as_str()) {
            outcome.skipped += 1;
        } else {
            work.push(Work { asset, input, hash });
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

        // A batch can mix the two input kinds — an image space falls back to
        // text for assets with no thumbnail — and the provider exposes one
        // call per kind. Split, call, then put the answers back in batch
        // order.
        let texts: Vec<String> = batch
            .iter()
            .filter_map(|work| match &work.input {
                Input::Text(text) => Some(text.clone()),
                Input::Image(_) => None,
            })
            .collect();
        let images: Vec<PathBuf> = batch
            .iter()
            .filter_map(|work| match &work.input {
                Input::Image(path) => Some(path.clone()),
                Input::Text(_) => None,
            })
            .collect();

        // An empty leg is skipped entirely: a text-only provider *errors* on
        // `embed_images(&[])`, so calling it blindly would fail every batch.
        let text_vectors = if texts.is_empty() {
            Vec::new()
        } else {
            match provider.embed_texts(&texts) {
                Ok(vectors) => vectors,
                Err(error) => {
                    outcome.error = Some(format!("embedding provider failed: {error}"));
                    return Ok(outcome);
                }
            }
        };
        let image_vectors = if images.is_empty() {
            Vec::new()
        } else {
            match provider.embed_images(&images) {
                Ok(vectors) => vectors,
                Err(error) => {
                    outcome.error = Some(format!("embedding provider failed: {error}"));
                    return Ok(outcome);
                }
            }
        };
        if text_vectors.len() != texts.len() || image_vectors.len() != images.len() {
            outcome.error = Some(format!(
                "embedding provider returned {} text and {} image vectors for {} inputs",
                text_vectors.len(),
                image_vectors.len(),
                batch.len()
            ));
            return Ok(outcome);
        }

        // Reassemble in batch order, which is the order the rows are written.
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(batch.len());
        let mut text_iter = text_vectors.into_iter();
        let mut image_iter = image_vectors.into_iter();
        for work in batch {
            match &work.input {
                Input::Text(_) => vectors.push(text_iter.next().unwrap_or_default()),
                Input::Image(_) => vectors.push(image_iter.next().unwrap_or_default()),
            }
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
        for (work, vector) in batch.iter().zip(vectors) {
            let embedding = NewEmbedding {
                asset_id: work.asset.id,
                model: model.clone(),
                space,
                vector,
                source_hash: work.hash.clone(),
            };
            match embeddings::upsert(&tx, &embedding) {
                Ok(()) => outcome.embedded += 1,
                // One bad row must not sink the batch; the fingerprint
                // still mismatches, so the next run retries it.
                Err(e) => {
                    outcome.failed += 1;
                    tracing::warn!(asset = %work.asset.file_name, error = %e, "embedding row rejected");
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
            data_root: dir.clone(),
            cache_root: dir.join("cache"),
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
            data_root: dir.clone(),
            cache_root: dir.join("cache"),
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
            data_root: dir.clone(),
            cache_root: dir.join("cache"),
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
            data_root: dir.clone(),
            cache_root: dir.join("cache"),
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
            data_root: dir.clone(),
            cache_root: dir.join("cache"),
        };
        let outcome = run(&options, &MockProvider::new("mock", 8), &bare_ctx()).unwrap();
        assert_eq!(outcome.embedded, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A multimodal mock: image space, and it counts the images it saw so the
    /// test can prove the backfill reached for the thumbnail.
    struct MockImageProvider {
        model: String,
        dim: usize,
        images_seen: std::sync::atomic::AtomicUsize,
    }

    impl EmbeddingProvider for MockImageProvider {
        fn id(&self) -> &str {
            &self.model
        }
        fn asset_space(&self) -> EmbeddingSpace {
            EmbeddingSpace::Image
        }
        fn dim(&self) -> Option<usize> {
            Some(self.dim)
        }
        fn embed_texts(&self, texts: &[String]) -> crate::error::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0; self.dim]).collect())
        }
        fn embed_images(
            &self,
            paths: &[std::path::PathBuf],
        ) -> crate::error::Result<Vec<Vec<f32>>> {
            self.images_seen
                .fetch_add(paths.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(paths.iter().map(|_| vec![0.5; self.dim]).collect())
        }
    }

    #[test]
    fn an_image_space_provider_embeds_the_thumbnail_not_the_metadata() {
        use crate::model::Origin;

        let root = library_dir();
        let data = root.join("data");
        let cache = root.join("cache");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        // A real PNG behind a linked asset, so a thumbnail can be generated.
        let store = Store::open(&data.join("library.db")).unwrap();
        let source = data.join("cat.png");
        image::RgbImage::from_pixel(32, 32, image::Rgb([200, 60, 40]))
            .save(&source)
            .unwrap();
        let hash = crate::media::hash::hash_bytes(&std::fs::read(&source).unwrap());
        let mut asset = test_asset("cat.png", crate::model::AssetKind::Image, Uuid::new_v4());
        asset.origin = Origin::Linked;
        asset.content_hash = Some(hash);
        asset.width = Some(32);
        asset.height = Some(32);
        asset.facts.source_path = Some(source.display().to_string());
        assets::insert(store.conn(), &asset).unwrap();

        let options = EmbedOptions {
            db_path: data.join("library.db"),
            data_root: data.clone(),
            cache_root: cache.clone(),
        };
        let provider = MockImageProvider {
            model: "mock-clip".into(),
            dim: 8,
            images_seen: std::sync::atomic::AtomicUsize::new(0),
        };
        let outcome = run(&options, &provider, &bare_ctx()).unwrap();
        assert_eq!(outcome.embedded, 1, "{outcome:?}");
        assert_eq!(
            provider
                .images_seen
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the image leg was used"
        );

        // The row is filed under the image space, at the provider's width.
        let store = Store::open(&data.join("library.db")).unwrap();
        assert_eq!(
            embeddings::coverage(store.conn(), "mock-clip", EmbeddingSpace::Image).unwrap(),
            (1, 1)
        );
        let vector = embeddings::get(store.conn(), asset.id, "mock-clip", EmbeddingSpace::Image)
            .unwrap()
            .expect("an image-space row");
        assert_eq!(vector.len(), 8);

        // Its fingerprint is the content hash, so a second run is free.
        let again = run(&options, &provider, &bare_ctx()).unwrap();
        assert_eq!(again.skipped, 1);
        assert_eq!(again.embedded, 0);
        std::fs::remove_dir_all(&root).ok();
    }
}
