//! Task journal: persists task metadata to SQLite so the UI can surface
//! interrupted work after a restart and track retry history.
//!
//! The journal is written by the [`TaskManager`](crate::tasks::TaskManager) on
//! every task lifecycle transition (start, complete, fail, retry) — except for
//! a [`resident`](crate::tasks::TaskKind::is_resident) service, which is never
//! recorded at all. On library open, [`retire_resident_runs`] first folds the
//! rows an earlier build left behind for such a service, then
//! [`load_interrupted`] reads the tasks that were running or paused when the
//! previous process died, and [`Library::interrupted_tasks`]
//! (crate::library::Library) hands them to the task panel.
//!
//! What that is *not* is a resume. A row records the kind, label and progress
//! of a job, never the inputs that started it, and those live only in the
//! session that started it — so an interrupted job is reported, not replayed.
//! Anything that claims otherwise here has not checked the call sites.

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
///
/// A [`TaskKind::is_resident`] service is not recorded, and the reason is the
/// journal's own contract: a row means "this was running when a process ended,
/// say so on the next open". A resident service never ends on its own — quitting
/// the app does not run its wind-down — so recording it would answer that
/// promise with a lie once per launch. Every later write for such a task is an
/// `UPDATE ... WHERE task_id`, so skipping the insert leaves them all to match
/// nothing rather than to resurrect the row.
pub fn record_start(
    conn: &rusqlite::Connection,
    task_id: TaskId,
    kind: &TaskKind,
    label: &str,
    max_retries: u8,
) -> Result<()> {
    if kind.is_resident() {
        return Ok(());
    }
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

/// Fold the unfinished rows a previous process left behind for a resident
/// service, returning how many rows it took out of the interrupted set.
///
/// [`record_start`] refuses such rows now, but a library opened by an earlier
/// build has them — one per launch, each reported as work cut off mid-flight.
/// The kind is read back through [`str_to_kind`] and asked, rather than matched
/// in SQL, so this follows [`TaskKind::is_resident`] instead of repeating its
/// list: a second resident kind is retired the day it is added.
pub fn retire_resident_runs(conn: &rusqlite::Connection) -> Result<usize> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT kind FROM task_journal WHERE status IN ('running', 'paused')")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut unfinished = Vec::new();
    for row in rows {
        unfinished.push(row?);
    }
    drop(stmt);

    let mut retired = 0;
    for kind in unfinished {
        if !str_to_kind(&kind).is_resident() {
            continue;
        }
        retired += conn.execute(
            "UPDATE task_journal SET status = ?2, finished_at = ?3 \
             WHERE status IN ('running', 'paused') AND kind = ?1",
            params![
                kind,
                status_to_str(TaskStatus::Cancelled),
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
    }
    Ok(retired)
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
        TaskKind::Transcription => "transcribe".into(),
        TaskKind::Migration => "migration".into(),
        TaskKind::Export => "export".into(),
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
        "transcribe" => TaskKind::Transcription,
        "migration" => TaskKind::Migration,
        "export" => TaskKind::Export,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// The journal's whole contract, end to end: a start is readable as
    /// interrupted work, a terminal status takes it out again, and a custom
    /// kind comes back under the name it went in as.
    ///
    /// These three are the reason `load_interrupted` is worth calling at open —
    /// without them the module is a table that nothing has ever read back.
    #[test]
    fn a_running_task_is_reported_as_interrupted_until_it_settles() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let id = crate::model::new_id();

        record_start(conn, id, &TaskKind::Import, "import 1200 files", 2).unwrap();
        let found = load_interrupted(conn).unwrap();
        assert_eq!(found.len(), 1, "a running job must be readable");
        let entry = &found[0];
        assert_eq!(entry.task_id, id);
        assert_eq!(entry.kind, TaskKind::Import);
        assert_eq!(entry.label, "import 1200 files");
        assert_eq!(entry.status, TaskStatus::Running);
        assert_eq!(entry.max_retries, 2);
        assert_eq!(entry.retry_count, 0);
        assert!(entry.finished_at.is_none(), "it has not finished");

        record_status(
            conn,
            id,
            TaskStatus::Completed,
            1200,
            1200,
            Some("done"),
            None,
        )
        .unwrap();
        assert!(
            load_interrupted(conn).unwrap().is_empty(),
            "a settled job is not interrupted work"
        );
    }

    /// A failure that used its retries stays out of the interrupted set, and a
    /// retry increments the counter rather than starting a second row.
    #[test]
    fn retries_accumulate_on_one_row() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let id = crate::model::new_id();

        record_start(conn, id, &TaskKind::AiAnalysis, "ai analysis", 2).unwrap();
        record_retry(conn, id).unwrap();
        record_retry(conn, id).unwrap();
        let found = load_interrupted(conn).unwrap();
        assert_eq!(found.len(), 1, "a retry must not fork the row");
        assert_eq!(found[0].retry_count, 2);
        assert_eq!(found[0].status, TaskStatus::Running);

        record_status(conn, id, TaskStatus::Failed, 3, 10, None, Some("boom")).unwrap();
        assert!(load_interrupted(conn).unwrap().is_empty());
    }

    /// A plugin kind round-trips through the `"plugin:<name>"` slug, so a row
    /// written by a plugin is readable by name after a restart. An unknown slug
    /// still surfaces rather than being dropped.
    #[test]
    fn a_plugin_kind_round_trips_and_an_unknown_slug_survives() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let id = crate::model::new_id();
        let kind = TaskKind::Custom(std::borrow::Cow::Borrowed("notes-index"));

        record_start(conn, id, &kind, "index sidecars", 0).unwrap();
        let found = load_interrupted(conn).unwrap();
        assert_eq!(found[0].kind, kind, "the custom name did not round-trip");

        // A row written by a build whose kinds this one does not know still
        // reads back as Custom rather than vanishing from the report.
        conn.execute(
            "INSERT INTO task_journal (task_id, kind, label, status, started_at) \
             VALUES (?1, 'from-a-future-build', 'x', 'running', 'now')",
            [crate::model::new_id().to_string()],
        )
        .unwrap();
        let found = load_interrupted(conn).unwrap();
        assert!(
            found
                .iter()
                .any(|e| e.kind
                    == TaskKind::Custom(std::borrow::Cow::Borrowed("from-a-future-build"))),
            "an unrecognised kind was dropped instead of surfaced"
        );
    }

    /// A resident service is never recorded, because no quit writes its terminal
    /// row: a start that was journalled would read as interrupted work on every
    /// later open, one more per launch. The exclusion is per kind — a job of an
    /// ordinary kind beside it is still recorded — and the service's later
    /// writes must not resurrect the row they never created.
    #[test]
    fn a_resident_service_is_never_recorded() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let service = crate::model::new_id();

        record_start(conn, service, &TaskKind::WatchScan, "watch", 0).unwrap();
        assert!(
            load_interrupted(conn).unwrap().is_empty(),
            "the watch service left nothing behind"
        );
        record_status(conn, service, TaskStatus::Cancelled, 0, 0, None, None).unwrap();
        record_retry(conn, service).unwrap();

        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM task_journal", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "not even a row the terminal writes could have updated"
        );

        let job = crate::model::new_id();
        record_start(conn, job, &TaskKind::Import, "import", 0).unwrap();
        assert_eq!(
            load_interrupted(conn).unwrap().len(),
            1,
            "an ordinary job is still journalled"
        );
    }

    /// The cleanup for a library an earlier build already left dirty: the
    /// service rows no process could close are folded out of the interrupted
    /// set, while a genuinely unfinished import is left to be reported — and a
    /// settled row of either kind is not touched at all.
    #[test]
    fn legacy_service_rows_are_retired_and_a_real_job_still_shows() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let insert = |kind: &str, status: &str| {
            conn.execute(
                "INSERT INTO task_journal (task_id, kind, label, status, started_at) \
                 VALUES (?1, ?2, ?3, ?4, 'then')",
                params![crate::model::new_id().to_string(), kind, "x", status],
            )
            .unwrap();
        };
        insert("watch-scan", "running");
        insert("watch-scan", "paused");
        insert("watch-scan", "completed");
        insert("import", "running");

        assert_eq!(
            retire_resident_runs(conn).unwrap(),
            2,
            "both unfinished service rows, and no others"
        );
        let found = load_interrupted(conn).unwrap();
        assert_eq!(found.len(), 1, "only the import is interrupted work");
        assert_eq!(found[0].kind, TaskKind::Import);
        assert_eq!(
            retire_resident_runs(conn).unwrap(),
            0,
            "a second open finds nothing left to retire"
        );
    }
}
