//! The migration job: scan a foreign library (Eagle / Billfish), link-import
//! its files through the regular import pipeline, then write the carried
//! metadata — tags, ratings, notes, source URLs, folder collections — back
//! onto the records the import created.
//!
//! Three phases, one job. The import phase is literally
//! [`super::import::run`] with `ImportStorage::Link`, so it gets the same
//! batching, progress and cancellation as any import; the phases around it
//! are plain store work on this job's own connection.

use std::collections::HashMap;
use std::path::PathBuf;

use super::JobContext;
use super::import::{ImportOptions, ImportSource};
use crate::error::Result;
use crate::media::import::ImportStorage;
use crate::services::migrate::{self, MatchedItems, MigrationApplyReport};
use crate::store::{self, Store};

/// Where the migration reads from and which library it writes into.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// The foreign library folder — an Eagle `.library` directory or a
    /// Billfish library (one holding `.bf/billfish.db`).
    pub source: PathBuf,
    /// The open library's data root.
    pub data_root: PathBuf,
    /// The open library's cache root.
    pub cache_root: PathBuf,
}

/// What the migration job produced.
#[derive(Debug, Clone)]
pub struct MigrationOutcome {
    pub report: MigrationApplyReport,
    /// True when cancellation landed mid-import: fewer files were committed
    /// than planned and the metadata pass saw only what got in.
    pub cancelled: bool,
    /// The first transaction-level failure of the import phase, if any.
    pub error: Option<String>,
}

/// Run the three phases. Progress comes from the import phase itself (the
/// plan is built and the metadata written around it); the summary names the
/// phase so the UI can say what it is doing.
pub fn run(options: &MigrationOptions, ctx: &JobContext) -> Result<MigrationOutcome> {
    ctx.set_summary("scanning the foreign library".into());
    let plan = migrate::scan(&options.source)?;

    let store = Store::open(options.data_root.join("library.db").as_path())?;
    let prepared = store.transaction(|tx| migrate::prepare_metadata(&plan, tx))?;

    // The import phase reports its own progress; the plan's file count is
    // the total it will converge on (its internal walk can only shrink the
    // set — files that vanished since the scan fail as skips, not errors).
    let import_options = ImportOptions {
        data_root: options.data_root.clone(),
        cache_root: options.cache_root.clone(),
        storage: ImportStorage::Link,
        // No pre-gate: a re-run must reach the pipeline so the carried
        // metadata is re-asserted through the commit's reuse path — the
        // gate's "record complete" notion does not know about it.
        pre_gate: false,
        source: ImportSource::Paths {
            paths: plan.file_paths(),
            into_collection: None,
        },
    };
    ctx.set_summary("link-importing the files".into());
    let outcome = super::import::run(&import_options, ctx)?;

    // Backfill: which plan item did each imported record come from? The
    // importer wrote the source path into `facts`, so the record answers for
    // itself — including the content-hash reuse path, where the record
    // already existed and the migration simply re-asserts its metadata.
    ctx.set_summary("writing the metadata back".into());
    let index: HashMap<&std::path::Path, usize> = plan
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.source_path.as_path(), i))
        .collect();
    let mut matched = MatchedItems::with_capacity(outcome.report.imported.len());
    let mut asset_ids = Vec::with_capacity(outcome.report.imported.len());
    for item in &outcome.report.imported {
        let source = store::assets::get(store.conn(), item.asset_id)
            .ok()
            .flatten()
            .and_then(|asset| asset.facts.source_path);
        matched.push(
            source
                .as_deref()
                .and_then(|path| index.get(std::path::Path::new(path)).copied()),
        );
        asset_ids.push(item.asset_id);
    }
    let (tagged, backfilled, failed) = store
        .transaction(|tx| migrate::backfill_items(tx, &plan, &prepared, &asset_ids, &matched))?;

    Ok(MigrationOutcome {
        report: MigrationApplyReport {
            imported: outcome.report.imported.iter().filter(|i| !i.reused).count() as u64,
            reused: outcome.report.imported.iter().filter(|i| i.reused).count() as u64,
            skipped: outcome.report.skipped.len() as u64,
            tagged,
            collections_created: prepared.collections_created,
            backfilled,
            backfill_failed: failed,
        },
        cancelled: outcome.cancelled,
        error: outcome.error,
    })
}
