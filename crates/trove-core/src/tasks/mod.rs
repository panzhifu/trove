//! Backend task management: one registry for every long-running job the
//! library drives — imports, inbox collection, model/preview work,
//! maintenance — with progress, cancellation and lifecycle events in one
//! place.
//!
//! The manager is deliberately UI-free: jobs run on a shared thread pool
//! ([`TaskPool`]) and report through an event queue the embedder polls
//! ([`TaskManager::poll_events`], or [`TaskManager::poll_events_for`] to take
//! one job's events without disturbing the others'), or waits on
//! ([`TaskManager::wait_events_for`], which parks the calling thread until that
//! job has something to report), so no executor or UI types appear here. Jobs
//! are cooperative about cancellation and pausing (they check
//! [`JobContext::cancelled`] and [`JobContext::park_if_paused`] between units of
//! work); a parse already in flight can be neither interrupted nor held, so
//! both take effect only at the next checkpoint.
//!
//! Threads own their resources. A database job opens its own SQLite
//! connection (the UI thread's [`crate::store::Store`] is thread-confined)
//! and commits in batched transactions — see [`import`].
//!
//! The job registry (what `snapshot`/`is_running` read, and where `start`
//! inserts) and the event queue (what `poll_events` drains) live behind two
//! separate mutexes, so a job reporting progress never waits on the UI
//! draining events and the UI never waits on a job's registry write. When
//! both are needed the registry lock is taken first; no path takes them the
//! other way round. The queue holds one bucket per job (see [`EventQueue`]),
//! so a watcher's cost tracks its own job's unread events, never everyone
//! else's, and a job nobody reads cannot push another job's events out.
//!
//! Progress events are *lossy*: only the most recent [`TaskEvent::Progress`]
//! per job is kept, so a flood of progress updates cannot crowd out terminal
//! events. Terminal events (Started, Completed, Failed, Cancelled, Paused,
//! Resumed, Retrying) are never evicted.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::model::new_id;
use crate::store::task_journal;
use uuid::Uuid;

pub mod ai_analysis;
pub mod embed;
pub mod export;
pub mod ignore;
pub mod import;
pub mod migration;
pub mod transcription;
pub mod watch;

/// Handle identifying one job inside the manager.
pub type TaskId = Uuid;

/// How long a job waits between two throttled progress events.
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on one job's unread *terminal* events. Each job owns a bucket
/// (see [`EventQueue`]) and only that bucket is bounded. Progress events are
/// stored separately as a single latest-value slot per job (lossy: each new
/// Progress overwrites the previous one), so they never count against this
/// bound. A resident job nobody polls (the folder watcher, one-shot
/// maintenance) fills its own bucket with terminal events and evicts its own
/// stale ones — it can no longer push another job's terminal event out of a
/// shared queue the way the old global bound did. The live count a job
/// reports always reflects the freshest state, so losing a stale progress
/// event is invisible, whereas losing memory is not. Terminal events
/// (Started, Completed, Failed, Cancelled, Paused, Resumed, Retrying) are
/// never evicted; at the backend's rate this holds many minutes of backlog.
const MAX_EVENTS_PER_JOB: usize = 128;

/// How many jobs keep a bucket after their events stop being read. Buckets are
/// created per job id, so the ones nobody polls (a watch scan, a maintenance
/// run, the job before last) would otherwise accumulate for the life of the
/// process. When the set is full the *oldest* bucket is discarded whole, which
/// is by construction the job that has gone unread the longest: taking a bucket
/// removes its entry, so a job somebody is watching re-stamps itself with the
/// newest sequence on its next event and never sorts as the oldest.
///
/// The two bounds together keep the footprint where the shared queue had it:
/// 16 buckets × 128 events × 48 B ≈ the old 2 048-event global cap.
const MAX_TRACKED_JOBS: usize = 16;

/// The families of long-running work the library runs. The kind doubles as
/// the mutual-exclusion key: one job per kind at a time.
///
/// Built-in variants cover the library's own jobs; [`TaskKind::Custom`] lets
/// plugins register their own task types with the same scheduling and
/// persistence guarantees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Import,
    CollectInbox,
    ModelPreview,
    VideoDecode,
    BatchConvert,
    Maintenance,
    VisualBackfill,
    WatchScan,
    EmbeddingBackfill,
    /// Asking a chat model to tag assets. Serial by nature — one request per
    /// asset — so it is the longest-running job the library has.
    AutoTag,
    /// Multimodal AI analysis: description, tags, and rating from a vision
    /// model. One API call per asset.
    AiAnalysis,
    /// Speech-to-text: an audio or video asset's track goes to a cloud
    /// recogniser, the transcript comes back and is filed on the asset.
    /// Network-bound like analysis, but with a local ffmpeg transcode in
    /// front of every upload.
    Transcription,
    /// Migrating from another asset manager (Eagle / Billfish): scan the
    /// foreign library, link-import its files, then write the carried
    /// metadata back. Mutually exclusive with a plain import because both
    /// drive the same store the same way.
    Migration,
    /// Handing asset files to a user-chosen folder: originals copied,
    /// images re-encoded to a chosen raster format, videos transcoded or
    /// remuxed through the system ffmpeg. Reads the library, writes only
    /// outside it.
    Export,
    /// A plugin-registered task type. The string is the plugin's stable name
    /// for the kind, used in logs, the journal, and mutual-exclusion checks.
    Custom(Cow<'static, str>),
}

impl TaskKind {
    /// Stable machine-readable name (logs, task lists, debugging).
    pub fn name(&self) -> Cow<'_, str> {
        match self {
            TaskKind::Import => Cow::Borrowed("import"),
            TaskKind::CollectInbox => Cow::Borrowed("collect-inbox"),
            TaskKind::ModelPreview => Cow::Borrowed("model-preview"),
            TaskKind::VideoDecode => Cow::Borrowed("video-decode"),
            TaskKind::BatchConvert => Cow::Borrowed("batch-convert"),
            TaskKind::Maintenance => Cow::Borrowed("maintenance"),
            TaskKind::VisualBackfill => Cow::Borrowed("visual-backfill"),
            TaskKind::WatchScan => Cow::Borrowed("watch-scan"),
            TaskKind::EmbeddingBackfill => Cow::Borrowed("embedding-backfill"),
            TaskKind::AutoTag => Cow::Borrowed("auto-tag"),
            TaskKind::AiAnalysis => Cow::Borrowed("ai-analysis"),
            TaskKind::Transcription => Cow::Borrowed("transcribe"),
            TaskKind::Migration => Cow::Borrowed("migration"),
            TaskKind::Export => Cow::Borrowed("export"),
            TaskKind::Custom(name) => Cow::Borrowed(name),
        }
    }

    /// A resident service rather than a job with an end.
    ///
    /// The folder watcher loops until it is cooperatively cancelled, so it only
    /// ever stops by a library swap — quitting the process does not run its
    /// wind-down. Recording its start would therefore leave a `running` row
    /// behind on every launch, and the next process would report it as work cut
    /// off mid-flight: the task panel's warning strip and a status bar that
    /// never reads "idle" again. A service has no work to be cut off, so it is
    /// not journalled.
    pub fn is_resident(&self) -> bool {
        matches!(self, TaskKind::WatchScan)
    }
}

/// Lifecycle status of a registered job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    /// Temporarily held at a checkpoint by [`TaskManager::pause`]; it still
    /// occupies its kind's single slot and resumes with
    /// [`TaskManager::resume`] (or unwinds if cancelled while paused).
    Paused,
    Completed,
    Failed,
    Cancelled,
}

/// How many times a failed task should be retried before giving up.
///
/// Passed to [`TaskManager::start_with_retry`]; the manager re-queues the
/// task on failure until the budget is exhausted. Each retry gets a fresh
/// closure from the factory, so the task can re-open files, re-connect
/// sockets, etc.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// How many times to retry after the first failure. Zero means no retry
    /// (equivalent to [`TaskManager::start`]).
    pub max_retries: u8,
    /// How long to wait between retries. Defaults to 2 seconds.
    pub backoff: Duration,
}

impl RetryPolicy {
    /// No retries: fail immediately on the first error.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            backoff: Duration::from_secs(2),
        }
    }

    /// Retry up to `n` times with a 2-second backoff.
    pub fn times(n: u8) -> Self {
        Self {
            max_retries: n,
            backoff: Duration::from_secs(2),
        }
    }
}

