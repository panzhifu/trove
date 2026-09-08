//! Recently-viewed history.
//!
//! One row per asset with the last time it was selected in the UI. The UI
//! records a view whenever the primary selection changes; `record` upserts
//! (re-viewing bumps `viewed_at`) and prunes the table to
//! [`HISTORY_CAP`] entries so the history stays bounded.

use chrono::Utc;
use rusqlite::Connection;
use uuid::Uuid;

use super::rows;
use crate::error::Result;

/// Maximum number of history rows kept; older entries are pruned on record.
pub const HISTORY_CAP: usize = 200;

/// Record (or refresh) a view of `asset_id`, then prune to the cap.
///
/// Recording a trashed asset is allowed; readers hide trashed rows so a
/// later restore keeps the entry usable.
pub fn record(conn: &Connection, asset_id: Uuid) -> Result<()> {
    let viewed_at = rows::ts(Utc::now());
    rows::execute(
        conn,
        "INSERT INTO view_history (asset_id, viewed_at) VALUES (?1, ?2) \
         ON CONFLICT(asset_id) DO UPDATE SET viewed_at = excluded.viewed_at",
        vec![rows::uuid(asset_id).into(), viewed_at.into()],
    )?;
    prune(conn, HISTORY_CAP)
}

/// Drop the oldest entries beyond `cap`.
pub fn prune(conn: &Connection, cap: usize) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM view_history WHERE asset_id NOT IN \
         (SELECT asset_id FROM view_history ORDER BY viewed_at DESC LIMIT ?1)",
        vec![(cap as i64).into()],
    )?;
    Ok(())
}

/// Most recently viewed asset ids (newest first), excluding trashed assets.
pub fn recent_ids(conn: &Connection, limit: usize) -> Result<Vec<Uuid>> {
    rows::query_map(
        conn,
        "SELECT view_history.asset_id FROM view_history \
         JOIN assets ON assets.id = view_history.asset_id \
         WHERE assets.trashed_at IS NULL \
         ORDER BY view_history.viewed_at DESC \
         LIMIT ?1",
        vec![(limit as i64).into()],
        |row| rows::req_uuid(row, 0),
    )
}

/// Number of history entries whose asset is still live (the sidebar count).
pub fn live_count(conn: &Connection) -> Result<u64> {
    Ok(rows::query_count(
        conn,
        "SELECT COUNT(*) FROM view_history \
         JOIN assets ON assets.id = view_history.asset_id \
         WHERE assets.trashed_at IS NULL",
        vec![],
    )? as u64)
}

/// Clear the whole history ("Clear view history" context menu).
pub fn clear(conn: &Connection) -> Result<()> {
    rows::execute(conn, "DELETE FROM view_history", vec![])?;
    Ok(())
}
