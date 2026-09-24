//! Multimodal analysis: hand each asset to a vision-capable model and write
//! back what it saw — a description, tags, and an optional rating.
//!
//! Shaped like [`crate::tasks::embed`] and for the same reasons — its own
//! connection, batches, cancellation between them, one outcome — but with
//! three differences that come from what it is:
//!
//! - **One request per asset.** An embedding call takes a batch of 64; a
//!   vision model answers one conversation. The work is network-bound and
//!   slow, so requests run on a small pool while the database stays on this
//!   thread.
//! - **It writes user-visible text**, which is hard to take back. So it
//!   prefers the library's existing vocabulary, files anything new under one
//!   parent tag, never overwrites a hand-written description it was not
//!   given, and records what it added in the asset's `facts` — which is what
//!   [`undo`] reads.
//! - **It is idempotent through that record.** A second run skips every asset
//!   whose fingerprint already matches, which makes re-running free rather
//!   than merely cheap.
//!
//! The model comes from [`VendorAdapter`], so OpenAI, Anthropic, Gemini and
//! DashScope are interchangeable here. This is the successor to the older
//! chat-only tagger (`tasks::autotag`), and keeps everything that made that
//! one safe to point at a whole library.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rusqlite::Connection;
use uuid::Uuid;

use crate::ai::analysis::{
    self, AiAnalysisFields, AiAnalysisRequest, AiAnalysisResult, AiAnalysisSettings, MediaType,
};
use crate::ai::vendor::{VendorAdapter, VendorError};
use crate::model::{Asset, AssetFacts, AssetKind, AssetPatch, AssetQuery, AssetSort, NewTag};
use crate::store::{assets, tags};
use crate::tasks::JobContext;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Requests in flight. A run over a thousand assets is measured in minutes
/// and the endpoint, not the database, is the bottleneck; four at a time is
/// where a personal server stops queueing without hammering a metered one.
const DEFAULT_CONCURRENCY: usize = 4;

/// Override for the pool width, next to the staging pool's
/// `TROVE_STAGE_THREADS` for the same reason: the right number depends on
/// whether the endpoint is a local GPU or a paid API.
const THREADS_ENV: &str = "TROVE_ANALYSIS_THREADS";

/// Page size when walking the library; the store caps a query at 1000.
const PAGE: u32 = 1_000;

/// Key under which an asset records what the analysis did to it, inside
/// `facts`. A free-form key is the whole point of `AssetFacts::unknown`:
/// something only one feature reads does not deserve a column.
const MARKER_KEY: &str = "ai_analysis";

// ============================ options ======================================

/// What to analyse, and how far to let the model go.
#[derive(Debug, Clone)]
pub struct AiAnalysisOptions {
    pub db_path: PathBuf,
    pub data_root: PathBuf,
    pub cache_root: PathBuf,
    /// Restrict the run to these assets. Empty = every live asset.
    pub only: Vec<Uuid>,
    /// Stop after this many assets.
    pub limit: Option<u64>,
    /// Analyse assets whose fingerprint already matches, instead of skipping.
    pub force: bool,
    /// Send thumbnails. The task clears this itself when the endpoint refuses
    /// them, so a text-only model costs one failed request, not a failed run.
    pub send_images: bool,
    /// Which fields to ask for — and therefore which ones are written back.
    pub fields: AiAnalysisFields,
    /// Policy knobs: tag ceilings, the new-word budget, description limits.
    pub settings: AiAnalysisSettings,
    /// Parent the invented tags are filed under; empty files them at the root.
    pub new_tag_parent: String,
    pub language: String,
    /// Count the work and stop. Nothing is sent and nothing is written.
    pub dry_run: bool,
    /// Requests in flight. `None` falls back to [`THREADS_ENV`], then to
    /// [`DEFAULT_CONCURRENCY`].
    pub threads: Option<usize>,
}

/// What the caller wants, leaving the rest to the library.
///
/// Every preference is optional because the interesting caller — a command
/// line invocation, a button — usually disagrees with the stored
/// configuration about one thing and inherits everything else.
#[derive(Debug, Clone, Default)]
pub struct AiAnalysisRunRequest {
    /// Restrict the run to these assets. Empty = every live asset.
    pub only: Vec<Uuid>,
    pub limit: Option<u64>,
    pub force: bool,
    pub send_images: Option<bool>,
    pub fields: Option<AiAnalysisFields>,
    pub max_new_tags: Option<u32>,
    pub new_tag_parent: Option<String>,
    pub language: Option<String>,
    pub dry_run: bool,
    pub threads: Option<usize>,
}

