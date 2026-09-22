//! Automatic tagging: ask a chat model about each asset, file the answer as
//! tags.
//!
//! Shaped like [`crate::tasks::embed`] and for the same reasons — its own
//! connection, batches, cancellation between them, one outcome — but with
//! three differences that come from what it is:
//!
//! - **One request per asset.** An embedding call takes a batch of 64; a chat
//!   model answers one conversation. The work is network-bound and slow, so
//!   requests run on a small pool while the database stays on this thread.
//! - **It writes tags**, which are user-visible and hard to take back. So it
//!   prefers the library's existing vocabulary, files anything new under one
//!   parent tag, and records what it added in the asset's `facts`, which is
//!   what [`undo`] reads.
//! - **It is idempotent through that record.** A second run skips every asset
//!   whose fingerprint already matches, which makes re-running free rather
//!   than merely cheap.
//!
//! Nothing here decides *whether* to tag: the caller picks the assets and the
//! budget. This module's job is to not lose that decision's consequences.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rusqlite::Connection;
use uuid::Uuid;

use crate::ai::chat::{ChatProvider, ChatRequest};
use crate::ai::tagging::{self, PromptOptions};
use crate::model::{Asset, AssetFacts, AssetQuery, AssetSort, NewTag, Origin};
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
const THREADS_ENV: &str = "TROVE_AUTOTAG_THREADS";

/// Page size when walking the library; the store caps a query at 1000.
const PAGE: u32 = 1_000;

/// Key under which an asset records what the tagger did to it, inside
/// `facts`. A free-form key is the whole point of `AssetFacts::unknown`:
/// something only one feature reads does not deserve a column.
const MARKER_KEY: &str = "ai_tags";

