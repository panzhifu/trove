//! Task journal: persists task metadata to SQLite so the UI can surface
//! interrupted work after a restart and track retry history.
//!
//! The journal is written by the [`TaskManager`](crate::tasks::TaskManager) on
//! every task lifecycle transition (start, complete, fail, retry). On library
//! open, [`load_interrupted`] reads tasks that were running or paused when the
//! process died, so the UI can offer to resume or retry them.

use rusqlite::params;
use uuid::Uuid;

use crate::error::Result;
use crate::tasks::{TaskId, TaskKind, TaskStatus};

/// One row from the task journal.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub task_id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
    pub done: u64,
    pub total: u64,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub retry_count: u8,
    pub max_retries: u8,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// Record a task start.
pub fn record_start(
    conn: &rusqlite::Connection,
    task_id: TaskId,
    kind: &TaskKind,
    label: &str,
    max_retries: u8,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO task_journal \
         (task_id, kind, label, status, done, total, retry_count, max_retries, started_at) \
         VALUES (?1, ?2, ?3, ?4, 0, 0, 0, ?5, ?6)",
        params![
            task_id.to_string(),
            &kind_to_str(kind),
            label,
            status_to_str(TaskStatus::Running),
            max_retries,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Record a terminal status (completed, failed, cancelled).
pub fn record_status(
    conn: &rusqlite::Connection,
    task_id: TaskId,
    status: TaskStatus,
    done: u64,
    total: u64,
    summary: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE task_journal \
         SET status = ?2, done = ?3, total = ?4, summary = ?5, error = ?6, finished_at = ?7 \
         WHERE task_id = ?1",
        params![
            task_id.to_string(),
            status_to_str(status),
            done,
            total,
            summary,
            error,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Increment the retry counter and reset status to running.
pub fn record_retry(conn: &rusqlite::Connection, task_id: TaskId) -> Result<()> {
    conn.execute(
        "UPDATE task_journal \
         SET retry_count = retry_count + 1, status = ?2, error = NULL, finished_at = NULL \
         WHERE task_id = ?1",
        params![task_id.to_string(), status_to_str(TaskStatus::Running)],
    )?;
    Ok(())
}

/// Read every task that was running or paused when the process exited.
pub fn load_interrupted(conn: &rusqlite::Connection) -> Result<Vec<JournalEntry>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, kind, label, status, done, total, summary, error, \
         retry_count, max_retries, started_at, finished_at \
         FROM task_journal WHERE status IN ('running', 'paused')",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(JournalEntry {
            task_id: row
                .get::<_, String>(0)?
                .parse()
                .unwrap_or_else(|_| Uuid::nil()),
            kind: str_to_kind(&row.get::<_, String>(1)?),
            label: row.get(2)?,
            status: str_to_status(&row.get::<_, String>(3)?),
            done: row.get(4)?,
            total: row.get(5)?,
            summary: row.get(6)?,
            error: row.get(7)?,
            retry_count: row.get(8)?,
            max_retries: row.get(9)?,
            started_at: row.get(10)?,
            finished_at: row.get(11)?,
        })
    })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// Serialize a task kind for journal storage. Built-in kinds use their stable
/// slug; custom kinds are stored as `"plugin:<name>"` so they round-trip.
fn kind_to_str(kind: &TaskKind) -> String {
    match kind {
        TaskKind::Import => "import".into(),
        TaskKind::CollectInbox => "collect-inbox".into(),
        TaskKind::ModelPreview => "model-preview".into(),
        TaskKind::VideoDecode => "video-decode".into(),
        TaskKind::BatchConvert => "batch-convert".into(),
        TaskKind::Maintenance => "maintenance".into(),
        TaskKind::VisualBackfill => "visual-backfill".into(),
        TaskKind::WatchScan => "watch-scan".into(),
        TaskKind::EmbeddingBackfill => "embedding-backfill".into(),
        TaskKind::AutoTag => "auto-tag".into(),
        TaskKind::AiAnalysis => "ai-analysis".into(),
        TaskKind::Custom(name) => format!("plugin:{name}"),
    }
}

fn str_to_kind(s: &str) -> TaskKind {
    match s {
        "import" => TaskKind::Import,
        "collect-inbox" => TaskKind::CollectInbox,
        "model-preview" => TaskKind::ModelPreview,
        "video-decode" => TaskKind::VideoDecode,
        "batch-convert" => TaskKind::BatchConvert,
        "maintenance" => TaskKind::Maintenance,
        "visual-backfill" => TaskKind::VisualBackfill,
        "watch-scan" => TaskKind::WatchScan,
        "embedding-backfill" => TaskKind::EmbeddingBackfill,
        "auto-tag" => TaskKind::AutoTag,
        "ai-analysis" => TaskKind::AiAnalysis,
        other => {
            // Custom kinds are stored as "plugin:<name>"; unknown slugs
            // without the prefix are still surfaced as Custom so the journal
            // never loses a row to a schema this build does not recognise.
            let name = other.strip_prefix("plugin:").unwrap_or(other);
            TaskKind::Custom(std::borrow::Cow::Owned(name.to_string()))
        }
    }
}

fn status_to_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn str_to_status(s: &str) -> TaskStatus {
    match s {
        "running" => TaskStatus::Running,
        "paused" => TaskStatus::Paused,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Failed,
    }
}