/// Point-in-time description of one job (task lists, debugging).
#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub priority: TaskPriority,
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
    /// Handed the [`TaskStatus::Paused`] state; the job parks at its next
    /// checkpoint and stops reporting progress until it is resumed.
    Paused {
        id: TaskId,
        kind: TaskKind,
    },
    /// Left [`TaskStatus::Paused`]; the job resumes from its next checkpoint.
    Resumed {
        id: TaskId,
        kind: TaskKind,
    },
    /// The job failed but has retries remaining; it will be re-queued after
    /// a short backoff. The UI can use this to show a "retrying" indicator
    /// instead of a final failure.
    Retrying {
        id: TaskId,
        kind: TaskKind,
        attempt: u8,
        max_retries: u8,
    },
}

impl TaskEvent {
    /// The job this event belongs to. Every variant carries one, which is what
    /// lets the queue route it into that job's own bucket.
    pub fn task_id(&self) -> TaskId {
        match self {
            TaskEvent::Started { id, .. }
            | TaskEvent::Progress { id, .. }
            | TaskEvent::Completed { id, .. }
            | TaskEvent::Failed { id, .. }
            | TaskEvent::Cancelled { id, .. }
            | TaskEvent::Paused { id, .. }
            | TaskEvent::Resumed { id, .. }
            | TaskEvent::Retrying { id, .. } => *id,
        }
    }
}

struct JobState {
    kind: TaskKind,
    label: String,
    priority: TaskPriority,
    status: TaskStatus,
    done: u64,
    total: u64,
    summary: Option<String>,
    cancel: Arc<AtomicBool>,
    pause: Arc<PauseSignal>,
    retry: Option<RetryState>,
}

/// Live retry tracking for one job. Present only when the job was started
/// via [`TaskManager::start_with_retry`].
struct RetryState {
    remaining: u8,
    max: u8,
    backoff: Duration,
}

/// Cooperative pause gate shared between the manager and one job's thread.
///
/// [`TaskManager::pause`] only sets a flag; the running job notices it at its
/// next checkpoint and blocks on the condvar until either [`TaskManager::resume`]
/// (or a cancel, which also wakes it) fires. Parking never happens inside a
/// committed transaction, so a paused job holds no database lock.
struct PauseSignal {
    paused: AtomicBool,
    lock: Mutex<()>,
    cond: Condvar,
}

impl PauseSignal {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            lock: Mutex::new(()),
            cond: Condvar::new(),
        }
    }

    fn request_pause(&self) {
        // Flip under the lock so a thread that is between its `while` check
        // and `cond.wait` (and therefore still holding the lock) cannot miss
        // the transition.
        let _guard = crate::sync::lock(&self.lock);
        self.paused.store(true, Ordering::SeqCst);
    }

    fn request_resume(&self) {
        {
            let _guard = crate::sync::lock(&self.lock);
            self.paused.store(false, Ordering::SeqCst);
        }
        self.cond.notify_all();
    }

    /// Wake a parked thread so it re-checks and unwinds on cancellation. The
    /// cancellation flag itself is owned by the job state and already set;
    /// briefly taking the lock guarantees a `while`-check in flight sees it
    /// before `notify_all` can otherwise be lost against a thread not yet
    /// parked.
    fn wake(&self) {
        drop(crate::sync::lock(&self.lock));
        self.cond.notify_all();
    }

    /// Block while paused. Returns once the job is resumed or cancellation is
    /// requested; the caller then runs its own cancellation check.
    fn park(&self, cancel: &AtomicBool) {
        if !self.paused.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = crate::sync::lock(&self.lock);
        while self.paused.load(Ordering::SeqCst) && !cancel.load(Ordering::Relaxed) {
            guard = self.cond.wait(guard).unwrap();
        }
    }
}

/// Every job's unread events, one bucket per job behind one mutex and one
/// condvar.
///
/// The old shape was a single FIFO the jobs shared, which cost each watcher a
/// scan of the whole queue to pick out its own events, and let a job nobody
/// polls push another job's terminal event out of the shared bound. Per-job
/// buckets fix both: publishing appends to one bucket, taking one moves that
/// bucket out alone, and the bound is per bucket, so a job can only evict its
/// own stale events.
///
/// Progress events are stored separately as a single latest-value slot per
/// job. Each new [`TaskEvent::Progress`] overwrites the previous one (lossy),
/// so a flood of progress updates cannot crowd out terminal events. The
/// bounded VecDeque only holds terminal events (Started, Completed, Failed,
/// Cancelled, Paused, Resumed, Retrying), which are rare.
///
/// A bucket exists while it has unread events; [`EventQueue::take`] removes the
/// entry, so the common case (a watcher draining a live job every poll) keeps
/// the map at one or two entries no matter how many jobs the session has run.
/// Jobs that are never polled are bounded by [`MAX_TRACKED_JOBS`].
#[derive(Default)]
struct EventQueue {
    state: Mutex<QueueState>,
    /// Signalled whenever a bucket gains an event, so [`EventQueue::wait`] can
    /// sleep instead of polling. One condvar for the whole map: `std` cannot
    /// wake only the waiters interested in one key, so a push wakes every
    /// waiter and each re-checks its own bucket — the spurious wakeups cost a
    /// lock handoff and a lookup, and there are only a handful of watchers.
    ready: Condvar,
}

/// The buckets, plus what it takes to bound them.
#[derive(Default)]
struct QueueState {
    /// `id` → (sequence in which the bucket was created, its unread terminal
    /// events, latest progress if any). The sequence makes "drop the job that
    /// has gone unread the longest" a scan of a handful of entries instead of
    /// a second collection to keep in sync with the map.
    ///
    /// An entry is never present-and-empty: every take removes the entry with
    /// its events, which is what lets [`EventQueue::wait`] sleep on the key's
    /// absence rather than on a length.
    by_job: HashMap<TaskId, (u64, VecDeque<TaskEvent>, Option<TaskEvent>)>,
    next_seq: u64,
}

impl EventQueue {
    /// Append one event to its job's bucket and wake the waiters.
    ///
    /// Progress events are stored as a single latest-value slot: each new
    /// Progress overwrites the previous one, so a flood of progress updates
    /// cannot crowd out terminal events. Terminal events are appended to the
    /// bounded VecDeque as before.
    ///
    /// Callers hold no lock this needs; when invoked from [`TaskManager::start`]
    /// the registry lock is already held, preserving the `jobs → events` order.
    fn push(&self, event: TaskEvent) {
        {
            let mut state = crate::sync::lock(&self.state);
            let (bucket, progress) = state.entry(event.task_id());
            if matches!(event, TaskEvent::Progress { .. }) {
                // Lossy: overwrite the previous progress, never append.
                *progress = Some(event);
            } else {
                if bucket.len() >= MAX_EVENTS_PER_JOB {
                    bucket.pop_front();
                }
                bucket.push_back(event);
            }
            state.evict_stale();
        }
        // Notified with the lock released: a waiter either sees the event
        // before it parks (its predicate is checked under this same mutex) or
        // is parked and takes this wake-up, so neither order can lose it.
        self.ready.notify_all();
    }

    /// Take `id`'s events, oldest first, leaving every other job's alone.
    fn take(&self, id: TaskId) -> Vec<TaskEvent> {
        let mut state = crate::sync::lock(&self.state);
        state.take_for(id)
    }

    /// Block until `id` has events, or `timeout` elapses, and take them.
    ///
    /// This parks the calling thread, so it is for embedders that own their
    /// thread (a CLI's progress loop, a test). A watcher running on a shared
    /// thread pool should keep polling [`EventQueue::take`] instead: gpui's
    /// background pool is a fixed set of workers, and a parked watcher is a
    /// worker no other background task can use.
    fn wait(&self, id: TaskId, timeout: Duration) -> Vec<TaskEvent> {
        let state = crate::sync::lock(&self.state);
        // `wait_timeout_while` re-checks the predicate on every wake-up, so a
        // push for another job — or a spurious wake — goes back to sleep.
        let (mut state, _waited) = self
            .ready
            .wait_timeout_while(state, timeout, |state| !state.by_job.contains_key(&id))
            .unwrap();
        state.take_for(id)
    }

    /// Every job's unread events. Order within one job is kept; there is no
    /// global order to keep any more, since the buckets are separate.
    ///
    /// Takes the map out rather than emptying the buckets in place: a present
    /// bucket is always non-empty, which is what [`EventQueue::wait`] sleeps
    /// on.
    fn take_all(&self) -> Vec<TaskEvent> {
        let mut state = crate::sync::lock(&self.state);
        let ids: Vec<TaskId> = state.by_job.keys().copied().collect();
        let mut out = Vec::new();
        for id in ids {
            out.extend(state.take_for(id));
        }
        out
    }
}

