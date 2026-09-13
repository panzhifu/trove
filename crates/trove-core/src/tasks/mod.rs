//! Backend task management: one registry for every long-running job the
//! library drives — imports, inbox collection, model/preview work,
//! maintenance — with progress, cancellation and lifecycle events in one
//! place.
//!
//! The manager is deliberately UI-free: jobs run on plain `std::thread`s and
//! report through an event queue the embedder polls
//! ([`TaskManager::poll_events`]), so no executor or UI types appear here.
//! Jobs are cooperative about cancellation (they check
//! [`JobContext::cancelled`] between units of work); a parse already in
//! flight cannot be interrupted.
//!
//! Threads own their resources. A database job opens its own SQLite
//! connection (the UI thread's [`crate::store::Store`] is thread-confined)
//! and commits in batched transactions — see [`import`].

use std::collections::{HashMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::new_id;
use uuid::Uuid;

pub mod import;
pub mod watch;

/// Handle identifying one job inside the manager.
pub type TaskId = Uuid;

/// How long a job waits between two throttled progress events.
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

/// The families of long-running work the library runs. The kind doubles as
/// the mutual-exclusion key: one job per kind at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Import,
    CollectInbox,
    ModelPreview,
    VideoDecode,
    BatchConvert,
    Maintenance,
    VisualBackfill,
    WatchScan,
}

impl TaskKind {
    /// Stable machine-readable name (logs, task lists, debugging).
    pub fn name(self) -> &'static str {
        match self {
            TaskKind::Import => "import",
            TaskKind::CollectInbox => "collect-inbox",
            TaskKind::ModelPreview => "model-preview",
            TaskKind::VideoDecode => "video-decode",
            TaskKind::BatchConvert => "batch-convert",
            TaskKind::Maintenance => "maintenance",
            TaskKind::VisualBackfill => "visual-backfill",
            TaskKind::WatchScan => "watch-scan",
        }
    }
}

/// Lifecycle status of a registered job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Point-in-time description of one job (task lists, debugging).
#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
    pub done: u64,
    pub total: u64,
    pub summary: Option<String>,
}

/// Everything that happened to a job, drained by the embedder.
#[derive(Debug, Clone)]
pub enum TaskEvent {
    Started {
        id: TaskId,
        kind: TaskKind,
    },
    Progress {
        id: TaskId,
        done: u64,
        total: u64,
    },
    /// The job produced a value; `summary` is a short human-readable line
    /// the job chose (English, core-side — the UI re-localizes from the
    /// returned value when it has one).
    Completed {
        id: TaskId,
        kind: TaskKind,
        summary: String,
    },
    Failed {
        id: TaskId,
        kind: TaskKind,
        error: String,
    },
    /// Cancelled before finishing; partial results may have been committed.
    Cancelled {
        id: TaskId,
        kind: TaskKind,
    },
}

struct JobState {
    kind: TaskKind,
    label: String,
    status: TaskStatus,
    done: u64,
    total: u64,
    summary: Option<String>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<TaskId, JobState>,
    events: VecDeque<TaskEvent>,
}

/// Shared registry of background jobs. Cheap to clone.
#[derive(Clone, Default)]
pub struct TaskManager {
    inner: Arc<Mutex<Inner>>,
}

