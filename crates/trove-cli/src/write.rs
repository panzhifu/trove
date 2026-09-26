//! The commands that change a library.
//!
//! Two rules hold across all of them. **Nothing destructive happens without
//! being asked for twice** — `purge` needs `--yes`, `trash` is reversible by
//! design, and `import --dry-run` reports before it acts. And **a write is
//! complete when the database says so**: these go through the same store the
//! app uses, so the outbox, undo history and derived tables all stay
//! consistent, and an index that could not be updated simply stays a little
//! behind until a process that owns the writer drains it.

use serde_json::{Map, Value, json};

use trove_core::ai::vendor::build_from_config;
use trove_core::media::import::ImportStorage;
use trove_core::model::{AssetPatch, NewCollection};
use trove_core::tasks::ai_analysis::AiAnalysisRunRequest;
use trove_core::tasks::import::{ImportOptions, ImportSource};
use trove_core::tasks::{TaskKind, TaskManager};

use crate::cli::{
    AnalyzeArgs, CollectionCommand, Ids, ImportArgs, IndexCommand, PurgeArgs, SetArgs, TagArgs,
};
use crate::ctx::{CliError, Env, Rendered, parse_asset_ids, resolve_collection};

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// Import files or directories through the real import job.
pub fn import(
    env: &Env,
    args: &ImportArgs,
    style: &crate::ctx::Style,
) -> Result<Rendered, CliError> {
    let (files, skipped_dirs) = trove_core::tasks::import::expand_dirs(args.paths.clone());
    if files.is_empty() {
        return Err(CliError::usage(
            "nothing to import: no files found under the given paths (hidden entries are skipped)",
        ));
    }
    let storage = if args.copy {
        ImportStorage::Copy
    } else {
        ImportStorage::Link
    };
    let into_collection = match &args.into {
        Some(raw) => Some(resolve_collection(env, raw)?.id),
        None => None,
    };

    if args.dry_run {
        // The same precheck the job would run, minus the staging: how many of
        // these the library already holds, and how many are new.
        let fresh = env.library.unimported_paths(&files);
        let held = files.len().saturating_sub(fresh.len());
        let result = json!({
            "dry_run": true,
            "storage": storage_name(storage),
            "files": files.len(),
            "new": fresh.len(),
            "already_held": held,
            "collection": args.into,
            "skipped": skipped_json(&skipped_dirs),
            "new_files": fresh.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        });
        let human = format!(
            "{} file(s): {} new, {} already held ({})",
            files.len(),
            fresh.len(),
            held,
            storage_name(storage)
        );
        return Ok(Rendered::new(result, human));
    }

    let options = ImportOptions {
        data_root: env.data_root.clone(),
        cache_root: env.cache_root.clone(),
        storage,
        source: ImportSource::Paths {
            paths: files.clone(),
            into_collection,
        },
    };

    style.progress(&format!(
        "importing {} file(s) ({})",
        files.len(),
        storage_name(storage)
    ));

    // The import runs on its own connection inside the job, so this only
    // hands it the work and waits: `JobContext` has no public constructor,
    // and going through the manager is also what makes cancellation,
    // progress and the job registry behave the same as in the app.
    let manager = TaskManager::new();
    let (_, receiver) = manager
        .start(TaskKind::Import, "cli import", move |ctx| {
            trove_core::tasks::import::run(&options, ctx)
        })
        .map_err(|error| CliError::runtime(format!("cannot start the import job: {error:?}")))?;
    // A job reports its own failure through the manager's event stream rather
    // than the channel, so a dropped sender is the only signal available here;
    // the detail arrives in `ImportOutcome::error`.
    let outcome = receiver
        .recv()
        .map_err(|_| CliError::runtime("the import job did not run to completion"))?;
    if let Some(error) = &outcome.error {
        return Err(CliError::runtime(format!("import failed: {error}")));
    }

    let report = &outcome.report;
    let items: Vec<Value> = report
        .imported
        .iter()
        .map(|item| {
            json!({
                "asset_id": item.asset_id,
                "file_name": item.file_name,
                "kind": item.kind,
                "content_hash": item.content_hash,
                // The bytes were already in the library and the existing
                // record was reused instead of a second one being created.
                "reused": item.reused,
            })
        })
        .collect();
    let skipped: Vec<Value> = report
        .skipped
        .iter()
        .map(|skip| json!({ "path": skip.path.display().to_string(), "reason": skip.reason }))
        .collect();

    let result = json!({
        "imported": report.imported_count(),
        "skipped": report.skipped_count(),
        "already_imported": report.already_imported,
        "cancelled": outcome.cancelled,
        "storage": storage_name(storage),
        "collection": args.into,
        "items": items,
        "skipped_items": skipped,
        "unreadable_entries": skipped_json(&skipped_dirs),
    });
    let human = format!(
        "{} imported, {} reused, {} skipped, {} already held",
        report.imported.iter().filter(|item| !item.reused).count(),
        report.imported.iter().filter(|item| item.reused).count(),
        report.skipped_count(),
        report.already_imported,
    );
    Ok(Rendered::new(result, human))
}