impl QueueState {
    /// The job's bucket, created — and stamped with the next sequence number —
    /// if it does not exist yet. Returns the terminal event deque and the
    /// latest-progress slot separately, so [`EventQueue::push`] can route
    /// progress events into the lossy slot.
    fn entry(&mut self, id: TaskId) -> (&mut VecDeque<TaskEvent>, &mut Option<TaskEvent>) {
        match self.by_job.entry(id) {
            Entry::Occupied(entry) => {
                let (_, terminal, progress) = entry.into_mut();
                (terminal, progress)
            }
            Entry::Vacant(entry) => {
                let seq = self.next_seq;
                self.next_seq += 1;
                let (_, terminal, progress) = entry.insert((seq, VecDeque::new(), None));
                (terminal, progress)
            }
        }
    }

    /// Take one job's bucket out of the map: terminal events oldest first,
    /// then the latest progress (if any) appended at the end.
    fn take_for(&mut self, id: TaskId) -> Vec<TaskEvent> {
        self.by_job
            .remove(&id)
            .map_or_else(Vec::new, |(_, mut terminal, progress)| {
                let mut out: Vec<TaskEvent> = terminal.drain(..).collect();
                if let Some(progress) = progress {
                    out.push(progress);
                }
                out
            })
    }

    /// Drop whole buckets, oldest-created first, until the set fits again.
    fn evict_stale(&mut self) {
        while self.by_job.len() > MAX_TRACKED_JOBS {
            match self
                .by_job
                .iter()
                .min_by_key(|(_, (seq, _, _))| *seq)
                .map(|(id, _)| *id)
            {
                Some(id) => {
                    self.by_job.remove(&id);
                }
                None => break,
            }
        }
    }
}

/// Priority of a task. Higher-priority tasks are listed first in snapshots,
/// and `High` work also jumps ahead of the `Normal` queue in the pool: a
/// model preview's parse is the reason a viewport is showing a placeholder,
/// and it should not line up behind a backfill that can wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TaskPriority {
    Low,
    #[default]
    Normal,
    High,
}

/// A shared thread pool for running background tasks. Workers pull closures
/// from a common queue; the pool is created once and shared across all tasks
/// the manager starts, so the thread count stays bounded no matter how many
/// jobs run concurrently.
pub struct TaskPool {
    inner: Arc<TaskPoolInner>,
}

/// The two queues workers pull from: the priority lane is drained before
/// the normal one, so interactive work does not queue behind batch work.
/// Within a lane order is arrival order.
struct WorkQueues {
    high: VecDeque<Work>,
    normal: VecDeque<Work>,
}

impl WorkQueues {
    fn is_empty(&self) -> bool {
        self.high.is_empty() && self.normal.is_empty()
    }
}

struct TaskPoolInner {
    queue: Mutex<Option<WorkQueues>>,
    /// Signalled when a new task is enqueued; workers sleep on it when the
    /// queue is empty.
    notify: Condvar,
}

type Work = Box<dyn FnOnce() + Send + 'static>;

impl TaskPool {
    /// Create a pool with `num_threads` workers. Each worker is a named
    /// daemon thread (`trove-pool-N`) that pulls closures from the shared
    /// queue and runs them to completion.
    pub fn new(num_threads: usize) -> Self {
        let inner = Arc::new(TaskPoolInner {
            queue: Mutex::new(Some(WorkQueues {
                high: VecDeque::new(),
                normal: VecDeque::new(),
            })),
            notify: Condvar::new(),
        });
        for i in 0..num_threads {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name(format!("trove-pool-{i}"))
                .spawn(move || inner.worker_loop())
                .expect("spawn pool worker");
        }
        Self { inner }
    }

    /// Submit a closure for execution. Returns immediately; the closure runs
    /// on the next available worker.
    pub fn execute(&self, work: impl FnOnce() + Send + 'static) {
        self.execute_with_priority(work, TaskPriority::Normal)
    }

    /// [`TaskPool::execute`], into the lane the priority names. A `High`
    /// closure is picked up before every queued `Normal` one — it does not
    /// preempt a closure that already runs.
    pub fn execute_with_priority(
        &self,
        work: impl FnOnce() + Send + 'static,
        priority: TaskPriority,
    ) {
        let mut queue = crate::sync::lock(&self.inner.queue);
        match queue.as_mut() {
            Some(q) => {
                let work: Work = Box::new(work);
                match priority {
                    TaskPriority::High => q.high.push_back(work),
                    _ => q.normal.push_back(work),
                }
                self.inner.notify.notify_one();
            }
            None => {
                // Pool was shut down. Drop the work silently — callers should
                // not submit after the pool is dropped.
            }
        }
    }
}

impl TaskPoolInner {
    /// The next closure to run: the priority lane first, then the normal
    /// one. `None` means both lanes are empty.
    fn pop(queue: &mut Option<WorkQueues>) -> Option<Work> {
        let q = queue.as_mut()?;
        q.high.pop_front().or_else(|| q.normal.pop_front())
    }

    fn worker_loop(&self) {
        loop {
            let work = {
                let mut queue = crate::sync::lock(&self.queue);
                loop {
                    match Self::pop(&mut queue) {
                        Some(work) => break work,
                        None => match queue.as_ref() {
                            // Queue is empty but still alive: wait for work.
                            // `wait_while` re-checks on every wake so a
                            // spurious notify cannot pop an empty queue.
                            Some(_) => {
                                queue = self
                                    .notify
                                    .wait_while(queue, |q| {
                                        q.as_ref().is_some_and(|inner| inner.is_empty())
                                    })
                                    .unwrap();
                            }
                            None => return, // pool shut down
                        },
                    }
                }
            };
            // Run outside the lock so other workers can enqueue / dequeue.
            work();
        }
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        // Setting the queue to None is the shutdown signal workers check.
        // notify_all wakes every sleeper so it can see the flag and exit.
        *crate::sync::lock(&self.inner.queue) = None;
        self.inner.notify.notify_all();
    }
}

/// A thread-safe SQLite connection for the task journal. The manager holds
/// one of these when persistence is enabled; it opens its own connection to
/// the library database (WAL mode allows concurrent readers/writers) and
/// writes only to the `task_journal` table, so it never contends with the
/// main [`Store`](crate::store::Store) connection.
type JournalConn = Arc<Mutex<rusqlite::Connection>>;

/// One journal write, with its failure reported instead of discarded.
///
/// Every lifecycle transition goes through here, because a journal error the
/// manager swallows is the one background-task failure the *next* process cannot
/// see: if `record_start` never landed, nothing at startup can report the job
/// that was cut off mid-import, and the files it staged have no record of who
/// owns them. `let _ =` made that invisible by construction.
fn journal_write(
    journal: &Option<JournalConn>,
    degraded: &AtomicBool,
    task_id: TaskId,
    what: &str,
    run: impl FnOnce(&rusqlite::Connection) -> crate::error::Result<()>,
) {
    // No journal attached is the documented in-memory-only mode, not a failure.
    let Some(conn) = journal else {
        return;
    };
    let Ok(conn) = conn.lock() else {
        report_journal_failure(degraded, task_id, what, "journal lock is poisoned");
        return;
    };
    if let Err(error) = run(&conn) {
        report_journal_failure(degraded, task_id, what, &error.to_string());
    }
}

/// Raise the degraded flag and log the first reason only. A broken journal fails
/// on every transition of every job, so one warning per write would bury the
/// single fact the user needs — that interrupted tasks cannot be reported —
/// under a hundred identical lines.
fn report_journal_failure(degraded: &AtomicBool, task_id: TaskId, what: &str, reason: &str) {
    if !degraded.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            %task_id,
            what,
            reason,
            "task journal is not recording; interrupted work cannot be reported after a restart"
        );
    }
}

/// Shared registry of background jobs. Cheap to clone.
///
/// Two independent locks: `jobs` guards the registry, `events` guards the
/// per-job buckets. Splitting them keeps a job's per-file `progress` call from
/// contending with the embedder's drain. Lock order, when both are held, is
/// always `jobs` → `events`. A third optional lock, `journal`, guards the
/// SQLite connection for task persistence. Terminal writes happen after `jobs`
/// is released; the two *start* writes happen while it is held, so the order
/// that actually exists is `jobs` → `journal`. Nothing takes `journal` and then
/// `jobs`, which is the only shape that could deadlock, and a journal write is
/// one `INSERT` — it cannot hold the registry up long enough to matter.
///
/// The `pool` is the shared thread pool all tasks run on; it is created once
/// when the manager is assembled and shared by reference (via `Arc<TaskPool>`)
/// with every closure the manager submits.
#[derive(Clone)]
pub struct TaskManager {
    jobs: Arc<Mutex<HashMap<TaskId, JobState>>>,
    events: Arc<EventQueue>,
    journal: Option<JournalConn>,
    /// Set once a task-journal write fails; see [`TaskManager::journal_degraded`].
    journal_degraded: Arc<AtomicBool>,
    pool: Arc<TaskPool>,
    /// Custom task kinds a plugin has declared, so [`TaskKind::Custom`] can be
    /// checked against something. Set once at library open, before the manager
    /// is shared — see [`TaskManager::declare_task_kinds`].
    declared: Arc<HashSet<&'static str>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(EventQueue::default()),
            journal: None,
            journal_degraded: Arc::new(AtomicBool::new(false)),
            pool: Arc::new(TaskPool::new(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .max(2),
            )),
            declared: Arc::new(HashSet::new()),
        }
    }
}