impl AiAnalysisOptions {
    /// Resolve a request against the library's files and the stored analysis
    /// settings. Every path comes from the library, every preference from the
    /// request with the configuration as its fallback.
    pub fn resolve(
        request: &AiAnalysisRunRequest,
        db_path: PathBuf,
        data_root: PathBuf,
        cache_root: PathBuf,
        config: &crate::config::AiAnalysisConfig,
        language: Option<&str>,
    ) -> Self {
        let settings = AiAnalysisSettings {
            max_new_tags: request.max_new_tags.unwrap_or(config.max_new_tags),
            ..AiAnalysisSettings::default()
        };
        Self {
            db_path,
            data_root,
            cache_root,
            only: request.only.clone(),
            limit: request.limit,
            force: request.force,
            send_images: request.send_images.unwrap_or(config.send_images),
            fields: request.fields.unwrap_or(AiAnalysisFields {
                description: config.fields.description,
                tags: config.fields.tags,
                rating: config.fields.rating,
            }),
            settings,
            new_tag_parent: request
                .new_tag_parent
                .clone()
                .unwrap_or_else(|| config.new_tag_parent.clone()),
            language: request
                .language
                .clone()
                .or_else(|| config.tag_language.clone())
                .or_else(|| language.map(str::to_string))
                .unwrap_or_else(|| "en".into()),
            dry_run: request.dry_run,
            threads: request.threads,
        }
    }
}

// ============================ outcomes =====================================

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AiAnalysisOutcome {
    /// Assets the model was asked about and had something new to write.
    pub analysed: u64,
    /// Assets the model was asked about and knew nothing to add.
    pub unchanged: u64,
    /// Assets skipped because their fingerprint already described this run.
    pub skipped: u64,
    /// Assets whose request failed; their markers are untouched and the next
    /// run retries them.
    pub failed: u64,
    /// Assets that would be processed, set by `dry_run`.
    pub planned: u64,
    /// Tags this run invented (as opposed to reused).
    pub created_tags: Vec<String>,
    pub cancelled: bool,
    /// Set when the endpoint rejected an image and the run went text-only.
    pub images_rejected: bool,
    pub error: Option<String>,
}

/// Outcome of [`undo`].
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct UndoOutcome {
    /// Assets that carried an analysis record.
    pub assets: u64,
    /// Tags detached from those assets.
    pub detached: u64,
    /// Tags left behind because no asset uses them any more. Reported rather
    /// than deleted: a tag's identity is the user's.
    pub orphaned: Vec<String>,
    pub cancelled: bool,
    pub error: Option<String>,
}

// ============================ run ==========================================

/// One asset, prepared for the model.
struct Prepared {
    asset: Asset,
    /// Everything the adapter needs except the image bytes, which are read
    /// lazily at request time so a whole library does not sit in memory as
    /// JPEG.
    request: AiAnalysisRequest,
    /// Where the thumbnail is, when the run wants images.
    thumbnail: Option<PathBuf>,
    /// Where a video's contact sheet is, when one exists.
    contact_sheet: Option<PathBuf>,
    fingerprint: String,
    /// Tags the asset already carries, so the model is not told to add them
    /// and the run can tell "new" from "reused".
    existing: Vec<String>,
}