fn storage_name(storage: ImportStorage) -> &'static str {
    match storage {
        ImportStorage::Link => "link",
        ImportStorage::Copy => "copy",
    }
}

fn skipped_json(skipped: &[trove_core::media::import::ImportSkip]) -> Vec<Value> {
    skipped
        .iter()
        .map(|skip| json!({ "path": skip.path.display().to_string(), "reason": skip.reason }))
        .collect()
}

// ---------------------------------------------------------------------------
// set
// ---------------------------------------------------------------------------

/// Patch metadata on one or more assets.
pub fn set(env: &Env, args: &SetArgs) -> Result<Rendered, CliError> {
    let ids = parse_asset_ids(&args.ids)?;

    let patch = AssetPatch {
        title: if args.clear_title {
            Some(None)
        } else {
            args.title.clone().map(Some)
        },
        description: if args.clear_description {
            Some(None)
        } else {
            args.description.clone().map(Some)
        },
        rating: if args.clear_rating {
            Some(None)
        } else {
            args.rating.map(Some)
        },
        is_favorite: if args.favorite {
            Some(true)
        } else if args.no_favorite {
            Some(false)
        } else {
            None
        },
        source_url: if args.clear_source_url {
            Some(None)
        } else {
            args.source_url.clone().map(Some)
        },
        usage_status: args.usage.map(Into::into),
        ..AssetPatch::default()
    };

    if patch_is_empty(&patch) {
        return Err(CliError::usage(
            "nothing to change: pass at least one field, e.g. --title or --favorite",
        ));
    }

    for id in &ids {
        // `NotFound` comes back as a runtime error naming the id, which is
        // the useful thing to report when a batch has one bad member.
        env.library
            .patch_asset(*id, &patch)
            .map_err(|error| CliError::runtime(format!("{id}: {error}")))?;
    }

    let changed: Vec<String> = patch_field_names(&patch);
    Ok(Rendered::new(
        json!({
            "updated": ids.len(),
            "assets": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "fields": changed,
        }),
        format!("{} asset(s) updated: {}", ids.len(), changed.join(", ")),
    ))
}

fn patch_is_empty(patch: &AssetPatch) -> bool {
    patch.title.is_none()
        && patch.description.is_none()
        && patch.kind.is_none()
        && patch.rating.is_none()
        && patch.is_favorite.is_none()
        && patch.source_url.is_none()
        && patch.usage_status.is_none()
        && patch.commercial_use.is_none()
        && patch.facts.is_none()
}

fn patch_field_names(patch: &AssetPatch) -> Vec<String> {
    let mut names = Vec::new();
    if patch.title.is_some() {
        names.push("title".to_string());
    }
    if patch.description.is_some() {
        names.push("description".to_string());
    }
    if patch.kind.is_some() {
        names.push("kind".to_string());
    }
    if patch.rating.is_some() {
        names.push("rating".to_string());
    }
    if patch.is_favorite.is_some() {
        names.push("is_favorite".to_string());
    }
    if patch.source_url.is_some() {
        names.push("source_url".to_string());
    }
    if patch.usage_status.is_some() {
        names.push("usage_status".to_string());
    }
    names
}

// ---------------------------------------------------------------------------
// tag
// ---------------------------------------------------------------------------