/// Why a job could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// A job of the same kind is still running (one per kind at a time).
    AlreadyRunning,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `run` on a background thread under `kind`. Returns the task id
    /// plus a channel receiving the job's value on success (it closes with
    /// no value on failure, cancellation or panic).
    ///
    /// The job sees a [`JobContext`] for progress reporting and cooperative
    /// cancellation, and must return `Result` — `Err` becomes a
    /// [`TaskEvent::Failed`].
    pub fn start<T, F>(
        &self,
        kind: TaskKind,
        label: impl Into<String>,
        run: F,
    ) -> Result<(TaskId, std::sync::mpsc::Receiver<T>), StartError>
    where
        T: Send + 'static,
        F: FnOnce(&JobContext) -> Result<T, String> + Send + 'static,
    {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .jobs
            .values()
            .any(|j| j.kind == kind && j.status == TaskStatus::Running)
        {
            return Err(StartError::AlreadyRunning);
        }
        let id: TaskId = new_id();
        let cancel = Arc::new(AtomicBool::new(false));
        inner.jobs.insert(
            id,
            JobState {
                kind,
                label: label.into(),
                status: TaskStatus::Running,
                done: 0,
                total: 0,
                summary: None,
                cancel: cancel.clone(),
            },
        );
        inner.events.push_back(TaskEvent::Started { id, kind });
        let ctx_inner = self.inner.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = JobContext {
            id,
            kind,
            cancel,
            inner: ctx_inner,
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        };
        std::thread::Builder::new()
            .name(format!("trove-task-{}", kind.name()))
            .spawn(move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&ctx)));
                let mut inner = ctx.inner.lock().unwrap();
                let Some(state) = inner.jobs.get_mut(&ctx.id) else {
                    return;
                };
                match outcome {
                    Ok(Ok(value)) if !ctx.cancelled() => {
                        state.status = TaskStatus::Completed;
                        let summary = state.summary.clone().unwrap_or_default();
                        inner.events.push_back(TaskEvent::Completed {
                            id: ctx.id,
                            kind: ctx.kind,
                            summary,
                        });
                        drop(inner);
                        let _ = tx.send(value);
                    }
                    Ok(Ok(_)) => {
                        state.status = TaskStatus::Cancelled;
                        inner.events.push_back(TaskEvent::Cancelled {
                            id: ctx.id,
                            kind: ctx.kind,
                        });
                    }
                    Ok(Err(error)) => {
                        state.status = TaskStatus::Failed;
                        inner.events.push_back(TaskEvent::Failed {
                            id: ctx.id,
                            kind: ctx.kind,
                            error,
                        });
                    }
                    Err(_) => {
                        state.status = TaskStatus::Failed;
                        inner.events.push_back(TaskEvent::Failed {
                            id: ctx.id,
                            kind: ctx.kind,
                            error: "task panicked".into(),
                        });
                    }
                }
            })
            .expect("spawn task thread");
        Ok((id, rx))
    }

    /// Ask the job to stop at its next cancellation checkpoint.
    pub fn cancel(&self, id: TaskId) {
        let inner = self.inner.lock().unwrap();
        if let Some(state) = inner.jobs.get(&id) {
            state.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Whether a job of `kind` is currently running.
    pub fn is_running(&self, kind: TaskKind) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .jobs
            .values()
            .any(|j| j.kind == kind && j.status == TaskStatus::Running)
    }

    /// Whether this exact job is still running.
    pub fn is_task_running(&self, id: TaskId) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .jobs
            .get(&id)
            .is_some_and(|j| j.status == TaskStatus::Running)
    }

    /// Drain every accumulated event (all jobs, oldest first).
    pub fn poll_events(&self) -> Vec<TaskEvent> {
        let mut inner = self.inner.lock().unwrap();
        inner.events.drain(..).collect()
    }

    /// Snapshot of every known job. Finished jobs are dropped from the
    /// registry as they are observed here, so the map only holds what the
    /// embedder has not yet seen settle.
    pub fn snapshot(&self) -> Vec<TaskInfo> {
        let inner = self.inner.lock().unwrap();
        inner
            .jobs
            .iter()
            .map(|(id, j)| TaskInfo {
                id: *id,
                kind: j.kind,
                label: j.label.clone(),
                status: j.status,
                done: j.done,
                total: j.total,
                summary: j.summary.clone(),
            })
            .collect()
    }
}

/// Handle a running job uses to report progress and check cancellation.
pub struct JobContext {
    id: TaskId,
    kind: TaskKind,
    cancel: Arc<AtomicBool>,
    inner: Arc<Mutex<Inner>>,
    last_progress: Mutex<Instant>,
}

impl JobContext {
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn kind(&self) -> TaskKind {
        self.kind
    }

    /// A standalone context for tests that call job functions directly.
    #[cfg(test)]
    pub(crate) fn for_tests(cancelled: bool) -> Self {
        Self {
            id: new_id(),
            kind: TaskKind::Import,
            cancel: Arc::new(AtomicBool::new(cancelled)),
            inner: Arc::new(Mutex::new(Inner::default())),
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        }
    }

    /// Whether cancellation was requested. Jobs check this between units of
    /// work and unwind their loop early.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Update the total unit count once it is known (e.g. after expanding
    /// directories). Pushes an immediate progress event.
    pub fn set_total(&self, total: u64) {
        let done = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.jobs.get_mut(&self.id) {
                state.total = total;
            }
            let done = inner.jobs.get(&self.id).map(|s| s.done).unwrap_or(0);
            inner.events.push_back(TaskEvent::Progress {
                id: self.id,
                done,
                total,
            });
            done
        };
        let _ = done;
        *self.last_progress.lock().unwrap() = Instant::now();
    }

    /// Report progress. State updates always land; the event is throttled to
    /// [`PROGRESS_EVENT_INTERVAL`] except for the final unit.
    pub fn progress(&self, done: u64, total: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(state) = inner.jobs.get_mut(&self.id) {
            state.done = done;
            state.total = total;
        }
        let due = done == total || {
            let mut last = self.last_progress.lock().unwrap();
            if *last < Instant::now() - PROGRESS_EVENT_INTERVAL {
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if due {
            inner.events.push_back(TaskEvent::Progress {
                id: self.id,
                done,
                total,
            });
        }
    }

    /// Set the human-readable line carried by the completion event.
    pub fn set_summary(&self, summary: String) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(state) = inner.jobs.get_mut(&self.id) {
            state.summary = Some(summary);
        }
    }
}