/// Run one analysis pass.
pub fn run(
    options: &AiAnalysisOptions,
    provider: &dyn VendorAdapter,
    ctx: &JobContext,
) -> Result<AiAnalysisOutcome, String> {
    let started = Instant::now();
    // Open through the store once so pending migrations apply, then take a
    // connection of our own — the same arrangement every job uses.
    crate::store::Store::open(&options.db_path)
        .map_err(|e| format!("open library database: {e}"))?;
    let conn =
        Connection::open(&options.db_path).map_err(|e| format!("open library database: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("set busy timeout: {e}"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -16000;",
    )
    .map_err(|e| format!("set connection pragmas: {e}"))?;

    let mut outcome = AiAnalysisOutcome::default();

    // The vocabulary is read once and then kept up to date as this run
    // invents tags, so the parsing layer can reuse the spelling of a word the
    // library already has.
    let mut vocabulary = vocabulary(&conn).map_err(|e| format!("list tags: {e}"))?;
    // Created on demand, the first time a run actually invents a tag: a run
    // that finds nothing new — or a dry run, which sends nothing at all —
    // must not leave an empty parent tag behind.
    let parent_name = options.new_tag_parent.trim().to_string();
    let mut parent: Option<Uuid> = None;

    ctx.set_summary("scanning library".into());
    let candidates = match candidates(&conn, options) {
        Ok(candidates) => candidates,
        Err(error) => {
            outcome.error = Some(format!("list assets: {error}"));
            return Ok(outcome);
        }
    };

    let model_version = provider.model_version();
    let mut work: Vec<Prepared> = Vec::new();
    for asset in candidates {
        ctx.park_if_paused();
        if ctx.cancelled() {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        let existing: Vec<String> = match tags::for_asset(&conn, asset.id) {
            Ok(tags) => tags.into_iter().map(|tag| tag.name).collect(),
            Err(error) => {
                outcome.error = Some(format!("load tags: {error}"));
                return Ok(outcome);
            }
        };
        let fingerprint = fingerprint(model_version, &asset, options);
        if !options.force && stored_fingerprint(&asset).as_deref() == Some(fingerprint.as_str()) {
            outcome.skipped += 1;
            continue;
        }

        let request = build_request(&asset, options, &existing, &vocabulary);
        let thumbnail = options
            .send_images
            .then(|| thumbnail_path(options, &asset))
            .flatten();
        let contact_sheet = options
            .send_images
            .then(|| contact_sheet(options, &asset))
            .flatten();
        work.push(Prepared {
            asset,
            request,
            thumbnail,
            contact_sheet,
            fingerprint,
            existing,
        });
    }

    outcome.planned = work.len() as u64;
    if options.dry_run {
        return Ok(outcome);
    }

    let threads = concurrency(options.threads).min(work.len().max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("trove-analysis-{index}"))
        .build()
        .map_err(|e| format!("build the analysis thread pool: {e}"))?;
    let chunk = (threads * 4).max(1);
    // Once the endpoint refuses an image, every later request in this run goes
    // text-only — the point is to pay for that discovery once, not once per
    // asset.
    let image_rejected = AtomicBool::new(false);
    let cancel = ctx.cancel_flag();

    let total = work.len() as u64;
    ctx.set_total(total);
    let mut done: u64 = 0;

    for batch in work.chunks(chunk) {
        ctx.park_if_paused();
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }

        let replies: Vec<Result<AiAnalysisResult, String>> = pool.install(|| {
            batch
                .par_iter()
                .map(|prepared| ask(provider, prepared, &image_rejected, cancel))
                .collect()
        });

        for (prepared, reply) in batch.iter().zip(replies) {
            match reply {
                Ok(raw) => {
                    let processed = analysis::post_process(
                        raw,
                        &prepared.existing,
                        &vocabulary,
                        &options.settings,
                        &options.language,
                    );
                    match apply(
                        &conn,
                        prepared,
                        &processed,
                        &mut parent,
                        &parent_name,
                        &mut vocabulary,
                        &mut outcome.created_tags,
                    ) {
                        Ok(applied) => {
                            let wrote_metadata =
                                processed.description.is_some() || processed.rating.is_some();
                            if applied.is_empty() && !wrote_metadata {
                                outcome.unchanged += 1;
                            } else {
                                outcome.analysed += 1;
                            }
                            if let Err(error) =
                                record_marker(&conn, prepared, model_version, &applied)
                            {
                                tracing::warn!(
                                    asset = %prepared.asset.file_name,
                                    error = %error,
                                    "analysis: could not record what was added",
                                );
                            }
                        }
                        Err(error) => {
                            outcome.failed += 1;
                            tracing::warn!(
                                asset = %prepared.asset.file_name,
                                error = %error,
                                "analysis: result rejected",
                            );
                        }
                    }
                }
                Err(error) => {
                    // One bad asset must not sink the run; its marker is
                    // untouched, so the next run retries it.
                    outcome.failed += 1;
                    tracing::warn!(
                        asset = %prepared.asset.file_name,
                        error = %error,
                        "analysis: request failed",
                    );
                }
            }
            done += 1;
            ctx.progress(done, total);
        }
        ctx.set_summary(format!("{done} / {total}"));
    }

    outcome.images_rejected = image_rejected.load(Ordering::Relaxed);
    tracing::info!(
        analysed = outcome.analysed,
        unchanged = outcome.unchanged,
        skipped = outcome.skipped,
        failed = outcome.failed,
        created = outcome.created_tags.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "AI analysis run finished",
    );
    Ok(outcome)
}

// ============================ undo =========================================

/// Detach every tag the analysis ever added, and forget that it did.
///
/// The library's own undo stack is in memory and belongs to the process that
/// filled it, so a background run has to be able to take its own work back —
/// that is what the per-asset record is for. Descriptions and ratings are a
/// deliberate exception: the record does not keep their previous values, so
/// undo leaves them in place rather than guessing.
pub fn undo(options: &AiAnalysisOptions, ctx: &JobContext) -> Result<UndoOutcome, String> {
    crate::store::Store::open(&options.db_path)
        .map_err(|e| format!("open library database: {e}"))?;
    let conn =
        Connection::open(&options.db_path).map_err(|e| format!("open library database: {e}"))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("set busy timeout: {e}"))?;

    let mut outcome = UndoOutcome::default();
    let marked: Vec<Asset> = match marked_assets(&conn) {
        Ok(assets) => assets,
        Err(error) => {
            outcome.error = Some(format!("list analysed assets: {error}"));
            return Ok(outcome);
        }
    };

    let total = marked.len() as u64;
    ctx.set_total(total);
    let mut done = 0u64;
    let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
    for asset in marked {
        ctx.park_if_paused();
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }
        let added = stored_added(&asset);
        for name in &added {
            match tags::get_by_name(&conn, name) {
                Ok(Some(tag)) => {
                    if let Err(error) = tags::remove_from_asset(&conn, asset.id, tag.id) {
                        tracing::warn!(tag = %name, error = %error, "analysis undo: detach failed");
                        continue;
                    }
                    outcome.detached += 1;
                    touched.insert(name.clone());
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(tag = %name, error = %error, "analysis undo: lookup failed");
                }
            }
        }

        let mut facts = asset.facts.clone();
        facts.unknown.remove(MARKER_KEY);
        if let Err(error) = assets::update_facts(&conn, asset.id, &facts) {
            tracing::warn!(asset = %asset.file_name, error = %error, "analysis undo: marker not cleared");
        }
        outcome.assets += 1;
        done += 1;
        ctx.progress(done, total);
    }

    // Tags this undo emptied are reported, not deleted: one of them may be
    // something the user created by hand before the run ever reused it.
    outcome.orphaned = match (tags::list(&conn), tags::counts_by_tag(&conn)) {
        (Ok(all), Ok(counts)) => all
            .into_iter()
            .filter(|tag| touched.contains(&tag.name))
            .filter(|tag| counts.get(&tag.id).copied().unwrap_or(0) == 0)
            .map(|tag| tag.name)
            .collect(),
        _ => Vec::new(),
    };
    Ok(outcome)
}