/// Add, remove or replace tags on a batch of assets.
pub fn tag(env: &Env, args: &TagArgs) -> Result<Rendered, CliError> {
    let ids = parse_asset_ids(&args.ids)?;
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unknown = Vec::new();

    if !args.set.is_empty() {
        // Replace is one operation, not remove-then-add: `set_asset_tags`
        // writes exactly this set, so a tag that survives both lists is not
        // dropped and re-added.
        let tag_ids: Vec<uuid::Uuid> = args
            .set
            .iter()
            .map(|name| Ok(env.library.ensure_tag(name)?.id))
            .collect::<Result<_, trove_core::Error>>()?;
        for id in &ids {
            env.library.set_asset_tags(*id, &tag_ids)?;
        }
        return Ok(Rendered::new(
            json!({
                "updated": ids.len(),
                "assets": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                "tags": args.set,
                "mode": "replace",
            }),
            format!("{} asset(s) now carry: {}", ids.len(), args.set.join(", ")),
        ));
    }

    if !args.add.is_empty() {
        for name in &args.add {
            let tag = env.library.ensure_tag(name)?;
            env.library.tag_assets(&ids, tag.id, true)?;
            added.push(tag.name);
        }
    }

    if !args.remove.is_empty() {
        // Removing must not create: a typo in `--remove` would otherwise
        // leave behind a brand new empty tag.
        let all = trove_core::store::tags::list(env.library.store().conn())?;
        for name in &args.remove {
            let found = all
                .iter()
                .find(|tag| tag.name.eq_ignore_ascii_case(name.trim()));
            match found {
                Some(tag) => {
                    env.library.tag_assets(&ids, tag.id, false)?;
                    removed.push(tag.name.clone());
                }
                None => unknown.push(name.clone()),
            }
        }
    }

    if added.is_empty() && removed.is_empty() && unknown.is_empty() {
        return Err(CliError::usage(
            "nothing to do: pass --add, --remove or --set",
        ));
    }

    let mut object = Map::new();
    object.insert("updated".into(), json!(ids.len()));
    object.insert(
        "assets".into(),
        json!(ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()),
    );
    object.insert("added".into(), json!(added));
    object.insert("removed".into(), json!(removed));
    if !unknown.is_empty() {
        // Not an error: removing a tag that was never there is a no-op, and
        // saying so beats failing a whole batch over it.
        object.insert("unknown".into(), json!(unknown));
    }

    let mut human = format!("{} asset(s)", ids.len());
    if !added.is_empty() {
        human.push_str(&format!("; added {}", added.join(", ")));
    }
    if !removed.is_empty() {
        human.push_str(&format!("; removed {}", removed.join(", ")));
    }
    if !unknown.is_empty() {
        human.push_str(&format!("; no such tag: {}", unknown.join(", ")));
    }
    Ok(Rendered::new(Value::Object(object), human))
}

// ---------------------------------------------------------------------------
// trash / restore / purge
// ---------------------------------------------------------------------------

/// Move assets to the trash. Reversible; no file is touched.
pub fn trash(env: &Env, args: &Ids) -> Result<Rendered, CliError> {
    let ids = parse_asset_ids(&args.ids)?;
    let changed = env.library.trash_assets(&ids)?;
    Ok(Rendered::new(
        json!({
            "trashed": changed,
            "requested": ids.len(),
        }),
        format!("{changed} asset(s) moved to the trash"),
    ))
}

/// Bring assets back out of the trash.
pub fn restore(env: &Env, args: &Ids) -> Result<Rendered, CliError> {
    let ids = parse_asset_ids(&args.ids)?;
    let changed = env.library.restore_assets(&ids)?;
    Ok(Rendered::new(
        json!({
            "restored": changed,
            "requested": ids.len(),
        }),
        format!("{changed} asset(s) restored"),
    ))
}

/// Permanently delete assets.
pub fn purge(env: &Env, args: &PurgeArgs) -> Result<Rendered, CliError> {
    if !args.yes {
        return Err(CliError::usage(
            "purge deletes records and files for good; pass --yes to confirm",
        ));
    }
    let ids = parse_asset_ids(&args.ids)?;
    let report = env.library.purge_assets(&ids)?;
    Ok(Rendered::new(
        json!({
            "purged": report.purged,
            "blobs_removed": report.blobs_removed,
            "thumbnails_removed": report.thumbs_removed,
            "inbox_files_removed": report.sources_removed,
            "source_files_removed": report.source_files_removed,
            "requested": ids.len(),
        }),
        format!(
            "{} asset(s) deleted, {} blob(s) and {} thumbnail(s) removed",
            report.purged, report.blobs_removed, report.thumbs_removed,
        ),
    ))
}

