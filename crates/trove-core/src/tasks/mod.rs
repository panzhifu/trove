//! Backend task management: one registry for every long-running job the
//! library drives — imports, inbox collection, model/preview work,
//! maintenance — with progress, cancellation and lifecycle events in one
//! place.
//!
//! The manager is deliberately UI-free: jobs run on plain `std::thread`s and
//! report through an event queue the embedder polls
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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::model::new_id;
use uuid::Uuid;

pub mod ai_analysis;
pub mod embed;
pub mod ignore;
pub mod import;
pub mod watch;

/// Handle identifying one job inside the manager.
pub type TaskId = Uuid;

/// How long a job waits between two throttled progress events.
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on one job's unread events. Each job owns a bucket (see
/// [`EventQueue`]) and only that bucket is bounded, so a resident job nobody
/// polls (the folder watcher, one-shot maintenance) fills its own bucket and
/// evicts its own stale progress — it can no longer push another job's
/// terminal event out of a shared queue the way the old global bound did. The
/// live count a job reports always reflects the freshest state, so losing a
/// stale event is invisible, whereas losing memory is not. At the backend's
/// ~10 events/s this holds 12 seconds of backlog: a watcher that stalls longer
/// than that still gets the job's latest numbers.
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
    EmbeddingBackfill,
    /// Asking a chat model to tag assets. Serial by nature — one request per
    /// asset — so it is the longest-running job the library has.
    AutoTag,
    /// Multimodal AI analysis: description, tags, and rating from a vision
    /// model. One API call per asset.
    AiAnalysis,
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
            TaskKind::EmbeddingBackfill => "embedding-backfill",
            TaskKind::AutoTag => "auto-tag",
            TaskKind::AiAnalysis => "ai-analysis",
        }
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
            | TaskEvent::Resumed { id, .. } => *id,
        }
    }
}