/// Why a job could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// A job of the same kind is still running (one per kind at a time).
    AlreadyRunning,
    /// The kind is [`TaskKind::Custom`] and no enabled plugin declared that
    /// name. Either the plugin is switched off or the string is a typo, and
    /// letting either through would hand a slot, a journal row and a panel row
    /// to a job nobody owns. The name is in the log, not here, so this stays
    /// `Copy`.
    UndeclaredKind,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the custom task kinds plugins contribute, so a job may be
    /// started under one. Call before the manager is shared — the registry
    /// is read once, exactly like the import pipeline reads its stages, so a
    /// plugin registered after this point cannot schedule work in this
    /// process.
    pub fn declare_task_kinds(&mut self, kinds: &[&'static str]) {
        self.declared = Arc::new(kinds.iter().copied().collect());
    }

    /// A custom kind must be one an enabled plugin claimed. Built-in kinds are
    /// always allowed, and an empty declaration set only rejects custom ones —
    /// which is what a process with no plugins (the CLI) looks like.
    fn gate_kind(&self, kind: &TaskKind) -> Result<(), StartError> {
        if let TaskKind::Custom(name) = kind
            && !self.declared.contains(name.as_ref())
        {
            tracing::warn!(
                kind = %name,
                "refusing to start a custom task kind no enabled plugin declared"
            );
            return Err(StartError::UndeclaredKind);
        }
        Ok(())
    }

    /// Attach a journal connection so tasks are persisted to the `task_journal`
    /// table. Called once at library open; without this, tasks are tracked
    /// in-memory only (the pre-persistence behaviour).
    pub fn set_journal(&mut self, conn: rusqlite::Connection) {
        self.journal = Some(Arc::new(Mutex::new(conn)));
    }

    /// Whether a task-journal write has failed in this process.
    ///
    /// `false` means every transition of every job was recorded, so the rows a
    /// later process reads are complete. `true` means they are not: a job that
    /// was interrupted may have no row at all, and the task panel says so rather
    /// than letting an absent row read as "nothing was running".
    pub fn journal_degraded(&self) -> bool {
        self.journal_degraded.load(Ordering::Relaxed)
    }

    /// Spawn `run` on the shared thread pool under `kind`. Returns the task id
    /// plus a channel receiving the job's value on success (it closes with
    /// no value on failure, cancellation or panic).
    ///
    /// The job sees a [`JobContext`] for progress reporting and cooperative
    /// cancellation, and must return `Result` — `Err` becomes a
    /// [`TaskEvent::Failed`].
    ///
    /// Uses [`TaskPriority::Normal`]; for a different priority use
    /// [`TaskManager::start_with_priority`].
    pub fn start<T, F>(
        &self,
        kind: TaskKind,
        label: impl Into<String>,
        run: F,
    ) -> Result<(TaskId, std::sync::mpsc::Receiver<T>), StartError>
    where
        T: Send + 'static,
        F: FnOnce(&JobContext) -> crate::error::Result<T> + Send + 'static,
    {
        self.start_with_priority(kind, label, TaskPriority::Normal, run)
    }

    /// Like [`TaskManager::start`], but with an explicit priority. Higher-
    /// priority tasks are listed first in [`TaskManager::snapshot`] and may
    /// be scheduled before lower-priority ones in the future.
    pub fn start_with_priority<T, F>(
        &self,
        kind: TaskKind,
        label: impl Into<String>,
        priority: TaskPriority,
        run: F,
    ) -> Result<(TaskId, std::sync::mpsc::Receiver<T>), StartError>
    where
        T: Send + 'static,
        F: FnOnce(&JobContext) -> crate::error::Result<T> + Send + 'static,
    {
        self.gate_kind(&kind)?;
        let mut jobs = crate::sync::lock(&self.jobs);
        if jobs
            .values()
            .any(|j| j.kind == kind && matches!(j.status, TaskStatus::Running | TaskStatus::Paused))
        {
            return Err(StartError::AlreadyRunning);
        }
        // Finished jobs leave the registry here rather than never: every
        // state lives on only until the next job starts, which bounds the map
        // to the live job plus the last one of each kind instead of every job
        // the process ever ran. Their terminal events are already queued in
        // `events`, whose buckets outlive the registry entries that produced
        // them — nothing a consumer reads through the registry is lost with
        // the entries. A *paused* job stays:
        // it has not finished, still owns its slot, and may yet be resumed.
        jobs.retain(|_, j| matches!(j.status, TaskStatus::Running | TaskStatus::Paused));
        let id: TaskId = new_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(PauseSignal::new());
        let label = label.into();
        jobs.insert(
            id,
            JobState {
                kind: kind.clone(),
                label: label.clone(),
                priority,
                status: TaskStatus::Running,
                done: 0,
                total: 0,
                summary: None,
                cancel: cancel.clone(),
                pause: pause.clone(),
                retry: None,
            },
        );
        // Queued while the registry lock is held so no other job can slip an
        // event in ahead of this job's `Started`.
        self.events.push(TaskEvent::Started {
            id,
            kind: kind.clone(),
        });
        // Persist to journal before spawning, so a crash mid-spawn leaves a
        // "running" row the next startup can surface.
        journal_write(
            &self.journal,
            &self.journal_degraded,
            id,
            "record_start",
            |conn| task_journal::record_start(conn, id, &kind, &label, 0),
        );
        drop(jobs);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = JobContext {
            id,
            kind: kind.clone(),
            cancel,
            pause,
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        };
        let journal = self.journal.clone();
        let degraded = self.journal_degraded.clone();
        self.pool.execute_with_priority(
            move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&ctx)));
                // Extract the terminal state and the success value (if any)
                // while the registry lock is held, then drop it before the
                // journal write and the channel send.
                let (status, summary, error, done, total, value) = {
                    let mut jobs = crate::sync::lock(&ctx.jobs);
                    let Some(state) = jobs.get_mut(&ctx.id) else {
                        return;
                    };
                    match outcome {
                        Ok(Ok(value)) if !ctx.cancelled() => {
                            state.status = TaskStatus::Completed;
                            let summary = state.summary.clone().unwrap_or_default();
                            ctx.events.push(TaskEvent::Completed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                summary: summary.clone(),
                            });
                            (
                                TaskStatus::Completed,
                                Some(summary),
                                None,
                                state.done,
                                state.total,
                                Some(value),
                            )
                        }
                        Ok(Ok(_)) => {
                            state.status = TaskStatus::Cancelled;
                            ctx.events.push(TaskEvent::Cancelled {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                            });
                            (
                                TaskStatus::Cancelled,
                                None,
                                None,
                                state.done,
                                state.total,
                                None,
                            )
                        }
                        Ok(Err(error)) => {
                            // The event and the journal carry text, not the error
                            // value: both outlive the job and are read by other
                            // processes, so one message has to be materialized here.
                            let error = error.to_string();
                            state.status = TaskStatus::Failed;
                            ctx.events.push(TaskEvent::Failed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                error: error.clone(),
                            });
                            (
                                TaskStatus::Failed,
                                None,
                                Some(error),
                                state.done,
                                state.total,
                                None,
                            )
                        }
                        Err(_) => {
                            state.status = TaskStatus::Failed;
                            ctx.events.push(TaskEvent::Failed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                error: "task panicked".into(),
                            });
                            (
                                TaskStatus::Failed,
                                None,
                                Some("task panicked".into()),
                                state.done,
                                state.total,
                                None,
                            )
                        }
                    }
                };
                // Send the value before the journal write: the watcher picks
                // up the outcome from the channel, so it must arrive first.
                if let Some(value) = value {
                    let _ = tx.send(value);
                }
                journal_write(&journal, &degraded, ctx.id, "record_status", |conn| {
                    task_journal::record_status(
                        conn,
                        ctx.id,
                        status,
                        done,
                        total,
                        summary.as_deref(),
                        error.as_deref(),
                    )
                });
            },
            priority,
        );
        Ok((id, rx))
    }

    /// Spawn `run` with automatic retries on failure. The `factory` is called
    /// once per attempt to produce a fresh closure, so each retry can re-open
    /// files, re-connect sockets, etc.
    ///
    /// Between attempts the worker sleeps for `policy.backoff` and emits a
    /// [`TaskEvent::Retrying`] so the UI can show a "retrying" indicator.
    /// When all retries are exhausted the final failure is reported normally
    /// via [`TaskEvent::Failed`].
    ///
    /// The journal records each retry (incrementing `retry_count`) so a crash
    /// mid-retry surfaces the right attempt number on restart.
    pub fn start_with_retry<T, F>(
        &self,
        kind: TaskKind,
        label: impl Into<String>,
        policy: RetryPolicy,
        factory: F,
    ) -> Result<(TaskId, std::sync::mpsc::Receiver<T>), StartError>
    where
        T: Send + 'static,
        F: FnMut() -> Box<dyn FnOnce(&JobContext) -> crate::error::Result<T> + Send>
            + Send
            + 'static,
    {
        self.start_with_retry_and_priority(kind, label, policy, TaskPriority::Normal, factory)
    }

    /// Like [`TaskManager::start_with_retry`], but with an explicit priority.
    pub fn start_with_retry_and_priority<T, F>(
        &self,
        kind: TaskKind,
        label: impl Into<String>,
        policy: RetryPolicy,
        priority: TaskPriority,
        factory: F,
    ) -> Result<(TaskId, std::sync::mpsc::Receiver<T>), StartError>
    where
        T: Send + 'static,
        F: FnMut() -> Box<dyn FnOnce(&JobContext) -> crate::error::Result<T> + Send>
            + Send
            + 'static,
    {
        self.gate_kind(&kind)?;
        let mut jobs = crate::sync::lock(&self.jobs);
        if jobs
            .values()
            .any(|j| j.kind == kind && matches!(j.status, TaskStatus::Running | TaskStatus::Paused))
        {
            return Err(StartError::AlreadyRunning);
        }
        jobs.retain(|_, j| matches!(j.status, TaskStatus::Running | TaskStatus::Paused));
        let id: TaskId = new_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(PauseSignal::new());
        let label = label.into();
        jobs.insert(
            id,
            JobState {
                kind: kind.clone(),
                label: label.clone(),
                priority,
                status: TaskStatus::Running,
                done: 0,
                total: 0,
                summary: None,
                cancel: cancel.clone(),
                pause: pause.clone(),
                retry: Some(RetryState {
                    remaining: policy.max_retries,
                    max: policy.max_retries,
                    backoff: policy.backoff,
                }),
            },
        );
        self.events.push(TaskEvent::Started {
            id,
            kind: kind.clone(),
        });
        journal_write(
            &self.journal,
            &self.journal_degraded,
            id,
            "record_start",
            |conn| task_journal::record_start(conn, id, &kind, &label, policy.max_retries),
        );
        drop(jobs);

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = JobContext {
            id,
            kind: kind.clone(),
            cancel,
            pause,
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        };
        let journal = self.journal.clone();
        let degraded = self.journal_degraded.clone();
        self.pool.execute_with_priority(
            move || {
                // The factory is wrapped in a Mutex so the worker can call it
                // once per attempt (it is FnMut, not Fn).
                let factory = Mutex::new(factory);
                loop {
                    let run = crate::sync::lock(&factory)();
                    let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&ctx)));
                    let mut jobs = crate::sync::lock(&ctx.jobs);
                    let Some(state) = jobs.get_mut(&ctx.id) else {
                        return;
                    };
                    match outcome {
                        Ok(Ok(value)) if !ctx.cancelled() => {
                            state.status = TaskStatus::Completed;
                            let summary = state.summary.clone().unwrap_or_default();
                            ctx.events.push(TaskEvent::Completed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                summary: summary.clone(),
                            });
                            let done = state.done;
                            let total = state.total;
                            drop(jobs);
                            let _ = tx.send(value);
                            journal_write(&journal, &degraded, ctx.id, "record_status", |conn| {
                                task_journal::record_status(
                                    conn,
                                    ctx.id,
                                    TaskStatus::Completed,
                                    done,
                                    total,
                                    Some(&summary),
                                    None,
                                )
                            });
                            return;
                        }
                        Ok(Ok(_)) => {
                            // Cancelled mid-run.
                            state.status = TaskStatus::Cancelled;
                            ctx.events.push(TaskEvent::Cancelled {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                            });
                            let done = state.done;
                            let total = state.total;
                            drop(jobs);
                            journal_write(&journal, &degraded, ctx.id, "record_status", |conn| {
                                task_journal::record_status(
                                    conn,
                                    ctx.id,
                                    TaskStatus::Cancelled,
                                    done,
                                    total,
                                    None,
                                    None,
                                )
                            });
                            return;
                        }
                        Ok(Err(error)) => {
                            let error = error.to_string();
                            // Check whether we have retries left.
                            let can_retry = state.retry.as_mut().is_some_and(|r| r.remaining > 0);
                            if can_retry {
                                let retry = state.retry.as_mut().unwrap();
                                retry.remaining -= 1;
                                let attempt = retry.max - retry.remaining;
                                let backoff = retry.backoff;
                                ctx.events.push(TaskEvent::Retrying {
                                    id: ctx.id,
                                    kind: ctx.kind.clone(),
                                    attempt,
                                    max_retries: retry.max,
                                });
                                drop(jobs);
                                journal_write(
                                    &journal,
                                    &degraded,
                                    ctx.id,
                                    "record_retry",
                                    |conn| task_journal::record_retry(conn, ctx.id),
                                );
                                // Sleep for backoff, but wake early on cancel.
                                let deadline = Instant::now() + backoff;
                                while Instant::now() < deadline && !ctx.cancelled() {
                                    std::thread::sleep(Duration::from_millis(100));
                                }
                                if ctx.cancelled() {
                                    let mut jobs = crate::sync::lock(&ctx.jobs);
                                    if let Some(state) = jobs.get_mut(&ctx.id) {
                                        state.status = TaskStatus::Cancelled;
                                        ctx.events.push(TaskEvent::Cancelled {
                                            id: ctx.id,
                                            kind: ctx.kind.clone(),
                                        });
                                    }
                                    drop(jobs);
                                    journal_write(
                                        &journal,
                                        &degraded,
                                        ctx.id,
                                        "record_status",
                                        |conn| {
                                            task_journal::record_status(
                                                conn,
                                                ctx.id,
                                                TaskStatus::Cancelled,
                                                0,
                                                0,
                                                None,
                                                None,
                                            )
                                        },
                                    );
                                    return;
                                }
                                // Reset progress for the next attempt.
                                let mut jobs = crate::sync::lock(&ctx.jobs);
                                if let Some(state) = jobs.get_mut(&ctx.id) {
                                    state.done = 0;
                                    state.total = 0;
                                }
                                drop(jobs);
                                continue;
                            }
                            // No retries left: report final failure.
                            state.status = TaskStatus::Failed;
                            ctx.events.push(TaskEvent::Failed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                error: error.clone(),
                            });
                            let done = state.done;
                            let total = state.total;
                            drop(jobs);
                            journal_write(&journal, &degraded, ctx.id, "record_status", |conn| {
                                task_journal::record_status(
                                    conn,
                                    ctx.id,
                                    TaskStatus::Failed,
                                    done,
                                    total,
                                    None,
                                    Some(&error),
                                )
                            });
                            return;
                        }
                        Err(_) => {
                            // Panic: treat as a non-retryable failure.
                            state.status = TaskStatus::Failed;
                            ctx.events.push(TaskEvent::Failed {
                                id: ctx.id,
                                kind: ctx.kind.clone(),
                                error: "task panicked".into(),
                            });
                            let done = state.done;
                            let total = state.total;
                            drop(jobs);
                            journal_write(&journal, &degraded, ctx.id, "record_status", |conn| {
                                task_journal::record_status(
                                    conn,
                                    ctx.id,
                                    TaskStatus::Failed,
                                    done,
                                    total,
                                    None,
                                    Some("task panicked"),
                                )
                            });
                            return;
                        }
                    }
                }
            },
            priority,
        );
        Ok((id, rx))
    }

    /// Ask the job to stop at its next cancellation checkpoint. A job parked
    /// at a pause checkpoint is woken too, so it can observe the request and
    /// unwind instead of waiting forever for a resume that will not come.
    pub fn cancel(&self, id: TaskId) {
        let jobs = crate::sync::lock(&self.jobs);
        if let Some(state) = jobs.get(&id) {
            state.cancel.store(true, Ordering::Relaxed);
            state.pause.wake();
        }
    }

    /// Hold a running job at its next checkpoint. It keeps its slot and can be
    /// resumed; nothing happens if the job is not running.
    pub fn pause(&self, id: TaskId) {
        let mut jobs = crate::sync::lock(&self.jobs);
        if let Some(state) = jobs.get_mut(&id)
            && state.status == TaskStatus::Running
        {
            state.status = TaskStatus::Paused;
            state.pause.request_pause();
            self.events.push(TaskEvent::Paused {
                id,
                kind: state.kind.clone(),
            });
        }
    }

    /// Release a parked job to continue from its next checkpoint.
    pub fn resume(&self, id: TaskId) {
        let mut jobs = crate::sync::lock(&self.jobs);
        if let Some(state) = jobs.get_mut(&id)
            && state.status == TaskStatus::Paused
        {
            state.status = TaskStatus::Running;
            state.pause.request_resume();
            self.events.push(TaskEvent::Resumed {
                id,
                kind: state.kind.clone(),
            });
        }
    }

    /// Whether a job of `kind` is currently running.
    pub fn is_running(&self, kind: &TaskKind) -> bool {
        let jobs = crate::sync::lock(&self.jobs);
        jobs.values()
            .any(|j| &j.kind == kind && j.status == TaskStatus::Running)
    }

    /// Whether a job of `kind` holds its slot: running *or* paused. Used to
    /// refuse a second job of the same kind while one is paused.
    pub fn is_active(&self, kind: &TaskKind) -> bool {
        let jobs = crate::sync::lock(&self.jobs);
        jobs.values().any(|j| {
            &j.kind == kind && matches!(j.status, TaskStatus::Running | TaskStatus::Paused)
        })
    }

    /// Whether this exact job is still running.
    pub fn is_task_running(&self, id: TaskId) -> bool {
        let jobs = crate::sync::lock(&self.jobs);
        jobs.get(&id)
            .is_some_and(|j| j.status == TaskStatus::Running)
    }

    /// Whether this exact job still holds its slot: running *or* paused. A
    /// library swap waits on this so a paused-but-not-yet-wound-down job gets
    /// its chance to observe cancellation and stop before the store is swapped.
    pub fn is_task_active(&self, id: TaskId) -> bool {
        let jobs = crate::sync::lock(&self.jobs);
        jobs.get(&id)
            .is_some_and(|j| matches!(j.status, TaskStatus::Running | TaskStatus::Paused))
    }

    /// Drain every accumulated event (all jobs). Takes only the event lock, so
    /// it never queues behind a job's registry writes.
    ///
    /// A watcher tracking one specific job should prefer
    /// [`TaskManager::poll_events_for`]: this drains *every* job's events, and
    /// the result carries no order between jobs.
    pub fn poll_events(&self) -> Vec<TaskEvent> {
        self.events.take_all()
    }

    /// Drain only `id`'s events, oldest first, leaving every other job's
    /// events in place for their own watchers. This is what lets several jobs
    /// run and report progress at the same time without their pollers
    /// consuming one another's events — and, since each job has its own bucket,
    /// it costs only that bucket's length.
    pub fn poll_events_for(&self, id: TaskId) -> Vec<TaskEvent> {
        self.events.take(id)
    }

    /// Like [`TaskManager::poll_events_for`], but parks the calling thread on
    /// the queue's condvar until `id` has an event or `timeout` runs out, so an
    /// embedder that owns its thread reacts to events as they land instead of
    /// on a fixed poll cadence. Returns the empty vec on timeout.
    ///
    /// Do not call this from a shared thread pool: a parked watcher is a pool
    /// worker nobody else can run on. gpui's watchers keep polling instead.
    pub fn wait_events_for(&self, id: TaskId, timeout: Duration) -> Vec<TaskEvent> {
        self.events.wait(id, timeout)
    }

    /// Snapshot of every known job. Finished jobs leave the registry when the
    /// next one starts (see [`TaskManager::start`]), so the map only ever
    /// holds running jobs plus the last finished ones. Results are sorted by
    /// priority (high first), then by insertion order within the same level.
    pub fn snapshot(&self) -> Vec<TaskInfo> {
        let jobs = crate::sync::lock(&self.jobs);
        let mut infos: Vec<TaskInfo> = jobs
            .iter()
            .map(|(id, j)| TaskInfo {
                id: *id,
                kind: j.kind.clone(),
                label: j.label.clone(),
                priority: j.priority,
                status: j.status,
                done: j.done,
                total: j.total,
                summary: j.summary.clone(),
            })
            .collect();
        // Stable sort: high priority first, same-priority jobs keep their
        // map-iteration order (which is arbitrary but deterministic per
        // snapshot).
        infos.sort_by_key(|task| std::cmp::Reverse(task.priority));
        infos
    }
}