// ---------------------------------------------------------------------------
// collection
// ---------------------------------------------------------------------------

/// Collection maintenance.
pub fn collection(env: &Env, command: &CollectionCommand) -> Result<Rendered, CliError> {
    use trove_core::store::collections;

    match command {
        CollectionCommand::List => crate::read::collections(env),

        CollectionCommand::Create { name, parent } => {
            let parent_id = match parent {
                Some(raw) => Some(resolve_collection(env, raw)?.id),
                None => None,
            };
            let conn = env.library.store().conn();
            let siblings = collections::children_of(conn, parent_id)?;
            let created = collections::create(
                conn,
                &NewCollection {
                    parent_id,
                    name: name.clone(),
                    position: siblings.len() as i64,
                },
            )?;
            Ok(Rendered::new(
                json!({
                    "created": {
                        "id": created.id,
                        "name": created.name,
                        "parent_id": created.parent_id,
                    }
                }),
                format!("created collection {} ({})", created.name, created.id),
            ))
        }

        CollectionCommand::Rename { collection, name } => {
            let target = resolve_collection(env, collection)?;
            env.library.rename_collection(target.id, name)?;
            Ok(Rendered::new(
                json!({ "renamed": { "id": target.id, "from": target.name, "to": name } }),
                format!("renamed {} to {name}", target.name),
            ))
        }

        CollectionCommand::Rm { collection } => {
            let target = resolve_collection(env, collection)?;
            collections::delete(env.library.store().conn(), target.id)?;
            Ok(Rendered::new(
                json!({ "deleted": { "id": target.id, "name": target.name } }),
                format!(
                    "deleted collection {} (its assets stay in the library)",
                    target.name
                ),
            ))
        }

        CollectionCommand::Add { collection, assets } => {
            let target = resolve_collection(env, collection)?;
            let ids = parse_asset_ids(assets)?;
            let changed = env.library.add_assets_to_collection(target.id, &ids)?;
            Ok(Rendered::new(
                json!({ "collection": target.name, "added": changed, "requested": ids.len() }),
                format!("{changed} asset(s) added to {}", target.name),
            ))
        }

        CollectionCommand::Remove { collection, assets } => {
            let target = resolve_collection(env, collection)?;
            let ids = parse_asset_ids(assets)?;
            let changed = env.library.remove_assets_from_collection(target.id, &ids)?;
            Ok(Rendered::new(
                json!({ "collection": target.name, "removed": changed, "requested": ids.len() }),
                format!("{changed} asset(s) removed from {}", target.name),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

/// Inspect or rebuild the search index.
pub fn index(env: &Env, command: &IndexCommand) -> Result<Rendered, CliError> {
    match command {
        IndexCommand::Status => crate::read::index_status(env),

        IndexCommand::Rebuild => {
            if !env.index_writable {
                return Err(CliError::library(
                    "the desktop app holds the search index; close it and run this again",
                ));
            }
            let documents = env.library.rebuild_text_index()?;
            Ok(Rendered::new(
                json!({ "documents": documents, "rebuilt": true }),
                format!("index rebuilt: {documents} documents"),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

/// Analyse assets with a multimodal model, or take a previous run's tags
/// back.
///
/// The vendor, endpoint and model come from the library's stored analysis
/// configuration, not from flags: a model name is a setting, not something to
/// retype on every invocation. Everything the run may disagree with it about
/// has a flag.
pub fn analyze(
    env: &Env,
    args: &AnalyzeArgs,
    style: &crate::ctx::Style,
) -> Result<Rendered, CliError> {
    let config = trove_core::config::AppConfig::load();
    let analysis = config.ai_analysis.clone().unwrap_or_default();
    let request = AiAnalysisRunRequest {
        only: parse_asset_ids(&args.ids)?,
        limit: args.limit,
        force: args.force,
        send_images: match (args.no_images, args.with_images) {
            (true, _) => Some(false),
            (_, true) => Some(true),
            _ => None,
        },
        fields: analysis_fields(args, &analysis),
        max_new_tags: args.max_new_tags,
        new_tag_parent: args.parent_tag.clone(),
        language: args.language.clone(),
        dry_run: args.dry_run,
        threads: args.threads,
    };

    if args.undo {
        return undo_analysis(env, request);
    }

    if !analysis.is_configured() {
        return Err(CliError::usage(
            "no analysis endpoint is configured: set `ai_analysis` (vendor, base_url, model, \
             api_key) in Trove's config.json",
        ));
    }
    // Built even for a dry run: the fingerprint deciding what counts as
    // already done includes the model name, so a dry run that invented one
    // would not answer the question it was asked. It is never called.
    let provider = std::sync::Arc::from(build_from_config(&analysis)?);
    let options = env.library.ai_analysis_options(&request);

    style.progress(&format!(
        "{}: {}",
        analysis.model,
        if args.dry_run {
            "counting what would be analysed"
        } else if options.send_images {
            "analysing, with thumbnails"
        } else {
            "analysing, text only"
        },
    ));

    let (_, receiver) = env
        .library
        .start_ai_analysis(provider, request)
        .map_err(|error| CliError::runtime(format!("cannot start the analysis job: {error:?}")))?;
    let outcome = receiver
        .recv()
        .map_err(|_| CliError::runtime("the analysis job did not run to completion"))?;
    if let Some(error) = &outcome.error {
        return Err(CliError::runtime(format!("analysis failed: {error}")));
    }

    let result = json!({
        "model": analysis.model,
        "vendor": analysis.vendor,
        "dry_run": args.dry_run,
        "planned": outcome.planned,
        "analysed": outcome.analysed,
        "unchanged": outcome.unchanged,
        "skipped": outcome.skipped,
        "failed": outcome.failed,
        "created_tags": outcome.created_tags,
        // The endpoint would not take an image, so the run went text-only
        // rather than failing — worth knowing, because it is a quality drop.
        "images_rejected": outcome.images_rejected,
        "cancelled": outcome.cancelled,
    });

    let human = if args.dry_run {
        format!(
            "would analyse {} asset(s); {} already done",
            outcome.planned, outcome.skipped
        )
    } else {
        let mut line = format!(
            "{} analysed, {} unchanged, {} skipped, {} failed",
            outcome.analysed, outcome.unchanged, outcome.skipped, outcome.failed,
        );
        if !outcome.created_tags.is_empty() {
            line.push_str(&format!("; new: {}", outcome.created_tags.join(", ")));
        }
        if outcome.images_rejected {
            line.push_str("; the endpoint refused images, so the run went text-only");
        }
        line
    };
    Ok(Rendered::new(result, human))
}

/// Resolve `--description` / `--no-description` / `--rating` / `--no-rating`
/// into a field set, or `None` to inherit the stored configuration verbatim.
fn analysis_fields(
    args: &AnalyzeArgs,
    config: &trove_core::config::AiAnalysisConfig,
) -> Option<trove_core::ai::analysis::AiAnalysisFields> {
    let touched = args.description || args.no_description || args.rating || args.no_rating;
    if !touched {
        return None;
    }
    // A flag pair is mutually exclusive by clap, so at most one of each lands.
    let mut fields = trove_core::ai::analysis::AiAnalysisFields {
        description: config.fields.description,
        tags: config.fields.tags,
        rating: config.fields.rating,
    };
    if args.description {
        fields.description = true;
    }
    if args.no_description {
        fields.description = false;
    }
    if args.rating {
        fields.rating = true;
    }
    if args.no_rating {
        fields.rating = false;
    }
    Some(fields)
}

fn undo_analysis(env: &Env, request: AiAnalysisRunRequest) -> Result<Rendered, CliError> {
    let (_, receiver) = env
        .library
        .start_ai_analysis_undo(request)
        .map_err(|error| CliError::runtime(format!("cannot start the undo job: {error:?}")))?;
    let outcome = receiver
        .recv()
        .map_err(|_| CliError::runtime("the undo job did not run to completion"))?;
    if let Some(error) = &outcome.error {
        return Err(CliError::runtime(format!("undo failed: {error}")));
    }

    let result = json!({
        "assets": outcome.assets,
        "detached": outcome.detached,
        "orphaned_tags": outcome.orphaned,
        "cancelled": outcome.cancelled,
    });
    let mut human = format!(
        "{} tag(s) detached from {} asset(s)",
        outcome.detached, outcome.assets
    );
    if !outcome.orphaned.is_empty() {
        human.push_str(&format!(
            "; now empty: {} (reported, not deleted)",
            outcome.orphaned.join(", ")
        ));
    }
    Ok(Rendered::new(result, human))
}