// ============================ helpers ======================================

/// Ask the model about one asset, with the image when there is one.
fn ask(
    provider: &dyn VendorAdapter,
    prepared: &Prepared,
    rejected: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<AiAnalysisResult, String> {
    let mut request = prepared.request.clone();
    if !rejected.load(Ordering::Relaxed) {
        request.thumbnail_jpeg = prepared
            .thumbnail
            .as_deref()
            .and_then(|path| std::fs::read(path).ok());
        request.contact_sheet_jpeg = prepared
            .contact_sheet
            .as_deref()
            .and_then(|path| std::fs::read(path).ok());
    }
    let had_image = request.thumbnail_jpeg.is_some() || request.contact_sheet_jpeg.is_some();

    match provider.analyze(&request, cancel) {
        Ok(text) => analysis::parse_model_reply(&text, provider.model_version())
            .map_err(|error| error.to_string()),
        Err(error) if had_image && is_request_rejection(&error) => {
            // The endpoint refused the request itself, and the image is the
            // only part of it a text-only server would object to.
            tracing::debug!(%error, "analysis: endpoint refused an image; going text-only");
            rejected.store(true, Ordering::Relaxed);
            request.thumbnail_jpeg = None;
            request.contact_sheet_jpeg = None;
            let text = provider
                .analyze(&request, cancel)
                .map_err(|error| error.message.clone())?;
            analysis::parse_model_reply(&text, provider.model_version())
                .map_err(|error| error.to_string())
        }
        // A transport failure already exhausted the retries; downgrading
        // would hide a broken endpoint behind a silent quality drop.
        Err(error) => Err(error.message),
    }
}

/// A 4xx that was not retried: the server answered and refused the request
/// itself. Only such a reply is worth retrying without the image.
fn is_request_rejection(error: &VendorError) -> bool {
    !error.kind.is_transient()
        && error
            .http_status
            .is_some_and(|status| (400..500).contains(&status))
}

/// The request one asset's analysis is built from.
fn build_request(
    asset: &Asset,
    options: &AiAnalysisOptions,
    existing: &[String],
    vocabulary: &[String],
) -> AiAnalysisRequest {
    AiAnalysisRequest {
        asset_id: asset.id,
        display_name: asset
            .title
            .as_deref()
            .unwrap_or(&asset.file_name)
            .to_string(),
        file_name: asset.file_name.clone(),
        mime: asset.mime.clone(),
        media_type: media_type_of(asset.kind),
        thumbnail_jpeg: None,
        contact_sheet_jpeg: None,
        language: options.language.clone(),
        enabled_fields: options.fields,
        metadata_lines: analysis::asset_metadata_lines(asset),
        existing_tag_names: existing.to_vec(),
        vocabulary: vocabulary.to_vec(),
        settings: options.settings,
    }
}

fn media_type_of(kind: AssetKind) -> MediaType {
    match kind {
        AssetKind::Image => MediaType::Image,
        AssetKind::Video => MediaType::Video,
        AssetKind::Model => MediaType::Model3D,
        _ => MediaType::Other,
    }
}

/// Attach what the model suggested, creating what the library does not have.
/// Returns the tags actually added to this asset.
///
/// `parent` is filled the first time a tag is invented, so the parent tag
/// itself only comes into existence when something is filed under it.
fn apply(
    conn: &Connection,
    prepared: &Prepared,
    result: &AiAnalysisResult,
    parent: &mut Option<Uuid>,
    parent_name: &str,
    vocabulary: &mut Vec<String>,
    created: &mut Vec<String>,
) -> crate::Result<Vec<String>> {
    let already: Vec<&str> = prepared.existing.iter().map(String::as_str).collect();
    let mut applied = Vec::new();

    for name in &result.tags {
        if already
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let tag = match tags::get_by_name(conn, name)? {
            Some(tag) => tag,
            None => {
                let parent_id = match *parent {
                    Some(id) => Some(id),
                    None if parent_name.is_empty() => None,
                    None => {
                        let tag = tags::ensure_named(conn, parent_name)?;
                        *parent = Some(tag.id);
                        Some(tag.id)
                    }
                };
                let tag = tags::create(
                    conn,
                    &NewTag {
                        name: name.clone(),
                        color: None,
                        parent_id,
                    },
                )?;
                created.push(tag.name.clone());
                vocabulary.push(tag.name.clone());
                tag
            }
        };
        tags::add_to_asset(conn, prepared.asset.id, tag.id)?;
        applied.push(tag.name);
    }

    // Description and rating are written only when the model actually produced
    // them. `Some(None)` would clear the column, which would silently erase a
    // description the user wrote by hand.
    let patch = AssetPatch {
        description: result.description.clone().map(Some),
        rating: result.rating.map(Some),
        ..Default::default()
    };
    if patch.description.is_some() || patch.rating.is_some() {
        assets::update(conn, prepared.asset.id, &patch)?;
    }

    Ok(applied)
}

/// Write what this run added to the asset, so the next run can skip it and
/// [`undo`] can take it back.
///
/// The added list is merged rather than replaced: a forced re-run finds
/// nothing new to add (the tags are already there), and overwriting the
/// record with an empty list would quietly make the earlier run un-undoable.
fn record_marker(
    conn: &Connection,
    prepared: &Prepared,
    model: &str,
    applied: &[String],
) -> crate::Result<()> {
    // Re-read: `apply` only touched `asset_tag` and `assets`, but a
    // concurrent writer (the app's own inspector, say) may have moved `facts`
    // under us, and this column is whole-value on write.
    let mut facts: AssetFacts = match assets::get(conn, prepared.asset.id)? {
        Some(current) => current.facts,
        None => prepared.asset.facts.clone(),
    };
    let mut added = stored_added(&prepared.asset);
    for name in applied {
        if !added
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            added.push(name.clone());
        }
    }
    facts.unknown.insert(
        MARKER_KEY.into(),
        serde_json::json!({
            "model": model,
            "prompt": analysis::PROMPT_VERSION,
            "digest": prepared.fingerprint,
            "at": chrono::Utc::now().to_rfc3339(),
            "added": added,
        }),
    );
    assets::update_facts(conn, prepared.asset.id, &facts)
}