/// What to tag, and how far to let the model go.
#[derive(Debug, Clone)]
pub struct AutoTagOptions {
    pub db_path: PathBuf,
    pub data_root: PathBuf,
    pub cache_root: PathBuf,
    /// Restrict the run to these assets. Empty = every live asset.
    pub only: Vec<Uuid>,
    /// Stop after this many assets.
    pub limit: Option<u64>,
    /// Tag assets whose fingerprint already matches, instead of skipping them.
    pub force: bool,
    /// Send thumbnails. The task clears this itself when the endpoint refuses
    /// them, so a text-only model costs one failed request, not a failed run.
    pub send_images: bool,
    pub max_new_tags: u32,
    /// Parent the invented tags are filed under; empty files them at the root.
    pub new_tag_parent: String,
    pub language: String,
    /// Count the work and stop. Nothing is sent and nothing is written.
    pub dry_run: bool,
    /// Requests in flight. `None` falls back to [`THREADS_ENV`], then to
    /// [`DEFAULT_CONCURRENCY`]. One is a useful setting for a rate-limited
    /// endpoint — and the only one that makes "the endpoint refused an image
    /// exactly once" a testable statement.
    pub threads: Option<usize>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AutoTagOutcome {
    /// Assets the model was asked about and had something new to say about.
    pub tagged: u64,
    /// Assets the model was asked about and knew nothing to add.
    pub unchanged: u64,
    /// Assets skipped because their fingerprint already described this run.
    pub skipped: u64,
    /// Assets whose request failed; they keep their old markers and are
    /// retried by the next run.
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
    /// Assets that carried an automatic-tag record.
    pub assets: u64,
    /// Tags detached from those assets.
    pub detached: u64,
    /// Tags left behind because no asset uses them any more and they are
    /// listed here rather than deleted: a tag's identity is the user's, and
    /// an empty one costs nothing.
    pub orphaned: Vec<String>,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// What the caller wants, leaving the rest to the library.
///
/// Every field is optional because the interesting caller — a command line
/// invocation, a button — usually disagrees with the stored configuration
/// about one thing and inherits everything else.
#[derive(Debug, Clone, Default)]
pub struct AutoTagRequest {
    /// Restrict the run to these assets. Empty = every live asset.
    pub only: Vec<Uuid>,
    pub limit: Option<u64>,
    pub force: bool,
    pub send_images: Option<bool>,
    pub max_new_tags: Option<u32>,
    pub new_tag_parent: Option<String>,
    pub language: Option<String>,
    pub dry_run: bool,
    pub threads: Option<usize>,
}

impl AutoTagOptions {
    /// Resolve a request against the library's files and the stored chat
    /// settings. Every path comes from the library, every preference from the
    /// request with the configuration as its fallback.
    pub fn resolve(
        request: &AutoTagRequest,
        db_path: PathBuf,
        data_root: PathBuf,
        cache_root: PathBuf,
        chat: &crate::config::ChatConfig,
        language: Option<&str>,
    ) -> Self {
        Self {
            db_path,
            data_root,
            cache_root,
            only: request.only.clone(),
            limit: request.limit,
            force: request.force,
            send_images: request.send_images.unwrap_or(chat.send_images),
            max_new_tags: request.max_new_tags.unwrap_or(chat.max_new_tags),
            new_tag_parent: request
                .new_tag_parent
                .clone()
                .unwrap_or_else(|| chat.new_tag_parent.clone()),
            language: request
                .language
                .clone()
                .or_else(|| chat.tag_language.clone())
                .or_else(|| language.map(str::to_string))
                .unwrap_or_else(|| "en".into()),
            dry_run: request.dry_run,
            threads: request.threads,
        }
    }
}

/// One asset, prepared for the model.
struct Prepared {
    asset: Asset,
    /// The user message: the digest plus the note about the image.
    message: String,
    /// Where this asset's thumbnail is, when it has one and the run wants
    /// images. A path rather than the bytes: a whole library prepared up
    /// front would otherwise hold tens of megabytes of JPEG the pool has not
    /// asked for yet.
    thumbnail: Option<PathBuf>,
    fingerprint: String,
    /// Tags the asset already carries, so the model is not told to add them
    /// and the run can tell "new" from "reused".
    existing: Vec<String>,
}

/// Run one tagging pass.
pub fn run(
    options: &AutoTagOptions,
    provider: &dyn ChatProvider,
    ctx: &JobContext,
) -> Result<AutoTagOutcome, String> {
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

    let mut outcome = AutoTagOutcome::default();

    // The vocabulary is read once and then kept up to date as this run
    // invents tags, so the twentieth asset of a run is offered the words the
    // first one produced instead of reinventing them.
    let mut vocabulary = vocabulary(&conn).map_err(|e| format!("list tags: {e}"))?;
    // Created on demand, the first time a run actually invents a tag: a run
    // that finds nothing new — or a dry run, which sends nothing at all —
    // must not leave an empty `AI` tag behind.
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

    let mut work: Vec<Prepared> = Vec::new();
    for asset in candidates {
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
        let fingerprint = fingerprint(provider.id(), &asset, options);
        if !options.force && stored_fingerprint(&asset).as_deref() == Some(fingerprint.as_str()) {
            outcome.skipped += 1;
            continue;
        }

        let message = tagging::asset_digest(&asset, &existing);
        let thumbnail = options
            .send_images
            .then(|| thumbnail_path(options, &asset))
            .flatten();
        work.push(Prepared {
            message: format!("{message}\n\n{}", tagging::image_note(thumbnail.is_some())),
            thumbnail,
            fingerprint,
            existing,
            asset,
        });
    }

    outcome.planned = work.len() as u64;
    if options.dry_run {
        return Ok(outcome);
    }

    let system = tagging::system_prompt(&PromptOptions {
        vocabulary: &vocabulary,
        max_new_tags: options.max_new_tags,
        language: &options.language,
    });

    let threads = concurrency(options.threads).min(work.len().max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("trove-autotag-{index}"))
        .build()
        .map_err(|e| format!("build the tagger thread pool: {e}"))?;
    let chunk = (threads * 4).max(1);
    // Once the endpoint refuses an image, every later request in this run
    // goes text-only — the point is to pay for that discovery once, not once
    // per asset.
    let image_rejected = AtomicBool::new(false);

    let total = work.len() as u64;
    ctx.set_total(total);
    let mut done: u64 = 0;

    for batch in work.chunks(chunk) {
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }

        let replies: Vec<Result<Vec<String>, crate::Error>> = pool.install(|| {
            batch
                .par_iter()
                .map(|prepared| {
                    // Read at request time rather than up front: a large run
                    // stays flat in memory, and the pool is idle on IO
                    // anyway.
                    let image = if image_rejected.load(Ordering::Relaxed) {
                        None
                    } else {
                        prepared
                            .thumbnail
                            .as_deref()
                            .and_then(|path| std::fs::read(path).ok())
                    };
                    ask(
                        provider,
                        &system,
                        &prepared.message,
                        image.as_deref(),
                        &image_rejected,
                    )
                })
                .collect()
        });

        for (prepared, reply) in batch.iter().zip(replies) {
            match reply {
                Ok(suggested) => {
                    match apply(
                        &conn,
                        prepared,
                        &suggested,
                        &mut parent,
                        &parent_name,
                        &mut vocabulary,
                        &mut outcome.created_tags,
                    ) {
                        Ok(applied) => {
                            if applied.is_empty() {
                                outcome.unchanged += 1;
                            } else {
                                outcome.tagged += 1;
                            }
                            if let Err(error) =
                                record_marker(&conn, prepared, provider.id(), &applied)
                            {
                                tracing::warn!(
                                    asset = %prepared.asset.file_name,
                                    error = %error,
                                    "tagger: could not record what was added",
                                );
                            }
                        }
                        Err(error) => {
                            outcome.failed += 1;
                            tracing::warn!(
                                asset = %prepared.asset.file_name,
                                error = %error,
                                "tagger: tags rejected",
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
                        "tagger: request failed",
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
        tagged = outcome.tagged,
        unchanged = outcome.unchanged,
        skipped = outcome.skipped,
        failed = outcome.failed,
        created = outcome.created_tags.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "auto-tag run finished",
    );
    Ok(outcome)
}

/// Detach everything the tagger ever added, and forget that it did.
///
/// The library's own undo stack is in memory and belongs to the process that
/// filled it, so a background tag run has to be able to take its own work
/// back — that is what the per-asset record is for.
pub fn undo(options: &AutoTagOptions, ctx: &JobContext) -> Result<UndoOutcome, String> {
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
            outcome.error = Some(format!("list tagged assets: {error}"));
            return Ok(outcome);
        }
    };

    let total = marked.len() as u64;
    ctx.set_total(total);
    let mut done = 0;
    let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
    for asset in marked {
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }
        let added = stored_added(&asset);
        for name in &added {
            match tags::get_by_name(&conn, name) {
                Ok(Some(tag)) => {
                    if let Err(error) = tags::remove_from_asset(&conn, asset.id, tag.id) {
                        tracing::warn!(tag = %name, error = %error, "tagger undo: detach failed");
                        continue;
                    }
                    outcome.detached += 1;
                    touched.insert(name.clone());
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(tag = %name, error = %error, "tagger undo: lookup failed");
                }
            }
        }

        let mut facts = asset.facts.clone();
        facts.unknown.remove(MARKER_KEY);
        if let Err(error) = assets::update_facts(&conn, asset.id, &facts) {
            tracing::warn!(asset = %asset.file_name, error = %error, "tagger undo: marker not cleared");
        }
        outcome.assets += 1;
        done += 1;
        ctx.progress(done, total);
    }

    // Tags this undo emptied are reported, not deleted: one of them may be
    // something the user created by hand before the tagger ever reused it.
    // Scoped to what this call actually detached, so an unrelated empty tag
    // elsewhere in the tree is not blamed on the tagger.
    //
    // (`counts_by_tag` only lists tags that have assets, so the check runs
    // the other way round: every tag, against the counts.)
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

/// Attach what the model suggested, creating what the library does not have.
/// Returns the tags actually added to this asset.
///
/// `parent` is filled in the first time a tag is invented, so the parent tag
/// itself only comes into existence when something is filed under it.
fn apply(
    conn: &Connection,
    prepared: &Prepared,
    suggested: &[String],
    parent: &mut Option<Uuid>,
    parent_name: &str,
    vocabulary: &mut Vec<String>,
    created: &mut Vec<String>,
) -> crate::Result<Vec<String>> {
    let already: Vec<&str> = prepared.existing.iter().map(String::as_str).collect();
    let mut applied = Vec::new();

    for name in suggested {
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
    // Re-read: `apply` only touched `asset_tag`, but a concurrent writer (the
    // app's own inspector, say) may have moved `facts` under us, and this
    // column is whole-value on write.
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
            "prompt": tagging::PROMPT_VERSION,
            "digest": prepared.fingerprint,
            "at": chrono::Utc::now().to_rfc3339(),
            "added": added,
        }),
    );
    assets::update_facts(conn, prepared.asset.id, &facts)
}

/// Answer one asset, with the image when there is one.
fn ask(
    provider: &dyn ChatProvider,
    system: &str,
    message: &str,
    image: Option<&[u8]>,
    rejected: &AtomicBool,
) -> crate::Result<Vec<String>> {
    if let Some(bytes) = image {
        match provider.complete(&ChatRequest {
            system,
            user: message,
            image: Some(bytes),
        }) {
            Ok(reply) => return Ok(tagging::parse_tags(&reply)),
            Err(crate::Error::Validation(error)) => {
                // The endpoint refused the request itself, and the image is
                // the only part of it a text-only server would object to.
                tracing::debug!(%error, "tagger: endpoint refused an image; going text-only");
                rejected.store(true, Ordering::Relaxed);
            }
            // A transport failure already exhausted the retries; downgrading
            // would hide a broken endpoint behind a silent quality drop.
            Err(other) => return Err(other),
        }
    }
    let reply = provider.complete(&ChatRequest {
        system,
        user: message,
        image: None,
    })?;
    Ok(tagging::parse_tags(&reply))
}

/// The assets this run would consider: live, newest first, narrowed by
/// `only` / `limit`.
fn candidates(conn: &Connection, options: &AutoTagOptions) -> crate::Result<Vec<Asset>> {
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
/// The asset's *current* tags are deliberately left out even though the
/// prompt includes them: this run is about to change them, so counting them
/// as input would make every tagged asset permanently stale and re-ask about
/// the whole library on the second run.
///
/// Reuses [`crate::ai::source_hash`] rather than hashing here — same
/// primitive, same output shape, no second definition of "fingerprint" in
/// the crate.
fn fingerprint(model: &str, asset: &Asset, options: &AutoTagOptions) -> String {
    let mut input = String::new();
    input.push_str(model);
    input.push('\n');
    input.push_str(&tagging::PROMPT_VERSION.to_string());
    input.push('\n');
    input.push_str(&options.max_new_tags.to_string());
    input.push('\n');
    input.push_str(&options.language);
    input.push('\n');
    // The image is part of the answer's input, so a re-encoded file must
    // re-tag; the content hash is exactly that identity.
    input.push_str(asset.content_hash.as_deref().unwrap_or("-"));
    input.push('\n');
    input.push_str(&tagging::asset_digest(asset, &[]));
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
fn thumbnail_path(options: &AutoTagOptions, asset: &Asset) -> Option<PathBuf> {
    let sha = asset.content_hash.as_deref()?;
    let blob = match asset.origin {
        Origin::Linked => PathBuf::from(asset.facts.source_path.clone()?),
        Origin::Stored => options.data_root.join(asset.rel_path.as_deref()?),
    };
    crate::media::thumb::ensure(&options.cache_root, sha, asset.kind, &blob)
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
    use crate::ai::chat::ChatProvider;
    use crate::model::{AssetKind, test_asset};
    use crate::store::{Store, assets, tags};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// A provider that answers with one fixed reply, and remembers whether
    /// each request carried an image.
    struct MockChat {
        reply: String,
        images: Mutex<Vec<bool>>,
    }

    impl MockChat {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.into(),
                images: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> usize {
            self.images.lock().unwrap().len()
        }

        fn images_sent(&self) -> usize {
            self.images
                .lock()
                .unwrap()
                .iter()
                .filter(|sent| **sent)
                .count()
        }
    }

    impl ChatProvider for MockChat {
        fn id(&self) -> &str {
            "mock-chat"
        }

        fn complete(&self, request: &ChatRequest<'_>) -> crate::Result<String> {
            self.images.lock().unwrap().push(request.image.is_some());
            Ok(self.reply.clone())
        }
    }

    /// A provider that refuses anything carrying an image — what a text-only
    /// server does — and counts the refusals.
    struct TextOnlyChat {
        refusals: AtomicUsize,
    }

    impl TextOnlyChat {
        fn new() -> Self {
            Self {
                refusals: AtomicUsize::new(0),
            }
        }

        fn refusals(&self) -> usize {
            self.refusals.load(Ordering::Relaxed)
        }
    }

    impl ChatProvider for TextOnlyChat {
        fn id(&self) -> &str {
            "text-only"
        }

        fn complete(&self, request: &ChatRequest<'_>) -> crate::Result<String> {
            if request.image.is_some() {
                self.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(crate::Error::Validation(
                    "this model does not accept images".into(),
                ));
            }
            Ok(r#"["plain"]"#.into())
        }
    }

    /// A throwaway library of `count` images, each backed by a real PNG so
    /// the thumbnail path has something to work with.
    fn library(count: usize) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("trove-autotag-{}", Uuid::new_v4()));
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
            let mut asset = test_asset(
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

    fn options(
        data: &std::path::Path,
        cache: &std::path::Path,
        threads: Option<usize>,
    ) -> AutoTagOptions {
        AutoTagOptions {
            db_path: data.join("library.db"),
            data_root: data.to_path_buf(),
            cache_root: cache.to_path_buf(),
            only: Vec::new(),
            limit: None,
            force: false,
            send_images: true,
            max_new_tags: 3,
            new_tag_parent: "AI".into(),
            language: "en".into(),
            dry_run: false,
            threads,
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
        let provider = MockChat::new(r#"["cat", "outdoors"]"#);
        let outcome = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();

        assert_eq!(outcome.tagged, 3);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.created_tags.len(), 2, "both tags were invented");

        let conn = open(&data);
        let parent = tags::get_by_name(&conn, "AI")
            .unwrap()
            .expect("the parent tag exists");
        let all = tags::list(&conn).unwrap();
        assert_eq!(all.len(), 3, "the parent plus the two invented");
        for tag in all.iter().filter(|tag| tag.name != "AI") {
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
    fn an_image_is_sent_with_the_request() {
        let (root, data, cache) = library(1);
        let provider = MockChat::new(r#"["cat"]"#);
        run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();
        assert_eq!(provider.images_sent(), 1, "the thumbnail rode along");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_run_skips_everything_and_asks_nothing() {
        let (root, data, cache) = library(3);
        let provider = MockChat::new(r#"["cat"]"#);
        let first = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();
        assert_eq!(first.tagged, 3);
        assert_eq!(provider.requests(), 3);

        let second = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();
        assert_eq!(second.skipped, 3);
        assert_eq!(second.tagged, 0);
        assert_eq!(second.planned, 0);
        assert_eq!(
            provider.requests(),
            3,
            "a repeat run is free: nothing is asked"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn force_ignores_the_fingerprint_and_undo_takes_everything_back() {
        let (root, data, cache) = library(2);
        let provider = MockChat::new(r#"["cat", "outdoors"]"#);
        run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();

        let mut forced = options(&data, &cache, Some(1));
        forced.force = true;
        let again = run(&forced, &provider, &ctx()).unwrap();
        assert_eq!(again.planned, 2, "--force re-asks");
        assert_eq!(again.tagged, 0, "nothing new to add the second time");
        assert_eq!(again.unchanged, 2);

        let undone = undo(&options(&data, &cache, Some(1)), &ctx()).unwrap();
        assert_eq!(undone.assets, 2);
        assert_eq!(undone.detached, 4, "two tags off two assets");
        assert_eq!(
            undone.orphaned,
            vec!["cat".to_string(), "outdoors".to_string()],
            "the tags this undo emptied are reported",
        );

        let conn = open(&data);
        for asset in live_assets(&conn) {
            assert!(tag_names(&conn, asset.id).is_empty());
            assert!(
                !asset.facts.unknown.contains_key(MARKER_KEY),
                "the record goes too, so a later run tags it again"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_text_only_endpoint_is_noticed_exactly_once() {
        let (root, data, cache) = library(3);
        let provider = TextOnlyChat::new();
        let outcome = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();

        assert!(outcome.images_rejected);
        assert_eq!(
            provider.refusals(),
            1,
            "the discovery is paid for once, not once per asset"
        );
        assert_eq!(outcome.tagged, 3, "every asset succeeded on the retry");
        assert_eq!(outcome.failed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dry_run_counts_without_asking_and_writes_nothing() {
        let (root, data, cache) = library(2);
        let provider = MockChat::new(r#"["cat"]"#);
        let mut dry = options(&data, &cache, Some(1));
        dry.dry_run = true;
        let outcome = run(&dry, &provider, &ctx()).unwrap();

        assert_eq!(outcome.planned, 2);
        assert_eq!(outcome.tagged, 0);
        assert_eq!(provider.requests(), 0);
        let conn = open(&data);
        assert!(
            tags::list(&conn).unwrap().is_empty(),
            "not even the parent tag is created"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_useless_reply_changes_nothing() {
        let (root, data, cache) = library(2);
        let provider = MockChat::new("I would rather not.");
        let outcome = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();

        assert_eq!(outcome.tagged, 0);
        assert_eq!(outcome.unchanged, 2);
        assert_eq!(outcome.failed, 0, "a refusal to answer is not a failure");

        let conn = open(&data);
        assert!(tags::list(&conn).unwrap().is_empty());
        // The marker is still written: the model was asked and had nothing to
        // say, which is a fact worth not paying for twice.
        for asset in live_assets(&conn) {
            assert!(asset.facts.unknown.contains_key(MARKER_KEY));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_asset_with_a_tag_already_is_not_told_to_add_it_again() {
        let (root, data, cache) = library(1);
        {
            let conn = open(&data);
            let asset = live_assets(&conn).remove(0);
            let tag = tags::ensure_named(&conn, "cat").unwrap();
            tags::add_to_asset(&conn, asset.id, tag.id).unwrap();
        }

        let provider = MockChat::new(r#"["cat", "outdoors"]"#);
        let outcome = run(&options(&data, &cache, Some(1)), &provider, &ctx()).unwrap();

        let conn = open(&data);
        let asset = live_assets(&conn).remove(0);
        let mut names = tag_names(&conn, asset.id);
        names.sort();
        assert_eq!(names, vec!["cat".to_string(), "outdoors".to_string()]);
        assert_eq!(
            outcome.created_tags,
            vec!["outdoors".to_string()],
            "the tag that was already there is reused, not recreated"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