/// Handle a running job uses to report progress, check cancellation and park
/// for a pause.
pub struct JobContext {
    id: TaskId,
    kind: TaskKind,
    cancel: Arc<AtomicBool>,
    pause: Arc<PauseSignal>,
    jobs: Arc<Mutex<HashMap<TaskId, JobState>>>,
    events: Arc<EventQueue>,
    last_progress: Mutex<Instant>,
}

impl JobContext {
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn kind(&self) -> TaskKind {
        self.kind.clone()
    }

    /// A standalone context for tests that call job functions directly.
    #[cfg(test)]
    pub(crate) fn for_tests(cancelled: bool) -> Self {
        Self {
            id: new_id(),
            kind: TaskKind::Import,
            cancel: Arc::new(AtomicBool::new(cancelled)),
            pause: Arc::new(PauseSignal::new()),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(EventQueue::default()),
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        }
    }

    /// Whether cancellation was requested. Jobs check this between units of
    /// work and unwind their loop early.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Block here while the job is paused, then return so the caller's own
    /// cancellation check runs. Jobs call this at the same checkpoints where
    /// they call [`JobContext::cancelled`] — never inside a database
    /// transaction — so a pause costs at most one committed batch and a
    /// cancellation that arrives during a pause takes effect immediately.
    pub fn park_if_paused(&self) {
        self.pause.park(&self.cancel);
    }