/// The assets this run would consider: live, newest first, narrowed by
/// `only` / `limit`.
fn candidates(conn: &Connection, options: &AiAnalysisOptions) -> crate::Result<Vec<Asset>> {
    if !options.only.is_empty() {
        let mut assets = assets::by_ids(conn, &options.only)?;
        if let Some(limit) = options.limit {
            assets.truncate(limit as usize);
        }
        return Ok(assets);
    }

    let mut out: Vec<Asset> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = assets::query(
            conn,
            &AssetQuery {
                is_trashed: false,
                sort: AssetSort::CreatedAt,
                sort_desc: true,
                limit: Some(PAGE),
                offset,
                ..AssetQuery::default()
            },
        )?;
        if page.items.is_empty() {
            break;
        }
        let short = page.items.len() < PAGE as usize;
        out.extend(page.items);
        if let Some(limit) = options.limit
            && out.len() as u64 >= limit
        {
            out.truncate(limit as usize);
            break;
        }
        if short {
            break;
        }
        offset += PAGE as u64;
    }
    Ok(out)
}

fn marked_assets(conn: &Connection) -> crate::Result<Vec<Asset>> {
    let mut out: Vec<Asset> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = assets::query(
            conn,
            &AssetQuery {
                is_trashed: false,
                sort: AssetSort::CreatedAt,
                sort_desc: true,
                limit: Some(PAGE),
                offset,
                ..AssetQuery::default()
            },
        )?;
        if page.items.is_empty() {
            break;
        }
        let short = page.items.len() < PAGE as usize;
        out.extend(
            page.items
                .into_iter()
                .filter(|asset| asset.facts.unknown.contains_key(MARKER_KEY)),
        );
        if short {
            break;
        }
        offset += PAGE as u64;
    }
    Ok(out)
}