struct JobState {
    kind: TaskKind,
    label: String,
    status: TaskStatus,
    done: u64,
    total: u64,
    summary: Option<String>,
    cancel: Arc<AtomicBool>,
    pause: Arc<PauseSignal>,
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
        let _guard = self.lock.lock().unwrap();
        self.paused.store(true, Ordering::SeqCst);
    }

    fn request_resume(&self) {
        {
            let _guard = self.lock.lock().unwrap();
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
        drop(self.lock.lock().unwrap());
        self.cond.notify_all();
    }

    /// Block while paused. Returns once the job is resumed or cancellation is
    /// requested; the caller then runs its own cancellation check.
    fn park(&self, cancel: &AtomicBool) {
        if !self.paused.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.lock.lock().unwrap();
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
/// own stale progress.
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
    /// `id` → (sequence in which the bucket was created, its unread events).
    /// The sequence makes "drop the job that has gone unread the longest" a
    /// scan of a handful of entries instead of a second collection to keep in
    /// sync with the map.
    ///
    /// An entry is never present-and-empty: every take removes the entry with
    /// its events, which is what lets [`EventQueue::wait`] sleep on the key's
    /// absence rather than on a length.
    by_job: HashMap<TaskId, (u64, VecDeque<TaskEvent>)>,
    next_seq: u64,
}

impl EventQueue {
    /// Append one event to its job's bucket and wake the waiters.
    ///
    /// Callers hold no lock this needs; when invoked from [`TaskManager::start`]
    /// the registry lock is already held, preserving the `jobs → events` order.
    fn push(&self, event: TaskEvent) {
        {
            let mut state = self.state.lock().unwrap();
            let bucket = state.entry(event.task_id());
            if bucket.len() >= MAX_EVENTS_PER_JOB {
                bucket.pop_front();
            }
            bucket.push_back(event);
            state.evict_stale();
        }
        // Notified with the lock released: a waiter either sees the event
        // before it parks (its predicate is checked under this same mutex) or
        // is parked and takes this wake-up, so neither order can lose it.
        self.ready.notify_all();
    }

    /// Take `id`'s events, oldest first, leaving every other job's alone.
    fn take(&self, id: TaskId) -> Vec<TaskEvent> {
        let mut state = self.state.lock().unwrap();
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
        let state = self.state.lock().unwrap();
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
        let mut state = self.state.lock().unwrap();
        std::mem::take(&mut state.by_job)
            .into_values()
            .flat_map(|(_, bucket)| bucket)
            .collect()
    }
}

impl QueueState {
    /// The job's bucket, created — and stamped with the next sequence number —
    /// if it does not exist yet.
    fn entry(&mut self, id: TaskId) -> &mut VecDeque<TaskEvent> {
        match self.by_job.entry(id) {
            Entry::Occupied(entry) => &mut entry.into_mut().1,
            Entry::Vacant(entry) => {
                let seq = self.next_seq;
                self.next_seq += 1;
                &mut entry.insert((seq, VecDeque::new())).1
            }
        }
    }

    /// Take one job's bucket out of the map.
    fn take_for(&mut self, id: TaskId) -> Vec<TaskEvent> {
        self.by_job
            .remove(&id)
            .map_or_else(Vec::new, |(_, bucket)| bucket.into_iter().collect())
    }

    /// Drop whole buckets, oldest-created first, until the set fits again.
    fn evict_stale(&mut self) {
        while self.by_job.len() > MAX_TRACKED_JOBS {
            match self
                .by_job
                .iter()
                .min_by_key(|(_, (seq, _))| *seq)
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

/// Shared registry of background jobs. Cheap to clone.
///
/// Two independent locks: `jobs` guards the registry, `events` guards the
/// per-job buckets. Splitting them keeps a job's per-file `progress` call from
/// contending with the embedder's drain. Lock order, when both are held, is
/// always `jobs` → `events`.
#[derive(Clone, Default)]
pub struct TaskManager {
    jobs: Arc<Mutex<HashMap<TaskId, JobState>>>,
    events: Arc<EventQueue>,
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
        let mut jobs = self.jobs.lock().unwrap();
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
        jobs.insert(
            id,
            JobState {
                kind,
                label: label.into(),
                status: TaskStatus::Running,
                done: 0,
                total: 0,
                summary: None,
                cancel: cancel.clone(),
                pause: pause.clone(),
            },
        );
        // Queued while the registry lock is held so no other job can slip an
        // event in ahead of this job's `Started`.
        self.events.push(TaskEvent::Started { id, kind });
        drop(jobs);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = JobContext {
            id,
            kind,
            cancel,
            pause,
            jobs: self.jobs.clone(),
            events: self.events.clone(),
            last_progress: Mutex::new(Instant::now() - PROGRESS_EVENT_INTERVAL),
        };
        std::thread::Builder::new()
            .name(format!("trove-task-{}", kind.name()))
            .spawn(move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&ctx)));
                let mut jobs = ctx.jobs.lock().unwrap();
                let Some(state) = jobs.get_mut(&ctx.id) else {
                    return;
                };
                match outcome {
                    Ok(Ok(value)) if !ctx.cancelled() => {
                        state.status = TaskStatus::Completed;
                        let summary = state.summary.clone().unwrap_or_default();
                        ctx.events.push(TaskEvent::Completed {
                            id: ctx.id,
                            kind: ctx.kind,
                            summary,
                        });
                        drop(jobs);
                        let _ = tx.send(value);
                    }
                    Ok(Ok(_)) => {
                        state.status = TaskStatus::Cancelled;
                        ctx.events.push(TaskEvent::Cancelled {
                            id: ctx.id,
                            kind: ctx.kind,
                        });
                    }
                    Ok(Err(error)) => {
                        state.status = TaskStatus::Failed;
                        ctx.events.push(TaskEvent::Failed {
                            id: ctx.id,
                            kind: ctx.kind,
                            error,
                        });
                    }
                    Err(_) => {
                        state.status = TaskStatus::Failed;
                        ctx.events.push(TaskEvent::Failed {
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

    /// Ask the job to stop at its next cancellation checkpoint. A job parked
    /// at a pause checkpoint is woken too, so it can observe the request and
    /// unwind instead of waiting forever for a resume that will not come.
    pub fn cancel(&self, id: TaskId) {
        let jobs = self.jobs.lock().unwrap();
        if let Some(state) = jobs.get(&id) {
            state.cancel.store(true, Ordering::Relaxed);
            state.pause.wake();
        }
    }

    /// Hold a running job at its next checkpoint. It keeps its slot and can be
    /// resumed; nothing happens if the job is not running.
    pub fn pause(&self, id: TaskId) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(state) = jobs.get_mut(&id)
            && state.status == TaskStatus::Running
        {
            state.status = TaskStatus::Paused;
            state.pause.request_pause();
            self.events.push(TaskEvent::Paused {
                id,
                kind: state.kind,
            });
        }
    }

    /// Release a parked job to continue from its next checkpoint.
    pub fn resume(&self, id: TaskId) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(state) = jobs.get_mut(&id)
            && state.status == TaskStatus::Paused
        {
            state.status = TaskStatus::Running;
            state.pause.request_resume();
            self.events.push(TaskEvent::Resumed {
                id,
                kind: state.kind,
            });
        }
    }

    /// Whether a job of `kind` is currently running.
    pub fn is_running(&self, kind: TaskKind) -> bool {
        let jobs = self.jobs.lock().unwrap();
        jobs.values()
            .any(|j| j.kind == kind && j.status == TaskStatus::Running)
    }

    /// Whether a job of `kind` holds its slot: running *or* paused. Used to
    /// refuse a second job of the same kind while one is paused.
    pub fn is_active(&self, kind: TaskKind) -> bool {
        let jobs = self.jobs.lock().unwrap();
        jobs.values()
            .any(|j| j.kind == kind && matches!(j.status, TaskStatus::Running | TaskStatus::Paused))
    }

    /// Whether this exact job is still running.
    pub fn is_task_running(&self, id: TaskId) -> bool {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(&id)
            .is_some_and(|j| j.status == TaskStatus::Running)
    }

    /// Whether this exact job still holds its slot: running *or* paused. A
    /// library swap waits on this so a paused-but-not-yet-wound-down job gets
    /// its chance to observe cancellation and stop before the store is swapped.
    pub fn is_task_active(&self, id: TaskId) -> bool {
        let jobs = self.jobs.lock().unwrap();
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
    /// holds running jobs plus the last finished ones.
    pub fn snapshot(&self) -> Vec<TaskInfo> {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter()
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
        self.kind
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
            let mut jobs = self.jobs.lock().unwrap();
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
        *self.last_progress.lock().unwrap() = Instant::now();
    }

    /// Report progress. State updates always land; the event is throttled to
    /// [`PROGRESS_EVENT_INTERVAL`] except for the final unit.
    pub fn progress(&self, done: u64, total: u64) {
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(state) = jobs.get_mut(&self.id) {
                state.done = done;
                state.total = total;
            }
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
            self.events.push(TaskEvent::Progress {
                id: self.id,
                done,
                total,
            });
        }
    }

    /// Set the human-readable line carried by the completion event.
    pub fn set_summary(&self, summary: String) {
        let mut jobs = self.jobs.lock().unwrap();
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

    /// What the per-job buckets bought: a job nobody reads can only evict its
    /// own stale events. Under the shared bound the same backlog pushed
    /// whoever else's terminal event out of the queue — the one event a
    /// watcher cannot afford to miss.
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
        let backlog = queue.take(noisy);
        assert_eq!(backlog.len(), MAX_EVENTS_PER_JOB);
        // Oldest first out: the ten events over the bound are the ones gone.
        let TaskEvent::Progress { done, .. } = backlog.first().expect("backlog kept") else {
            panic!("expected a progress event");
        };
        assert_eq!(*done, 10, "evicted the wrong end of the bucket");
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
        assert!(!mgr.is_running(TaskKind::Import));
        assert!(mgr.is_active(TaskKind::Import));
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
}