    /// The raw cancellation flag, for workers that check it deep inside a
    /// parallel loop (the staging pool) rather than at job-level checkpoints.
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Update the total unit count once it is known (e.g. after expanding
    /// directories). Pushes an immediate progress event.
    pub fn set_total(&self, total: u64) {
        let done = {
            let mut jobs = crate::sync::lock(&self.jobs);
            match jobs.get_mut(&self.id) {
                Some(state) => {
                    state.total = total;
                    state.done
                }
                None => 0,
            }
        };
        self.events.push(TaskEvent::Progress {
            id: self.id,
            done,
            total,
        });
        *crate::sync::lock(&self.last_progress) = Instant::now();
    }

    /// Report progress. State updates always land; the event is throttled to
    /// [`PROGRESS_EVENT_INTERVAL`] except for the final unit.
    pub fn progress(&self, done: u64, total: u64) {
        {
            let mut jobs = crate::sync::lock(&self.jobs);
            if let Some(state) = jobs.get_mut(&self.id) {
                state.done = done;
                state.total = total;
            }
        }
        let due = done == total || {
            let mut last = crate::sync::lock(&self.last_progress);
            if *last < Instant::now() - PROGRESS_EVENT_INTERVAL {
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if due {
            self.events.push(TaskEvent::Progress {
                id: self.id,
                done,
                total,
            });
        }
    }

    /// Set the human-readable line carried by the completion event.
    pub fn set_summary(&self, summary: String) {
        let mut jobs = crate::sync::lock(&self.jobs);
        if let Some(state) = jobs.get_mut(&self.id) {
            state.summary = Some(summary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn status_of(mgr: &TaskManager, id: TaskId) -> TaskStatus {
        mgr.snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .map(|t| t.status)
            .expect("job present in registry")
    }

    /// The pool's two lanes: while a worker is busy and more work queues up,
    /// `High` closures are picked up before every queued `Normal` one. This
    /// is what keeps a model preview's parse from lining up behind a
    /// low-priority backfill. One worker makes the order deterministic.
    #[test]
    fn the_pool_runs_high_priority_work_ahead_of_queued_normal_work() {
        let pool = TaskPool::new(1);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<&'static str>();
        // Occupy the only worker until the gate opens. It is the first
        // closure submitted, so whichever way the worker's wake-up races
        // with the submits below, it is the one that takes the worker.
        pool.execute(move || {
            gate_rx.recv().expect("the gate opens");
        });
        pool.execute({
            let done_tx = done_tx.clone();
            move || {
                let _ = done_tx.send("normal");
            }
        });
        pool.execute_with_priority(
            move || {
                let _ = done_tx.send("high");
            },
            TaskPriority::High,
        );
        let _ = gate_tx.send(());
        drop(gate_tx);
        let order: Vec<&'static str> = (0..2)
            .filter_map(|_| done_rx.recv_timeout(std::time::Duration::from_secs(5)).ok())
            .collect();
        assert_eq!(order, ["high", "normal"], "the priority lane runs first");
    }

    /// The journal is the only record a *later* process has of a job, so a write
    /// that fails in silence is a hole nobody can see from the other side: an
    /// absent row reads exactly like a job that never ran.
    ///
    /// An in-memory database with no `task_journal` table stands in for the real
    /// causes — a full disk, a locked file, a library moved mid-run. Every
    /// lifecycle write against it returns `Err`.
    #[test]
    fn a_journal_write_that_fails_is_reported_not_swallowed() {
        let mut mgr = TaskManager::new();
        mgr.set_journal(rusqlite::Connection::open_in_memory().unwrap());
        assert!(
            !mgr.journal_degraded(),
            "a manager that has written nothing is not yet broken"
        );
        let (_id, _rx) = mgr
            .start(TaskKind::Import, "unrecorded", |_ctx| Ok(()))
            .unwrap();
        assert!(
            mgr.journal_degraded(),
            "record_start failed and the manager stayed silent"
        );
    }

    /// The two halves the test above cannot fail by accident: a journal that
    /// writes must not raise the flag, and having no journal at all is the
    /// documented in-memory mode rather than a failure. Either one violated would
    /// make the warning fire on every healthy library open.
    #[test]
    fn a_working_journal_and_a_missing_one_both_stay_clean() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE task_journal (
                 task_id TEXT PRIMARY KEY, kind TEXT NOT NULL, label TEXT NOT NULL,
                 status TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
                 total INTEGER NOT NULL DEFAULT 0, summary TEXT, error TEXT,
                 retry_count INTEGER NOT NULL DEFAULT 0,
                 max_retries INTEGER NOT NULL DEFAULT 0,
                 started_at TEXT NOT NULL, finished_at TEXT);",
        )
        .unwrap();
        let mut mgr = TaskManager::new();
        mgr.set_journal(conn);
        let (_id, _rx) = mgr
            .start(TaskKind::Import, "recorded", |_ctx| Ok(()))
            .unwrap();
        assert!(
            !mgr.journal_degraded(),
            "a write that landed was reported as a failure"
        );

        let plain = TaskManager::new();
        let (_id, _rx) = plain
            .start(TaskKind::Import, "no journal", |_ctx| Ok(()))
            .unwrap();
        assert!(
            !plain.journal_degraded(),
            "running without a journal is a mode, not an error"
        );
    }

    /// A watcher polling one job's id must not consume another job's events —
    /// the exact cross-talk that made concurrent toast numbers freeze.
    #[test]
    fn poll_events_for_only_takes_its_own_job() {
        let mgr = TaskManager::new();
        let (a_id, _) = mgr
            .start(TaskKind::Import, "a", |ctx| {
                ctx.set_total(1);
                Ok(())
            })
            .unwrap();
        let (b_id, _) = mgr
            .start(TaskKind::EmbeddingBackfill, "b", |_ctx| Ok(()))
            .unwrap();

        let a_events = mgr.poll_events_for(a_id);
        assert!(!a_events.is_empty());
        assert!(
            a_events.iter().all(|e| e.task_id() == a_id),
            "poll for A returned foreign events: {a_events:?}"
        );
        // B's Started must still be waiting for B's own poller.
        let b_events = mgr.poll_events_for(b_id);
        assert!(
            b_events
                .iter()
                .any(|e| matches!(e, TaskEvent::Started { .. })),
            "A's poll ate B's events: {b_events:?}"
        );
    }

    /// The global drain still reaches every job. What it no longer promises is
    /// an order *between* jobs — the buckets are separate — while each job's
    /// own sequence is intact.
    #[test]
    fn poll_events_drains_every_bucket() {
        let mgr = TaskManager::new();
        let (a_id, a_rx) = mgr.start(TaskKind::Import, "a", |_ctx| Ok(())).unwrap();
        let (b_id, b_rx) = mgr
            .start(TaskKind::VisualBackfill, "b", |_ctx| Ok(()))
            .unwrap();
        // A job publishes its terminal event before its value, so waiting for
        // the value is what makes both buckets settled here.
        a_rx.recv().expect("a finished");
        b_rx.recv().expect("b finished");

        let all = mgr.poll_events();
        for id in [a_id, b_id] {
            let own: Vec<&TaskEvent> = all.iter().filter(|e| e.task_id() == id).collect();
            assert_eq!(own.len(), 2, "job {id} missing from the global drain");
            assert!(
                matches!(own[0], TaskEvent::Started { .. }),
                "job {id} came out of its bucket out of order: {own:?}"
            );
        }
        assert!(
            mgr.poll_events().is_empty(),
            "the drain left a bucket behind"
        );
        // And it left no *empty* bucket either — a waiter sleeps on the key's
        // absence, so a stale key would make it spin instead of sleep.
        let idle = Instant::now();
        assert!(
            mgr.wait_events_for(a_id, Duration::from_millis(40))
                .is_empty()
        );
        assert!(
            idle.elapsed() >= Duration::from_millis(20),
            "a drained job still woke its waiters"
        );
    }

    /// Progress events are lossy: only the latest one per job is kept, so a
    /// flood of progress updates cannot crowd out terminal events — not even
    /// the same job's own.
    #[test]
    fn progress_events_are_lossy() {
        let queue = EventQueue::default();
        let id = new_id();
        for done in 0..100 {
            queue.push(TaskEvent::Progress {
                id,
                done,
                total: 1_000,
            });
        }
        let events = queue.take(id);
        assert_eq!(events.len(), 1, "progress events should coalesce to one");
        let TaskEvent::Progress { done, .. } = &events[0] else {
            panic!("expected a progress event");
        };
        assert_eq!(*done, 99, "kept the wrong progress event");
    }

    /// A foreign backlog of progress events cannot displace another job's
    /// terminal event — the one event a watcher cannot afford to miss.
    #[test]
    fn an_unread_backlog_evicts_only_its_own_events() {
        let queue = EventQueue::default();
        let noisy = new_id();
        let settled = new_id();
        queue.push(TaskEvent::Completed {
            id: settled,
            kind: TaskKind::Import,
            summary: String::new(),
        });
        // Flood progress for the noisy job — all but the latest are discarded.
        for done in 0..(MAX_EVENTS_PER_JOB as u64 + 10) {
            queue.push(TaskEvent::Progress {
                id: noisy,
                done,
                total: 1_000,
            });
        }

        assert_eq!(
            queue.take(settled).len(),
            1,
            "a foreign backlog dropped a terminal event"
        );
        // The noisy job's progress coalesced to a single event.
        let backlog = queue.take(noisy);
        assert_eq!(backlog.len(), 1, "progress should be lossy (one kept)");
        let TaskEvent::Progress { done, .. } = backlog.first().expect("backlog kept") else {
            panic!("expected a progress event");
        };
        assert_eq!(
            *done,
            MAX_EVENTS_PER_JOB as u64 + 9,
            "kept the wrong progress event"
        );
    }

    /// Buckets are created per job id, so the jobs nobody ever polls (a watch
    /// scan, the run before last) have to be bounded too — by dropping whole
    /// buckets, oldest first, which by construction leaves every job that is
    /// still being watched alone.
    #[test]
    fn unread_buckets_are_bounded_oldest_first() {
        let queue = EventQueue::default();
        let ids: Vec<TaskId> = (0..MAX_TRACKED_JOBS + 8).map(|_| new_id()).collect();
        for id in &ids {
            queue.push(TaskEvent::Started {
                id: *id,
                kind: TaskKind::Maintenance,
            });
        }

        assert!(
            queue.take(ids[0]).is_empty(),
            "the oldest unread bucket survived the bound"
        );
        assert!(
            !queue.take(*ids.last().expect("last id")).is_empty(),
            "the newest bucket was evicted"
        );
    }

    /// What makes that bound safe to apply: a job that is being read is never
    /// the oldest bucket, because taking its events takes the entry with them
    /// and its next event re-stamps it. So a session that runs job after job
    /// nobody watches still cannot starve the one watcher that is reading.
    #[test]
    fn a_watched_job_outlives_a_stream_of_unwatched_ones() {
        let queue = EventQueue::default();
        let watched = new_id();
        for _ in 0..MAX_TRACKED_JOBS * 3 {
            queue.push(TaskEvent::Progress {
                id: watched,
                done: 1,
                total: 2,
            });
            assert!(
                !queue.take(watched).is_empty(),
                "a polled job lost its bucket to eviction"
            );
            // A job that starts, reports, and is never read.
            queue.push(TaskEvent::Started {
                id: new_id(),
                kind: TaskKind::Maintenance,
            });
        }

        queue.push(TaskEvent::Progress {
            id: watched,
            done: 2,
            total: 2,
        });
        assert_eq!(
            queue.take(watched).len(),
            1,
            "the watched job was evicted by unwatched backlog"
        );
    }

    /// The blocking read: it sleeps until its own job publishes something, and
    /// gives up with nothing when the timeout runs out — which is what lets an
    /// embedder that owns its thread drop the poll cadence without risking a
    /// lost wake-up.
    #[test]
    fn wait_events_for_sleeps_until_its_job_reports() {
        let mgr = TaskManager::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (id, _rx) = mgr
            .start(TaskKind::Import, "parked", move |_ctx| {
                let _ = release_rx.recv(); // hold the job open for the test
                Ok(())
            })
            .unwrap();
        // Take the `Started` away, so the wait below has nothing to find.
        assert!(
            mgr.poll_events_for(id)
                .iter()
                .any(|e| matches!(e, TaskEvent::Started { .. }))
        );

        let idle = Instant::now();
        assert!(
            mgr.wait_events_for(id, Duration::from_millis(40))
                .is_empty(),
            "an idle wait returned events that were never published"
        );
        assert!(
            idle.elapsed() >= Duration::from_millis(20),
            "the idle wait spun instead of sleeping"
        );

        // A pause publishes for this job; the waiter must wake on it, long
        // before its (generous) timeout.
        let waiter = {
            let mgr = mgr.clone();
            std::thread::spawn(move || mgr.wait_events_for(id, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        mgr.pause(id);
        let events = waiter.join().expect("waiter");
        assert!(
            events.iter().any(|e| matches!(e, TaskEvent::Paused { .. })),
            "the waiter missed its own job's event: {events:?}"
        );

        release_tx.send(()).expect("release the job");
    }

    /// A paused job holds its kind's slot and is reported `Paused`, then flips
    /// back to `Running` on resume.
    #[test]
    fn pause_holds_slot_and_resume_releases_it() {
        let mgr = TaskManager::new();
        let (_release_tx, release_rx) = mpsc::channel::<()>();
        let (id, _) = mgr
            .start(TaskKind::Import, "block", move |_ctx| {
                let _ = release_rx.recv(); // stay Running for the whole test
                Ok(())
            })
            .unwrap();

        mgr.pause(id);
        assert_eq!(status_of(&mgr, id), TaskStatus::Paused);
        assert!(!mgr.is_running(&TaskKind::Import));
        assert!(mgr.is_active(&TaskKind::Import));
        // The kind stays occupied while paused.
        assert!(matches!(
            mgr.start(TaskKind::Import, "second", |_| Ok(())),
            Err(StartError::AlreadyRunning)
        ));

        mgr.resume(id);
        assert_eq!(status_of(&mgr, id), TaskStatus::Running);
        // Leaving the release sender unused drops it at end of test, so the
        // blocked thread unwinds on disconnect.
    }

    /// A job parked at a checkpoint does not advance until it is resumed.
    #[test]
    fn paused_job_parks_until_resumed() {
        let mgr = TaskManager::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (past_tx, past_rx) = mpsc::channel::<()>();
        let (id, _) = mgr
            .start(TaskKind::Import, "park", move |ctx| {
                let _ = release_rx.recv(); // stay Running until the pause is set
                ctx.park_if_paused(); // blocks here while paused
                let _ = past_tx.send(());
                Ok(())
            })
            .unwrap();

        mgr.pause(id);
        release_tx.send(()).unwrap(); // let the job reach the park point
        assert!(
            past_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "job ran past a pause checkpoint"
        );

        mgr.resume(id);
        assert!(
            past_rx.recv_timeout(Duration::from_millis(1_000)).is_ok(),
            "job did not resume after being released"
        );
    }

    /// Cancelling a parked job wakes it so it unwinds instead of waiting for a
    /// resume that never comes.
    #[test]
    fn cancel_wakes_a_parked_job() {
        let mgr = TaskManager::new();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (past_tx, past_rx) = mpsc::channel::<&'static str>();
        let (id, _) = mgr
            .start(TaskKind::Import, "cancel-while-parked", move |ctx| {
                let _ = release_rx.recv();
                ctx.park_if_paused();
                if ctx.cancelled() {
                    let _ = past_tx.send("cancelled");
                } else {
                    let _ = past_tx.send("resumed");
                }
                Ok(())
            })
            .unwrap();

        mgr.pause(id);
        release_tx.send(()).unwrap(); // reach the park point
        mgr.cancel(id); // must wake the parked thread
        let woke = past_rx
            .recv_timeout(Duration::from_millis(1_000))
            .expect("cancel did not wake the parked job");
        assert_eq!(woke, "cancelled");
    }

    /// A custom task kind must be one an enabled plugin declared. Without this
    /// gate any call site could mint a name, occupy its slot, and write a
    /// journal row no plugin will ever read back.
    #[test]
    fn an_undeclared_custom_kind_is_refused_and_a_declared_one_runs() {
        let mut mgr = TaskManager::new();
        let notes = TaskKind::Custom(Cow::Borrowed("notes-index"));
        let err = mgr.start(notes.clone(), "x", |_| Ok(())).unwrap_err();
        assert_eq!(err, StartError::UndeclaredKind);
        // Built-in kinds are never gated, and an empty declaration set is what
        // a process with no plugins (the CLI) looks like.
        assert!(mgr.start(TaskKind::Import, "x", |_| Ok(())).is_ok());

        mgr.declare_task_kinds(&["notes-index"]);
        assert!(
            mgr.start(notes.clone(), "x", |_| Ok(())).is_ok(),
            "a declared kind should hold its slot"
        );
        // The same gate covers the retrying entry point.
        assert!(
            mgr.start_with_retry(
                TaskKind::Custom(Cow::Borrowed("never-declared")),
                "x",
                RetryPolicy::times(1),
                || Box::new(|_| Ok(())),
            )
            .is_err()
        );
    }

    /// A retried job calls the factory again rather than reusing a closure it
    /// already consumed, reports the intermediate attempt instead of swallowing
    /// it, and still delivers the value.
    #[test]
    fn a_failed_job_retries_and_reports_each_attempt() {
        let mgr = TaskManager::new();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let (id, rx) = mgr
            .start_with_retry(
                TaskKind::Import,
                "flaky",
                RetryPolicy {
                    max_retries: 2,
                    backoff: Duration::from_millis(10),
                },
                move || {
                    let seen = seen.clone();
                    Box::new(move |_| {
                        let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if n < 2 {
                            Err(crate::error::Error::Message("first attempt fails".into()))
                        } else {
                            Ok(n)
                        }
                    })
                },
            )
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            2,
            "the second attempt is the one that produced a value"
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

        let events = mgr.poll_events_for(id);
        assert!(
            events.iter().any(|e| matches!(
                e,
                TaskEvent::Retrying {
                    attempt: 1,
                    max_retries: 2,
                    ..
                }
            )),
            "a retried attempt was never reported: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(TaskEvent::Completed { .. })),
            "the terminal event should be last: {events:?}"
        );
    }

    /// Exhausting the budget ends the job as failed, on the *last* attempt's
    /// error — the retry loop must not turn a real failure into a silent one.
    #[test]
    fn a_job_that_uses_every_retry_fails_for_good() {
        let mgr = TaskManager::new();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let (id, rx): (TaskId, std::sync::mpsc::Receiver<()>) = mgr
            .start_with_retry(
                TaskKind::Import,
                "doomed",
                RetryPolicy::times(1),
                move || {
                    let seen = seen.clone();
                    Box::new(move |_| {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Err(crate::error::Error::Message("always fails".into()))
                    })
                },
            )
            .unwrap();
        assert!(
            rx.recv().is_err(),
            "a job that never succeeded must close the channel"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one run plus one retry"
        );
        let events = mgr.poll_events_for(id);
        assert!(
            matches!(events.last(), Some(TaskEvent::Failed { error, .. }) if error == "always fails"),
            "the final failure was not reported: {events:?}"
        );
    }

    /// The panel reads [`TaskManager::snapshot`], so priority is only worth
    /// having if a user-requested job comes back ahead of a backfill.
    #[test]
    fn a_snapshot_lists_higher_priority_jobs_first() {
        let mgr = TaskManager::new();
        let park = || -> crate::error::Result<()> {
            std::thread::sleep(Duration::from_millis(60));
            Ok(())
        };
        // A background backfill first, then the job the user is waiting on.
        let (low, _) = mgr
            .start_with_priority(
                TaskKind::EmbeddingBackfill,
                "backfill",
                TaskPriority::Low,
                move |_| park(),
            )
            .unwrap();
        let (high, _) = mgr
            .start_with_priority(
                TaskKind::ModelPreview,
                "parse",
                TaskPriority::High,
                move |_| park(),
            )
            .unwrap();
        let order: Vec<TaskId> = mgr.snapshot().into_iter().map(|t| t.id).collect();
        let positions = |id: TaskId| order.iter().position(|x| *x == id).unwrap();
        assert!(
            positions(high) < positions(low),
            "a High job should sort before a Low one: {order:?}"
        );
        mgr.cancel(high);
        mgr.cancel(low);
    }
}
