//! The outbox drain: `search_queue` rows are the only way mutations reach
//! the index, and this is what moves them.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use rusqlite::types::Value;
use uuid::Uuid;

use crate::error::Result;

use super::TextIndex;

/// How many outbox rows one pass of [`drain`] consumes before committing.
///
/// The batch is what the drain's remaining cost is spent on: the queued deletes
/// are batched into one statement, but Tantivy's commit (flush + reader reload)
/// is paid once per batch, so a small batch multiplies a fixed ~150 ms by the
/// number of batches. Measured on 20k assets (`search_smoke --micro 20000`,
/// release):
///
/// | batch | ms/row |
/// |-------|--------|
/// | 500   | 0.509  |
/// | 2000  | 0.139  |
/// | 8000  | 0.045  |
///
/// with a floor of 0.034 ms/row for indexing alone, i.e. 8000 is where the
/// commit overhead stops mattering. It stays honest about the other two bounds:
/// the batch delete binds one parameter per row (8000 ≪ SQLite's 32766 ceiling)
/// and holds the write lock for ~9 ms, well inside the 5 s `busy_timeout` a
/// concurrent backend writer (imports own a second connection) may wait, and a
/// crash mid-batch only costs re-indexing those rows, since the outbox rows are
/// still there.
const DRAIN_BATCH: i64 = 8_000;

/// A drain pass that runs longer than this escalates its log line from
/// debug to warn — the index's closest thing to a slow-query log. One batch
/// of [`DRAIN_BATCH`] lands well under it; a warn means a backlog big enough
/// that the UI thread paid real time on the read path that triggered it.
const SLOW_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Rows waiting in the `search_queue` outbox — how far the index is behind
/// the database. Non-zero is routine in a second process (the rows belong to
/// whoever owns the writer) and is what a read-only CLI handle reports, since
/// it cannot drain them itself.
pub fn pending_count(conn: &Connection) -> Result<u64> {
    let count = crate::store::rows::query_count(conn, "SELECT COUNT(*) FROM search_queue", vec![])?;
    Ok(count.max(0) as u64)
}

/// Flush the `search_queue` outbox into the index: upsert rows whose assets
/// still exist, drop documents for purged ones. `root` (the library root,
/// passed by the desktop app) additionally unlocks indexing text bodies from
/// the files themselves; `None` (tests, read-only handles that drain nothing
/// anyway) indexes from the rows alone. Cheap when the queue is empty
/// (one small SELECT), so every search can afford to call it. Lives on the
/// store connection, so both [`crate::library::Library`] and tests drive it.
///
/// A pass that moved rows logs its size and duration — debug normally, warn
/// past [`SLOW_DRAIN`]. These lines are the outbox's only queue-depth signal.
///
/// This is the only place the search index learns about asset or tag writes —
/// the `search_queue` triggers fill the outbox and nothing else touches the
/// index.
///
/// Ordering inside a batch is deliberate: the Tantivy commit lands *before*
/// the queue rows are dropped, so a crash in between leaves the rows queued
/// and the next drain redoes them (indexing is idempotent, and
/// [`TextIndex::index_asset`] also drops a doc whose row has vanished). The
/// converse order would lose index updates silently.
pub fn drain(conn: &Connection, index: &TextIndex, root: Option<&Path>) -> Result<()> {
    // A read-only handle owns no writer, so it cannot move rows out of the
    // outbox. Leaving them queued is the point: the next writable open (the
    // app, or a CLI command that got the lock) drains the same backlog.
    if !index.is_writable() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut rows: u64 = 0;
    loop {
        let pending: Vec<(i64, Uuid, bool)> = crate::store::rows::query_map(
            conn,
            "SELECT rowid, asset_id, deleted FROM search_queue LIMIT ?1",
            vec![Value::Integer(DRAIN_BATCH)],
            |row| {
                Ok((
                    crate::store::rows::int(row, 0)?,
                    crate::store::rows::req_uuid(row, 1)?,
                    crate::store::rows::int(row, 2)? != 0,
                ))
            },
        )?;
        if pending.is_empty() {
            break;
        }
        rows += pending.len() as u64;
        let full_batch = pending.len() as i64 == DRAIN_BATCH;

        // `search_queue` has no UNIQUE constraint (duplicate rows are
        // harmless), so the same asset can appear twice in one batch. Collapse
        // it to a single action — `deleted` is AND-ed, so a lone "live" row
        // wins — which keeps exactly one Tantivy op per asset per batch and
        // makes the outcome independent of row order.
        let mut actions: HashMap<Uuid, bool> = HashMap::with_capacity(pending.len());
        for (_, id, deleted) in &pending {
            actions
                .entry(*id)
                .and_modify(|d| *d &= *deleted)
                .or_insert(*deleted);
        }
        for (id, deleted) in &actions {
            if *deleted {
                index.remove_asset(*id)?;
            } else {
                index.index_asset_in(conn, *id, root)?;
            }
        }
        index.commit()?;

        // One transaction and one statement for the whole batch. Deleting the
        // rows one by one let each delete autocommit — a fsync each, ~5 ms per
        // row on the 10k benchmark, and ~95% of the drain's total cost. The
        // delete targets the exact rowids consumed, so a row a concurrent
        // writer enqueues for the same asset is left for the next pass.
        //
        // `unchecked_transaction` rather than `Store::transaction` because
        // this function only has a bare `&Connection` — the backend import task
        // drains through its own connection.
        let tx = conn.unchecked_transaction()?;
        let mut sql = String::from("DELETE FROM search_queue WHERE rowid IN (");
        let mut args: Vec<Value> = Vec::with_capacity(pending.len());
        for (i, (rowid, _, _)) in pending.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(Value::Integer(*rowid));
        }
        sql.push(')');
        crate::store::rows::execute(&tx, &sql, args)?;
        tx.commit()?;

        if !full_batch {
            break;
        }
    }
    if rows > 0 {
        let elapsed = started.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let slow = elapsed >= SLOW_DRAIN;
        crate::metrics::note_drain(rows, elapsed, slow);
        if slow {
            tracing::warn!(rows, elapsed_ms, "slow search outbox drain");
        } else {
            tracing::debug!(rows, elapsed_ms, "search outbox drained");
        }
    }
    Ok(())
}