/// The library's tags, most used first — the order the prompt quotes them in,
/// because the model sees the head of the list and not the tail.
fn vocabulary(conn: &Connection) -> crate::Result<Vec<String>> {
    let counts = tags::counts_by_tag(conn)?;
    let mut all = tags::list(conn)?;
    all.sort_by(|left, right| {
        let left_count = counts.get(&left.id).copied().unwrap_or(0);
        let right_count = counts.get(&right.id).copied().unwrap_or(0);
        right_count
            .cmp(&left_count)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(all.into_iter().map(|tag| tag.name).collect())
}

/// What "the same work" means: the facts the model would see, the model
/// itself, and the knobs that change its answer.
///
/// The asset's *current* tags are deliberately left out even though the prompt
/// includes them: this run is about to change them, so counting them as input
/// would make every tagged asset permanently stale and re-ask about the whole
/// library on the second run.
fn fingerprint(model: &str, asset: &Asset, options: &AiAnalysisOptions) -> String {
    let mut input = String::new();
    input.push_str(model);
    input.push('\n');
    input.push_str(&analysis::PROMPT_VERSION.to_string());
    input.push('\n');
    input.push_str(&options.settings.max_new_tags.to_string());
    input.push('\n');
    input.push_str(&options.language);
    input.push('\n');
    input.push_str(&format!(
        "{}{}{}",
        options.fields.description as u8, options.fields.tags as u8, options.fields.rating as u8
    ));
    input.push('\n');
    // The image is part of the answer's input, so a re-encoded file must
    // re-analyse; the content hash is exactly that identity.
    input.push_str(asset.content_hash.as_deref().unwrap_or("-"));
    input.push('\n');
    input.push_str(&analysis::asset_metadata_lines(asset).join("\n"));
    crate::ai::source_hash(&input)
}

fn stored_fingerprint(asset: &Asset) -> Option<String> {
    asset
        .facts
        .unknown
        .get(MARKER_KEY)
        .and_then(|marker| marker.get("digest"))
        .and_then(|digest| digest.as_str())
        .map(str::to_string)
}

fn stored_added(asset: &Asset) -> Vec<String> {
    asset
        .facts
        .unknown
        .get(MARKER_KEY)
        .and_then(|marker| marker.get("added"))
        .and_then(|added| added.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Where this asset's thumbnail is, generating it first if the cache is cold.
///
/// Both path rules are `Library::asset_file`'s — a linked file lives where it
/// was imported from, a stored one under this library's `media/` — but this
/// job holds no `Library`, and an asset whose file has moved on disk is not
/// worth failing a run over.
fn thumbnail_path(options: &AiAnalysisOptions, asset: &Asset) -> Option<PathBuf> {
    crate::media::thumb::ensure_for_asset(&options.cache_root, &options.data_root, asset)
}

/// This video's contact sheet, built on first use.
///
/// The cost belongs here and not at import: it is one ffmpeg pass per video, and
/// only the videos someone actually asks a model about should pay it. The result
/// is cached by content hash beside the thumbnails, so a second run over the same
/// library is free — and `clean_orphans` sweeps the directory with everything
/// else derived.
fn contact_sheet(options: &AiAnalysisOptions, asset: &Asset) -> Option<PathBuf> {
    if asset.kind != AssetKind::Video {
        return None;
    }
    let sha = asset.content_hash.as_deref()?;
    let path = options
        .cache_root
        .join("contact-sheet")
        .join(format!("{sha}.jpg"));
    if path.is_file() {
        return Some(path);
    }
    let blob = crate::media::thumb::blob_path(&options.data_root, asset)?;
    crate::media::video::write_contact_sheet(&blob, &path)?;
    Some(path)
}

/// How many requests to have in flight: the caller's choice, then the
/// environment's, then four.
fn concurrency(explicit: Option<usize>) -> usize {
    explicit
        .filter(|width| *width > 0)
        .or_else(|| {
            std::env::var(THREADS_ENV)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|width| *width > 0)
        })
        .unwrap_or(DEFAULT_CONCURRENCY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::vendor::{VendorError, VendorErrorKind, VendorId};
    use crate::model::Origin;
    use crate::store::Store;

    struct MockAdapter {
        response: String,
        requests: std::sync::atomic::AtomicUsize,
        images_sent: std::sync::atomic::AtomicUsize,
    }

    impl MockAdapter {
        fn new(response: &str) -> Self {
            Self {
                response: response.into(),
                requests: std::sync::atomic::AtomicUsize::new(0),
                images_sent: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::Relaxed)
        }

        fn images_sent(&self) -> usize {
            self.images_sent.load(Ordering::Relaxed)
        }
    }

    impl VendorAdapter for MockAdapter {
        fn vendor(&self) -> VendorId {
            VendorId::OpenAI
        }

        fn model_version(&self) -> &str {
            "mock-model"
        }

        fn analyze(
            &self,
            request: &AiAnalysisRequest,
            _cancel: &AtomicBool,
        ) -> std::result::Result<String, VendorError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            if request.thumbnail_jpeg.is_some() {
                self.images_sent.fetch_add(1, Ordering::Relaxed);
            }
            Ok(self.response.clone())
        }

        fn probe_connection(&self, _cancel: &AtomicBool) -> std::result::Result<(), VendorError> {
            Ok(())
        }
    }

    /// A throwaway library of `count` images, each backed by a real PNG so the
    /// thumbnail path has something to work with.
    fn library(count: usize) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("trove-analysis-{}", Uuid::new_v4()));
        let data = root.join("data");
        let cache = root.join("cache");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        let store = Store::open(&data.join("library.db")).unwrap();
        for index in 0..count {
            let source = data.join(format!("photo-{index}.png"));
            image::RgbImage::from_pixel(32, 32, image::Rgb([200, 60, 40]))
                .save(&source)
                .unwrap();
            let hash = crate::media::hash::hash_bytes(&std::fs::read(&source).unwrap());
            let mut asset = crate::model::test_asset(
                &format!("photo-{index}.png"),
                AssetKind::Image,
                Uuid::new_v4(),
            );
            asset.origin = Origin::Linked;
            asset.content_hash = Some(hash);
            asset.width = Some(32);
            asset.height = Some(32);
            asset.facts.source_path = Some(source.display().to_string());
            assets::insert(store.conn(), &asset).unwrap();
        }
        (root, data, cache)
    }

    fn options(data: &std::path::Path, cache: &std::path::Path) -> AiAnalysisOptions {
        AiAnalysisOptions {
            db_path: data.join("library.db"),
            data_root: data.to_path_buf(),
            cache_root: cache.to_path_buf(),
            only: Vec::new(),
            limit: None,
            force: false,
            send_images: true,
            fields: AiAnalysisFields {
                description: true,
                tags: true,
                rating: true,
            },
            settings: AiAnalysisSettings::default(),
            new_tag_parent: "AI".into(),
            language: "en".into(),
            dry_run: false,
            threads: Some(1),
        }
    }

    fn open(data: &std::path::Path) -> Connection {
        Connection::open(data.join("library.db")).unwrap()
    }

    fn ctx() -> JobContext {
        JobContext::for_tests(false)
    }

    fn live_assets(conn: &Connection) -> Vec<Asset> {
        assets::query(conn, &AssetQuery::default()).unwrap().items
    }

    fn tag_names(conn: &Connection, id: Uuid) -> Vec<String> {
        tags::for_asset(conn, id)
            .unwrap()
            .into_iter()
            .map(|tag| tag.name)
            .collect()
    }

    #[test]
    fn tags_are_attached_and_new_ones_filed_under_the_parent() {
        let (root, data, cache) = library(3);
        let provider = MockAdapter::new(
            r#"{"description": null, "tags": ["cat", "outdoors"], "rating": null}"#,
        );
        let outcome = run(&options(&data, &cache), &provider, &ctx()).unwrap();

        assert_eq!(outcome.analysed, 3);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.created_tags.len(), 2, "both tags were invented");

        let conn = open(&data);
        let parent = tags::get_by_name(&conn, "AI")
            .unwrap()
            .expect("the parent tag exists");
        for tag in tags::list(&conn).unwrap().iter().filter(|t| t.name != "AI") {
            assert_eq!(
                tag.parent_id,
                Some(parent.id),
                "{} must be filed under the parent",
                tag.name
            );
        }
        for asset in live_assets(&conn) {
            let mut names = tag_names(&conn, asset.id);
            names.sort();
            assert_eq!(names, vec!["cat".to_string(), "outdoors".to_string()]);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn description_and_rating_are_written_back() {
        let (root, data, cache) = library(1);
        let provider =
            MockAdapter::new(r#"{"description": "A red wall", "tags": ["red"], "rating": 4}"#);
        run(&options(&data, &cache), &provider, &ctx()).unwrap();

        let conn = open(&data);
        let asset = &live_assets(&conn)[0];
        assert_eq!(asset.description.as_deref(), Some("A red wall"));
        assert_eq!(asset.rating, Some(4));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_reply_never_erases_a_hand_written_description() {
        let (root, data, cache) = library(1);
        {
            let conn = open(&data);
            let id = live_assets(&conn)[0].id;
            assets::update(
                &conn,
                id,
                &AssetPatch {
                    description: Some(Some("mine".into())),
                    rating: Some(Some(5)),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let provider = MockAdapter::new(r#"{"description": null, "tags": [], "rating": null}"#);
        run(&options(&data, &cache), &provider, &ctx()).unwrap();

        let conn = open(&data);
        let asset = &live_assets(&conn)[0];
        assert_eq!(asset.description.as_deref(), Some("mine"), "not clobbered");
        assert_eq!(asset.rating, Some(5), "not clobbered");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_image_is_sent_with_the_request() {
        let (root, data, cache) = library(1);
        let provider =
            MockAdapter::new(r#"{"description": null, "tags": ["cat"], "rating": null}"#);
        run(&options(&data, &cache), &provider, &ctx()).unwrap();
        assert_eq!(provider.images_sent(), 1, "the thumbnail rode along");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_run_skips_everything_and_asks_nothing() {
        let (root, data, cache) = library(3);
        let provider =
            MockAdapter::new(r#"{"description": null, "tags": ["cat"], "rating": null}"#);
        assert_eq!(
            run(&options(&data, &cache), &provider, &ctx())
                .unwrap()
                .analysed,
            3
        );
        assert_eq!(provider.requests(), 3);

        let second = run(&options(&data, &cache), &provider, &ctx()).unwrap();
        assert_eq!(second.skipped, 3);
        assert_eq!(second.analysed, 0);
        assert_eq!(second.planned, 0);
        assert_eq!(provider.requests(), 3, "a repeat run is free");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn force_ignores_the_fingerprint_and_undo_takes_everything_back() {
        let (root, data, cache) = library(2);
        let provider = MockAdapter::new(
            r#"{"description": null, "tags": ["cat", "outdoors"], "rating": null}"#,
        );
        run(&options(&data, &cache), &provider, &ctx()).unwrap();

        let mut forced = options(&data, &cache);
        forced.force = true;
        run(&forced, &provider, &ctx()).unwrap();
        assert_eq!(provider.requests(), 4, "force re-asks both");

        let undo_outcome = undo(&forced, &ctx()).unwrap();
        assert_eq!(undo_outcome.assets, 2);
        assert_eq!(undo_outcome.detached, 4, "two assets × two tags");
        let conn = open(&data);
        for asset in live_assets(&conn) {
            assert!(tag_names(&conn, asset.id).is_empty());
            assert!(!asset.facts.unknown.contains_key(MARKER_KEY));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rejected_image_degrades_to_text_once() {
        struct Refuser {
            requests: std::sync::atomic::AtomicUsize,
        }
        impl VendorAdapter for Refuser {
            fn vendor(&self) -> VendorId {
                VendorId::OpenAI
            }
            fn model_version(&self) -> &str {
                "text-only"
            }
            fn analyze(
                &self,
                request: &AiAnalysisRequest,
                _cancel: &AtomicBool,
            ) -> std::result::Result<String, VendorError> {
                self.requests.fetch_add(1, Ordering::Relaxed);
                if request.thumbnail_jpeg.is_some() {
                    return Err(VendorError {
                        kind: VendorErrorKind::Refused,
                        message: "no images".into(),
                        http_status: Some(400),
                        provider_code: None,
                        request_id: None,
                    });
                }
                Ok(r#"{"description": null, "tags": ["plain"], "rating": null}"#.into())
            }
            fn probe_connection(
                &self,
                _cancel: &AtomicBool,
            ) -> std::result::Result<(), VendorError> {
                Ok(())
            }
        }

        let (root, data, cache) = library(3);
        let provider = Refuser {
            requests: std::sync::atomic::AtomicUsize::new(0),
        };
        let outcome = run(&options(&data, &cache), &provider, &ctx()).unwrap();
        assert!(outcome.images_rejected, "the run noticed and degraded");
        assert_eq!(outcome.analysed, 3);
        // Two requests for the first asset (image refused, then text), one
        // each for the rest.
        assert_eq!(provider.requests.load(Ordering::Relaxed), 4);
        let _ = std::fs::remove_dir_all(&root);
    }
}
