//! Bounded render queue in front of K renderer instances.
//!
//! One worker thread owns one [`RenderHost`] instance and calls it serially;
//! concurrency comes from running several instances side by side. The queue is
//! shared, priority ordered and bounded by the number of jobs in flight
//! (running plus waiting), so a burst of clients cannot exhaust memory waiting
//! for a slot.
//!
//! A panicking host is treated as a crashed instance: the worker replaces its
//! own host and retries that job once. A second panic fails the job with
//! `E_RENDER_CRASH`. Other workers and their hosts are never touched, which is
//! what keeps one broken instance from failing the whole queue.
//!
//! Callbacks run on worker threads. `update` is called while the queue lock is
//! held so a job's `queued` notification cannot race ahead of the reply that
//! announced its id; it must therefore stay non-blocking. `finished` runs after
//! the lock is released and may do IO (merging and tracing).
//!
//! Each worker holds at most one job, so the shared bounded queue plays the
//! role of the per-instance bounded channel: the instance is single-flight by
//! construction and the queue bound is global.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use printkit_platform::{RenderHost, RenderSpec};

use crate::{Warning, platform_code};

/// Builds one renderer instance. Called once per worker at startup, and again
/// for a single worker after its instance crashes.
pub type HostFactory = Arc<dyn Fn() -> Arc<dyn RenderHost> + Send + Sync>;

/// How often the timeout monitor wakes to expire jobs that waited too long.
const TICK: Duration = Duration::from_millis(25);

/// Completed jobs kept for `job.status`. Older records are dropped first; a
/// client asking for one of them gets `E_NOT_FOUND`, the same as for an id that
/// never existed.
const MAX_RETAINED_JOBS: usize = 1024;

/// Scheduling order. Preview blocking a print is worse than the reverse, so
/// preview sorts first and exports last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPriority {
    /// Window previews, scheduled first: a held preview blocks one window,
    /// while a held print blocks the shared spooler.
    Preview,
    /// `doc.print` jobs.
    Print,
    /// File exports, scheduled last because nobody waits on them
    /// interactively.
    Export,
}

/// Process-unique job handle. Only meaningful to the process that issued it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(String);

impl JobId {
    /// The id as reported to clients, for example `job-42`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for JobId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for JobId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Pool shape. Values are validated by the configuration layer; the pool
/// clamps them here as a last line of defence so an invalid zero cannot stall
/// every worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// Worker threads, each owning one renderer instance; clamped to at
    /// least 1 by [`PoolConfig::new`].
    pub workers: u32,
    /// Jobs allowed in flight, running plus waiting. A reservation past this
    /// bound fails with [`ReserveError::Full`]; clamped to at least 1.
    pub queue_max: u32,
    /// How long an activated job may stay queued before it expires with
    /// `E_TIMEOUT`. Zero expires every waiting job on the next monitor tick.
    pub queue_timeout: Duration,
    /// Build every instance at startup instead of before its first job, so the
    /// first request does not pay for renderer start-up.
    pub warmup: bool,
}

impl PoolConfig {
    /// Build a configuration for `workers` threads and a bounded queue.
    ///
    /// `workers` and `queue_max` are clamped to 1 so a zero from the
    /// configuration layer cannot stall the pool, and `warmup` starts as
    /// `true`.
    pub fn new(workers: u32, queue_max: u32, queue_timeout: Duration) -> Self {
        Self {
            workers: workers.max(1),
            queue_max: queue_max.max(1),
            queue_timeout,
            warmup: true,
        }
    }

    /// Create instances at startup (`true`), or immediately before each
    /// worker's first render (`false`), trading latency against idle cost.
    pub fn with_warmup(mut self, warmup: bool) -> Self {
        self.warmup = warmup;
        self
    }
}

/// A job failure in protocol terms. The engine's job closers decide the code;
/// the pool only carries it to `job.status` and to the job event.
#[derive(Debug, Clone, PartialEq)]
pub struct JobFailure {
    /// The protocol `E_*` code reported to the client and written to the
    /// trace.
    pub code: &'static str,
    /// One-line message shown to the client.
    pub message: String,
    /// Structured detail, for example queue position or the failing
    /// pointer.
    pub detail: Option<serde_json::Value>,
}

impl JobFailure {
    /// Build a failure that carries no structured detail.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }
}

/// How a job ended. Cancellation is not a failure: the client asked for it,
/// and `job.status` must not surface an error code for it. Warnings ride on
/// success so a degraded render stays visible to the client.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    /// The job succeeded, with its degraded-render findings.
    Done {
        /// Findings such as estimated table row heights.
        warnings: Vec<Warning>,
    },
    /// The client cancelled the job, so it produced no result.
    Cancelled,
    /// The job failed; the failure carries the code and message.
    Failed(JobFailure),
}

/// State changes a client observes through `job` events and `job.status`.
#[derive(Debug, Clone, PartialEq)]
pub enum JobUpdate {
    /// The job became schedulable at 1-based position `queue_pos`.
    Queued {
        /// Position among the active entries at the moment of activation.
        queue_pos: u32,
    },
    /// A renderer picked the job up.
    Printing,
    /// The job finished successfully, with its degraded-render findings.
    Done {
        /// Findings such as estimated table row heights.
        warnings: Vec<Warning>,
    },
    /// The job stopped on a client cancel.
    Cancelled,
    /// The job failed; the failure carries the code and message.
    Failed {
        /// The terminal failure.
        failure: JobFailure,
    },
}

/// Lifecycle stored for `job.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    /// Waiting for one of the K render slots.
    Queued,
    /// Rendering or submitting on a worker.
    Printing,
    /// The job completed successfully, whatever its output: spooler job,
    /// file export or external batch.
    Done,
    /// Ended in the failure recorded by the job closer.
    Failed,
    /// Stopped on a client cancel; never reported as an error code.
    Cancelled,
}

/// Point-in-time view of one job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    /// Lifecycle phase at the time of the query.
    pub phase: JobPhase,
    /// 1-based queue position while [`JobPhase::Queued`]; `None` after that.
    pub queue_pos: Option<u32>,
    /// Terminal failure, present only for [`JobPhase::Failed`].
    pub failure: Option<JobFailure>,
    /// Warnings from the successful terminal render; empty on other phases.
    pub warnings: Vec<Warning>,
    /// Batch progress, when this job is an external batch: documents
    /// completed so far.
    pub docs_done: Option<u32>,
    /// Documents the batch will print in total.
    pub docs_total: Option<u32>,
}

/// How long the job waited and where it entered the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueTiming {
    /// 1-based position the job held in the queue when it was scheduled.
    pub queue_pos: u32,
    /// Time from activation until a renderer picked the job up.
    pub waited: Duration,
}

/// Why a renderer produced no bytes. Carried as plain data so a failure can
/// cross a thread boundary without keeping a reference to the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderFailure {
    /// No render slot freed up within the configured queue timeout.
    QueueTimeout {
        /// Time the job spent waiting in the queue.
        waited: Duration,
    },
    /// The renderer instance panicked twice on the same job.
    Crash,
    /// The renderer returned a failure of its own.
    Platform {
        /// Protocol `E_*` code derived from the platform error.
        code: &'static str,
        /// Message from the renderer.
        message: String,
    },
}

impl RenderFailure {
    /// The protocol `E_*` code for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::QueueTimeout { .. } => "E_TIMEOUT",
            Self::Crash => "E_RENDER_CRASH",
            Self::Platform { code, .. } => code,
        }
    }

    /// The client-facing message for this failure.
    pub fn message(&self) -> String {
        match self {
            Self::QueueTimeout { waited } => {
                format!(
                    "the render queue did not start the job within {} ms",
                    duration_ms(*waited)
                )
            }
            Self::Crash => "the renderer instance crashed twice on the same job".to_owned(),
            Self::Platform { message, .. } => message.clone(),
        }
    }
}

/// What a worker reports once it stops rendering. The `finished` hook turns the
/// renderer bytes into the job's output and emits the closing trace events.
pub struct RenderReport {
    /// How long the job waited and where it entered the queue.
    pub timing: QueueTiming,
    /// Time the renderer instance spent on the job.
    pub rendered: Duration,
    /// The renderer's bytes, or why it produced none.
    pub result: Result<Bytes, RenderFailure>,
    /// The exact HTML and paper spec handed to the renderer. Returned so a
    /// backend that prints HTML directly can reuse them instead of rendering
    /// a second document. `None` when no render ran (queue timeout or a job
    /// cancelled while queued).
    pub prepared: Option<(String, RenderSpec)>,
}

struct QueueEntry {
    id: JobId,
    seq: u64,
    priority: JobPriority,
    /// `false` between reserve and activate: the slot is taken, but the client
    /// has not been told the job id yet.
    active: bool,
    /// Position announced in the `queued` event; kept for the trace event that
    /// reports how long the job waited.
    queue_pos: Option<u32>,
    enqueued_at: Option<Instant>,
    render: PreparedRender,
}

/// Called for every state change of one job. Runs under the queue lock.
pub type JobUpdateHook = Box<dyn Fn(&JobId, JobUpdate) + Send>;

/// Called exactly once per job after the renderer stops. Runs without the
/// queue lock; the returned outcome decides the job's terminal state.
pub type JobFinishHook = Box<dyn FnOnce(RenderReport) -> JobOutcome + Send>;

/// Callbacks that tie one render to its protocol events and trace chain.
pub struct JobHooks {
    /// Called on every state change; runs under the queue lock, so it must
    /// not block.
    pub update: JobUpdateHook,
    /// Called once after the renderer stops; runs without the queue lock and
    /// may do IO.
    pub finished: JobFinishHook,
}

/// A prepared render: the HTML and paper spec plus the hooks that finish it.
pub struct PreparedRender {
    /// Scheduling order for this job.
    pub priority: JobPriority,
    /// Serialised document handed to the renderer.
    pub html: String,
    /// Paper geometry and page count for the render.
    pub spec: RenderSpec,
    /// Whether the worker must produce PDF bytes. A backend that prints HTML
    /// itself skips `render_to_pdf` entirely: the finish hook consumes the
    /// HTML/spec and never looks at the bytes.
    pub render_pdf: bool,
    /// The job can be assembled from PDF pages without a renderer.
    pub pdf_only: bool,
    /// Event and trace callbacks that finish this render.
    pub hooks: JobHooks,
}

/// Why a reservation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveError {
    /// Every slot is taken by a running or waiting job.
    Full {
        /// The configured bound that was reached.
        queue_max: u32,
        /// The position the refused job would have taken.
        queue_pos: u32,
    },
    /// The pool is shutting down.
    Stopped,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { queue_max, .. } => {
                write!(
                    formatter,
                    "the render queue is full ({queue_max} jobs in flight)"
                )
            }
            Self::Stopped => formatter.write_str("the render pool is shutting down"),
        }
    }
}

/// What happened to a `job.cancel` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelDisposition {
    /// Removed from the render queue. The pool emitted the cancelled update.
    Queued,
    /// An external job was cancelled before it started. The caller owns its
    /// update hook, so the caller emits the cancelled notification.
    ExternalQueued,
    /// The job is running. Its finalizer sees the request and decides.
    Requested,
    /// Done, failed, or already cancelled.
    Terminal,
}

/// Queue with K renderer workers.
pub struct RenderPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    config: PoolConfig,
    factory: HostFactory,
    state: Mutex<PoolState>,
    ready: Condvar,
}

struct PoolState {
    queue: Vec<QueueEntry>,
    /// Running plus waiting jobs; this is what `queue_max` bounds.
    inflight: usize,
    /// Workers currently rendering one job.
    active_workers: usize,
    /// External runners currently holding one of the K slots.
    external_active: usize,
    /// External runners waiting for a slot.
    external_waiters: usize,
    next_id: u64,
    next_seq: u64,
    records: BTreeMap<JobId, JobRecord>,
    completed: VecDeque<JobId>,
    stopped: bool,
}

#[derive(Clone)]
struct JobRecord {
    phase: JobPhase,
    queue_pos: Option<u32>,
    failure: Option<JobFailure>,
    warnings: Vec<Warning>,
    docs_done: Option<u32>,
    docs_total: Option<u32>,
}

/// A job handed to a worker.
struct TakenJob {
    id: JobId,
    html: String,
    spec: RenderSpec,
    render_pdf: bool,
    pdf_only: bool,
    update: JobUpdateHook,
    finished: JobFinishHook,
    queue_pos: u32,
    waited: Duration,
}

/// An expired job collected by the timeout thread.
struct ExpiredJob {
    id: JobId,
    update: JobUpdateHook,
    finished: JobFinishHook,
    queue_pos: u32,
    waited: Duration,
    failure: JobFailure,
}

impl RenderPool {
    /// Start every worker and the timeout monitor. With `warmup` the renderer
    /// instances are created up front, so the first request does not pay for
    /// start-up; otherwise each instance appears just before it renders.
    pub fn new(config: PoolConfig, factory: HostFactory) -> Arc<Self> {
        let inner = Arc::new(PoolInner {
            config,
            factory,
            state: Mutex::new(PoolState {
                queue: Vec::new(),
                inflight: 0,
                active_workers: 0,
                external_active: 0,
                external_waiters: 0,
                next_id: 1,
                next_seq: 1,
                records: BTreeMap::new(),
                completed: VecDeque::new(),
                stopped: false,
            }),
            ready: Condvar::new(),
        });
        for _ in 0..inner.config.workers {
            let worker = Arc::clone(&inner);
            std::thread::spawn(move || worker_loop(worker));
        }
        let monitor = Arc::clone(&inner);
        std::thread::spawn(move || timeout_loop(monitor));
        Arc::new(Self { inner })
    }

    /// Number of instances that actually render in parallel.
    pub fn workers(&self) -> u32 {
        self.inner.config.workers
    }

    /// Jobs reserved but not yet finished, including external submissions.
    pub fn inflight(&self) -> usize {
        self.inner.lock().inflight
    }

    /// Claim a slot for one job. The job does not become visible to workers
    /// until [`Reservation::activate`], so the caller can answer the client
    /// first and keep the `queued` notification behind its response.
    pub fn reserve(&self, render: PreparedRender) -> Result<Reservation, ReserveError> {
        let mut state = self.inner.lock();
        if state.stopped {
            return Err(ReserveError::Stopped);
        }
        let queue_max = self.inner.config.queue_max;
        if state.inflight >= usize::try_from(queue_max).unwrap_or(usize::MAX) {
            return Err(ReserveError::Full {
                queue_max,
                queue_pos: u32::try_from(state.inflight + 1).unwrap_or(u32::MAX),
            });
        }
        let id = JobId(format!("job-{}", state.next_id));
        state.next_id += 1;
        let seq = state.next_seq;
        state.next_seq += 1;
        let position = state
            .queue
            .partition_point(|entry| (entry.priority, entry.seq) <= (render.priority, seq));
        state.queue.insert(
            position,
            QueueEntry {
                id: id.clone(),
                seq,
                priority: render.priority,
                active: false,
                queue_pos: None,
                enqueued_at: None,
                render,
            },
        );
        state.inflight += 1;
        state.records.insert(
            id.clone(),
            JobRecord {
                phase: JobPhase::Queued,
                queue_pos: None,
                failure: None,
                warnings: Vec::new(),
                docs_done: None,
                docs_total: None,
            },
        );
        Ok(Reservation {
            inner: Arc::clone(&self.inner),
            job_id: id,
            activated: false,
        })
    }

    /// Reserve a job that does not render through this pool, such as a batch
    /// that submits a PDF assembled from several renders. It still counts
    /// against the queue bound and appears in `job.status`.
    pub fn reserve_external(&self, update: JobUpdateHook) -> Result<ExternalJob, ReserveError> {
        let mut state = self.inner.lock();
        if state.stopped {
            return Err(ReserveError::Stopped);
        }
        let queue_max = self.inner.config.queue_max;
        if state.inflight >= usize::try_from(queue_max).unwrap_or(usize::MAX) {
            return Err(ReserveError::Full {
                queue_max,
                queue_pos: u32::try_from(state.inflight + 1).unwrap_or(u32::MAX),
            });
        }
        let id = JobId(format!("job-{}", state.next_id));
        state.next_id += 1;
        state.inflight += 1;
        state.records.insert(
            id.clone(),
            JobRecord {
                phase: JobPhase::Queued,
                queue_pos: None,
                failure: None,
                warnings: Vec::new(),
                docs_done: None,
                docs_total: None,
            },
        );
        Ok(ExternalJob {
            inner: Arc::clone(&self.inner),
            job_id: id,
            update: Some(update),
            activated: false,
        })
    }

    /// Cancel a job that has not reached the spooler yet.
    pub fn cancel_job(&self, job_id: &JobId) -> Option<CancelDisposition> {
        let mut state = self.inner.lock();
        let phase = state.records.get(job_id)?.phase;
        match phase {
            JobPhase::Queued => {
                if let Some(index) = state.queue.iter().position(|entry| &entry.id == job_id) {
                    let entry = state.queue.remove(index);
                    state.inflight = state.inflight.saturating_sub(1);
                    if let Some(record) = state.records.get_mut(job_id) {
                        record.phase = JobPhase::Cancelled;
                        record.queue_pos = None;
                    }
                    state.completed.push_back(job_id.clone());
                    prune_records(&mut state);
                    self.inner.ready.notify_all();
                    drop(state);
                    let update = entry.render.hooks.update;
                    let finished = entry.render.hooks.finished;
                    update(job_id, JobUpdate::Cancelled);
                    // The worker never runs, so close the trace and usage row
                    // here; otherwise a queued cancellation has no result.
                    finished(RenderReport {
                        timing: QueueTiming {
                            queue_pos: 0,
                            waited: Duration::ZERO,
                        },
                        rendered: Duration::ZERO,
                        result: Err(RenderFailure::Platform {
                            code: "E_CANCELLED",
                            message: "cancelled before the render started".to_owned(),
                        }),
                        prepared: None,
                    });
                    return Some(CancelDisposition::Queued);
                }
                // External job: its runner owns the update hook and the
                // inflight slot. Mark it cancelled and wake the runner; it
                // releases the slot only after it actually stops.
                if let Some(record) = state.records.get_mut(job_id) {
                    record.phase = JobPhase::Cancelled;
                    record.queue_pos = None;
                }
                self.inner.ready.notify_all();
                drop(state);
                Some(CancelDisposition::ExternalQueued)
            }
            JobPhase::Printing => Some(CancelDisposition::Requested),
            JobPhase::Done | JobPhase::Failed | JobPhase::Cancelled => {
                Some(CancelDisposition::Terminal)
            }
        }
    }

    /// Current state of a job, or `None` once its record has been trimmed.
    pub fn status(&self, job_id: &JobId) -> Option<JobStatus> {
        let state = self.inner.lock();
        let record = state.records.get(job_id)?;
        let queue_pos = if record.phase == JobPhase::Queued {
            queued_position(&state.queue, job_id).or(record.queue_pos)
        } else {
            None
        };
        Some(JobStatus {
            phase: record.phase,
            queue_pos,
            failure: record.failure.clone(),
            warnings: record.warnings.clone(),
            docs_done: record.docs_done,
            docs_total: record.docs_total,
        })
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        state.stopped = true;
        // Dropping the entries drops their hooks. An export waiting on a reply
        // channel then sees the disconnect instead of hanging.
        state.queue.clear();
        state.inflight = 0;
        self.inner.ready.notify_all();
    }
}

/// Result of waiting for one of the K render slots for an external job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAcquire {
    /// The job holds a slot and may render or submit.
    Permit,
    /// The job was cancelled or the pool stopped before a slot was free. The
    /// runner still calls `finish(Cancelled)` to release its inflight slot.
    Cancelled,
    /// The job waited past `queue_timeout`. The pool already failed its
    /// record, released the inflight slot and emitted the failure; the runner
    /// must submit nothing.
    TimedOut,
}

enum ExternalStart {
    Permit(ExternalPermit),
    Cancelled,
    TimedOut(JobFailure),
}

/// Take one of the K render slots for an external job. Normal workers use the
/// same counter, so external batches and document renders together never exceed
/// the configured parallelism.
///
/// The wait is bounded by `queue_timeout`: a stuck worker cannot keep an
/// external job queued past that deadline.
fn acquire_external(inner: &Arc<PoolInner>, job_id: &JobId) -> ExternalStart {
    let mut state = inner.lock();
    let wait_start = Instant::now();
    loop {
        let workers = inner.config.workers as usize;
        let Some(record) = state.records.get(job_id) else {
            state.external_waiters = state.external_waiters.saturating_sub(1);
            return ExternalStart::Cancelled;
        };
        if record.phase == JobPhase::Cancelled || state.stopped {
            state.external_waiters = state.external_waiters.saturating_sub(1);
            inner.ready.notify_all();
            return ExternalStart::Cancelled;
        }
        if state.active_workers + state.external_active < workers {
            state.external_waiters = state.external_waiters.saturating_sub(1);
            state.external_active += 1;
            if let Some(record) = state.records.get_mut(job_id) {
                record.phase = JobPhase::Printing;
                record.queue_pos = None;
            }
            return ExternalStart::Permit(ExternalPermit {
                inner: Arc::clone(inner),
            });
        }
        let remaining = inner
            .config
            .queue_timeout
            .saturating_sub(wait_start.elapsed());
        if remaining.is_zero() {
            let waited = wait_start.elapsed();
            let failure = queue_timeout_failure(waited);
            state.external_waiters = state.external_waiters.saturating_sub(1);
            if let Some(record) = state.records.get_mut(job_id) {
                record.phase = JobPhase::Failed;
                record.queue_pos = None;
                record.failure = Some(failure.clone());
            }
            state.inflight = state.inflight.saturating_sub(1);
            state.completed.push_back(job_id.clone());
            prune_records(&mut state);
            inner.ready.notify_all();
            return ExternalStart::TimedOut(failure);
        }
        let (guard, _) = inner
            .ready
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = guard;
    }
}

struct ExternalPermit {
    inner: Arc<PoolInner>,
}

impl Drop for ExternalPermit {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        state.external_active = state.external_active.saturating_sub(1);
        self.inner.ready.notify_all();
    }
}

impl PoolInner {
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn activate(&self, job_id: &JobId) {
        let mut state = self.lock();
        let Some(index) = state.queue.iter().position(|entry| &entry.id == job_id) else {
            return;
        };
        if state.queue[index].active {
            return;
        }
        state.queue[index].active = true;
        state.queue[index].enqueued_at = Some(Instant::now());
        let ahead = state.queue[..index]
            .iter()
            .filter(|entry| entry.active)
            .count();
        let queue_pos = u32::try_from(ahead + 1).unwrap_or(u32::MAX);
        state.queue[index].queue_pos = Some(queue_pos);
        if let Some(record) = state.records.get_mut(job_id) {
            record.queue_pos = Some(queue_pos);
        }
        {
            let update = &state.queue[index].render.hooks.update;
            update(job_id, JobUpdate::Queued { queue_pos });
        }
        self.ready.notify_all();
    }

    fn cancel(&self, job_id: &JobId) {
        let mut state = self.lock();
        if let Some(index) = state.queue.iter().position(|entry| &entry.id == job_id) {
            state.queue.remove(index);
            state.inflight = state.inflight.saturating_sub(1);
            state.records.remove(job_id);
            self.ready.notify_all();
            return;
        }
        // An external reservation never entered the queue, so there is no
        // entry to remove. Its client never learned the id either (activation
        // publishes it), which is why the record is forgotten instead of kept
        // as `Cancelled`. `cancel_job` marks a client-known external job and
        // moves it to `Cancelled`; the `Queued` guard here keeps the two paths
        // from releasing the same slot twice.
        if state.records.get(job_id).map(|record| record.phase) == Some(JobPhase::Queued) {
            state.inflight = state.inflight.saturating_sub(1);
            state.records.remove(job_id);
            self.ready.notify_all();
        }
    }
}

/// A reserved slot. Dropping it without activating releases the slot and
/// forgets the job; the client never learned the id in that case.
pub struct Reservation {
    inner: Arc<PoolInner>,
    job_id: JobId,
    activated: bool,
}

impl Reservation {
    /// The id already reserved for this job.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Make the job schedulable and publish its `queued` notification.
    pub fn activate(mut self) {
        self.inner.activate(&self.job_id);
        self.activated = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.activated {
            self.inner.cancel(&self.job_id);
        }
    }
}

/// A job that renders outside the pool but still needs a queue slot, a status
/// record and job events. Held until [`Self::activate`], so the response can
/// name the job id before the first event.
pub struct ExternalJob {
    inner: Arc<PoolInner>,
    job_id: JobId,
    update: Option<JobUpdateHook>,
    activated: bool,
}

impl ExternalJob {
    /// The id already reserved for this batch.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Set the document count shown by `job.status` while this batch runs.
    pub fn set_docs_total(&self, total: u32) {
        let mut state = self.inner.lock();
        if let Some(record) = state.records.get_mut(&self.job_id) {
            record.docs_done = Some(0);
            record.docs_total = Some(total);
        }
    }

    /// Publish the `queued` event and return the handle that owns the job.
    ///
    /// [`ExternalJobHandle::start`] then waits for one of the K slots.
    pub fn activate(mut self) -> ExternalJobHandle {
        let queue_pos = {
            let mut state = self.inner.lock();
            let phase = state.records.get(&self.job_id).map(|record| record.phase);
            if phase != Some(JobPhase::Queued) {
                self.activated = true;
                return ExternalJobHandle {
                    inner: Arc::clone(&self.inner),
                    job_id: self.job_id.clone(),
                    update: self.update.take().unwrap_or_else(|| Box::new(|_, _| {})),
                    finished: true,
                    permit: None,
                };
            }
            state.external_waiters += 1;
            let position = u32::try_from(state.external_waiters).unwrap_or(u32::MAX);
            if let Some(record) = state.records.get_mut(&self.job_id) {
                record.queue_pos = Some(position);
            }
            position
        };
        let update = self.update.take().unwrap_or_else(|| Box::new(|_, _| {}));
        update(&self.job_id, JobUpdate::Queued { queue_pos });
        self.activated = true;
        ExternalJobHandle {
            inner: Arc::clone(&self.inner),
            job_id: self.job_id.clone(),
            update,
            finished: false,
            permit: None,
        }
    }
}

impl Drop for ExternalJob {
    fn drop(&mut self) {
        if !self.activated {
            self.inner.cancel(&self.job_id);
        }
    }
}

/// One running external job.
pub struct ExternalJobHandle {
    inner: Arc<PoolInner>,
    job_id: JobId,
    update: JobUpdateHook,
    finished: bool,
    permit: Option<ExternalPermit>,
}

impl ExternalJobHandle {
    /// The id of the external job.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Wait for one of the K render slots and mark the job as running.
    ///
    /// A cancelled or stopped job returns [`ExternalAcquire::Cancelled`]; the
    /// runner then calls `finish(Cancelled)` to release its inflight slot.
    /// A job that waited past `queue_timeout` returns
    /// [`ExternalAcquire::TimedOut`]: the pool already failed its record,
    /// released the slot and published the failure, so the runner must submit
    /// nothing and call [`Self::finish_expired`].
    pub fn start(&mut self) -> ExternalAcquire {
        match acquire_external(&self.inner, &self.job_id) {
            ExternalStart::Permit(permit) => {
                self.permit = Some(permit);
                (self.update)(&self.job_id, JobUpdate::Printing);
                ExternalAcquire::Permit
            }
            ExternalStart::Cancelled => ExternalAcquire::Cancelled,
            ExternalStart::TimedOut(failure) => {
                (self.update)(&self.job_id, JobUpdate::Failed { failure });
                ExternalAcquire::TimedOut
            }
        }
    }

    /// Close a handle whose slot wait expired. The pool already released the
    /// inflight slot and published the failure; this only keeps the drop guard
    /// from reporting the job a second time.
    pub fn finish_expired(mut self) {
        self.finished = true;
    }

    /// Publish the number of completed documents in a batch.
    pub fn progress(&self, done: u32, total: u32) {
        let mut state = self.inner.lock();
        if let Some(record) = state.records.get_mut(&self.job_id) {
            record.docs_done = Some(done);
            record.docs_total = Some(total);
        }
    }

    /// Store the terminal state and publish it. A job the caller already
    /// cancelled keeps the cancelled state.
    pub fn finish(mut self, outcome: JobOutcome) {
        self.finished = true;
        let mut state = self.inner.lock();
        let Some(record) = state.records.get_mut(&self.job_id) else {
            return;
        };
        if record.phase == JobPhase::Cancelled {
            state.inflight = state.inflight.saturating_sub(1);
            state.completed.push_back(self.job_id.clone());
            prune_records(&mut state);
            return;
        }
        // A timed-out job is already terminal; its slot and notification were
        // released while it waited, so a later finish must not repeat them.
        if record.phase == JobPhase::Failed {
            return;
        }
        let phase = match &outcome {
            JobOutcome::Done { .. } => JobPhase::Done,
            JobOutcome::Cancelled => JobPhase::Cancelled,
            JobOutcome::Failed(_) => JobPhase::Failed,
        };
        record.phase = phase;
        record.queue_pos = None;
        record.failure = match &outcome {
            JobOutcome::Failed(failure) => Some(failure.clone()),
            JobOutcome::Done { .. } | JobOutcome::Cancelled => None,
        };
        record.warnings = match &outcome {
            JobOutcome::Done { warnings } => warnings.clone(),
            JobOutcome::Cancelled | JobOutcome::Failed(_) => Vec::new(),
        };
        state.inflight = state.inflight.saturating_sub(1);
        state.completed.push_back(self.job_id.clone());
        prune_records(&mut state);
        drop(state);
        (self.update)(&self.job_id, update_for(outcome));
    }
}

impl Drop for ExternalJobHandle {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.inner.lock();
        let phase = state.records.get(&self.job_id).map(|record| record.phase);
        // A timeout closes the record and its inflight slot before the runner
        // returns, so the drop guard must not fail it a second time.
        if phase == Some(JobPhase::Failed) {
            return;
        }
        if phase == Some(JobPhase::Queued) {
            state.external_waiters = state.external_waiters.saturating_sub(1);
        }
        if phase == Some(JobPhase::Cancelled) {
            state.inflight = state.inflight.saturating_sub(1);
            state.completed.push_back(self.job_id.clone());
            prune_records(&mut state);
            return;
        }
        let failure = JobFailure::new("E_JOB_FAILED", "the print task ended without a result");
        let Some(record) = state.records.get_mut(&self.job_id) else {
            return;
        };
        record.phase = JobPhase::Failed;
        record.queue_pos = None;
        record.failure = Some(failure.clone());
        state.inflight = state.inflight.saturating_sub(1);
        state.completed.push_back(self.job_id.clone());
        prune_records(&mut state);
        drop(state);
        (self.update)(&self.job_id, JobUpdate::Failed { failure });
    }
}

fn worker_loop(inner: Arc<PoolInner>) {
    let mut host = inner.config.warmup.then(|| (inner.factory)());
    loop {
        let Some(job) = take_job(&inner) else {
            return;
        };
        let started = Instant::now();
        let (result, rendered) = if !job.render_pdf || job.pdf_only {
            // The finish hook either prints the HTML itself or builds the
            // PDF-only document without a WebView.
            (Ok(Bytes::new()), Duration::ZERO)
        } else {
            let host = host.get_or_insert_with(|| (inner.factory)());
            let mut attempt = 0;
            loop {
                attempt += 1;
                let rendered = catch_unwind(AssertUnwindSafe(|| {
                    futures::executor::block_on(host.render_to_pdf(&job.html, &job.spec))
                }));
                match rendered {
                    Ok(Ok(pdf)) => break (Ok(pdf), started.elapsed()),
                    Ok(Err(error)) => {
                        let failure = RenderFailure::Platform {
                            code: platform_code(&error),
                            message: error.to_string(),
                        };
                        break (Err(failure), started.elapsed());
                    }
                    Err(_) if attempt == 1 => {
                        // The instance died. Hosts are stateless handles, so a new
                        // one can be built without disturbing other workers.
                        *host = (inner.factory)();
                    }
                    Err(_) => break (Err(RenderFailure::Crash), started.elapsed()),
                }
            }
        };
        let report = RenderReport {
            timing: QueueTiming {
                queue_pos: job.queue_pos,
                waited: job.waited,
            },
            rendered,
            result,
            prepared: Some((job.html, job.spec)),
        };
        let outcome = finish_hook(job.finished, report);
        complete(&inner, &job.id, outcome, job.update);
    }
}

fn take_job(inner: &PoolInner) -> Option<TakenJob> {
    let mut state = inner.lock();
    loop {
        if state.stopped {
            return None;
        }
        if let Some(index) = state.queue.iter().position(|entry| entry.active) {
            let workers = inner.config.workers as usize;
            if state.active_workers + state.external_active >= workers {
                // External batch runners occupy some of the same K slots.
                state = inner
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            }
            let entry = state.queue.remove(index);
            state.active_workers += 1;
            let waited = entry.enqueued_at.map(|at| at.elapsed()).unwrap_or_default();
            if let Some(record) = state.records.get_mut(&entry.id) {
                record.phase = JobPhase::Printing;
                record.queue_pos = None;
            }
            let update = entry.render.hooks.update;
            update(&entry.id, JobUpdate::Printing);
            return Some(TakenJob {
                id: entry.id,
                html: entry.render.html,
                spec: entry.render.spec,
                render_pdf: entry.render.render_pdf,
                pdf_only: entry.render.pdf_only,
                update,
                finished: entry.render.hooks.finished,
                queue_pos: entry.queue_pos.unwrap_or(1),
                waited,
            });
        }
        state = inner
            .ready
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn timeout_loop(inner: Arc<PoolInner>) {
    let mut state = inner.lock();
    loop {
        if state.stopped {
            return;
        }
        let expired = collect_expired(&inner, &mut state);
        if expired.is_empty() {
            let (guard, _) = inner
                .ready
                .wait_timeout(state, TICK)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
            continue;
        }
        drop(state);
        let mut reported = Vec::with_capacity(expired.len());
        for job in expired {
            let report = RenderReport {
                timing: QueueTiming {
                    queue_pos: job.queue_pos,
                    waited: job.waited,
                },
                rendered: Duration::ZERO,
                result: Err(RenderFailure::QueueTimeout { waited: job.waited }),
                prepared: None,
            };
            let failure = match finish_hook(job.finished, report) {
                // The record already says E_TIMEOUT; a hook that reports
                // another terminal state must not turn a queue timeout into a
                // successful job.
                JobOutcome::Failed(reported) => reported,
                JobOutcome::Done { .. } | JobOutcome::Cancelled => job.failure,
            };
            (job.update)(
                &job.id,
                JobUpdate::Failed {
                    failure: failure.clone(),
                },
            );
            reported.push((job.id, failure));
        }
        state = inner.lock();
        for (id, failure) in reported {
            if let Some(record) = state.records.get_mut(&id) {
                record.failure = Some(failure);
            }
        }
    }
}

/// Remove and account for every job that waited past the timeout.
fn collect_expired(inner: &PoolInner, state: &mut PoolState) -> Vec<ExpiredJob> {
    let mut expired = Vec::new();
    let mut index = 0;
    while index < state.queue.len() {
        let due = state.queue[index].active
            && state.queue[index]
                .enqueued_at
                .is_some_and(|at| at.elapsed() >= inner.config.queue_timeout);
        if !due {
            index += 1;
            continue;
        }
        let entry = state.queue.remove(index);
        let waited = entry.enqueued_at.map(|at| at.elapsed()).unwrap_or_default();
        let failure = queue_timeout_failure(waited);
        if let Some(record) = state.records.get_mut(&entry.id) {
            record.phase = JobPhase::Failed;
            record.queue_pos = None;
            record.failure = Some(failure.clone());
        }
        state.inflight = state.inflight.saturating_sub(1);
        state.completed.push_back(entry.id.clone());
        prune_records(state);
        expired.push(ExpiredJob {
            id: entry.id,
            update: entry.render.hooks.update,
            finished: entry.render.hooks.finished,
            queue_pos: entry.queue_pos.unwrap_or(1),
            waited,
            failure,
        });
    }
    expired
}

fn complete(inner: &PoolInner, job_id: &JobId, outcome: JobOutcome, update: JobUpdateHook) {
    let mut state = inner.lock();
    let phase = match &outcome {
        JobOutcome::Done { .. } => JobPhase::Done,
        JobOutcome::Cancelled => JobPhase::Cancelled,
        JobOutcome::Failed(_) => JobPhase::Failed,
    };
    let failure = match &outcome {
        JobOutcome::Failed(failure) => Some(failure.clone()),
        JobOutcome::Done { .. } | JobOutcome::Cancelled => None,
    };
    let warnings = match &outcome {
        JobOutcome::Done { warnings } => warnings.clone(),
        JobOutcome::Cancelled | JobOutcome::Failed(_) => Vec::new(),
    };
    if let Some(record) = state.records.get_mut(job_id) {
        record.phase = phase;
        record.queue_pos = None;
        record.failure = failure;
        record.warnings = warnings;
    }
    state.inflight = state.inflight.saturating_sub(1);
    state.active_workers = state.active_workers.saturating_sub(1);
    state.completed.push_back(job_id.clone());
    prune_records(&mut state);
    drop(state);
    inner.ready.notify_all();
    update(job_id, update_for(outcome));
}

fn update_for(outcome: JobOutcome) -> JobUpdate {
    match outcome {
        JobOutcome::Done { warnings } => JobUpdate::Done { warnings },
        JobOutcome::Cancelled => JobUpdate::Cancelled,
        JobOutcome::Failed(failure) => JobUpdate::Failed { failure },
    }
}

fn prune_records(state: &mut PoolState) {
    while state.completed.len() > MAX_RETAINED_JOBS {
        if let Some(oldest) = state.completed.pop_front() {
            state.records.remove(&oldest);
        }
    }
}

fn queued_position(queue: &[QueueEntry], job_id: &JobId) -> Option<u32> {
    let index = queue.iter().position(|entry| &entry.id == job_id)?;
    let ahead = queue[..index].iter().filter(|entry| entry.active).count();
    Some(u32::try_from(ahead + 1).unwrap_or(u32::MAX))
}

/// Run the finalizer without letting a bug in it kill the worker: a dead
/// worker would shrink the pool permanently, while a failed job is recoverable.
fn finish_hook(finished: JobFinishHook, report: RenderReport) -> JobOutcome {
    match catch_unwind(AssertUnwindSafe(|| finished(report))) {
        Ok(outcome) => outcome,
        Err(_) => JobOutcome::Failed(JobFailure::new(
            "E_JOB_FAILED",
            "the job finalizer panicked",
        )),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn queue_timeout_failure(waited: Duration) -> JobFailure {
    JobFailure {
        code: "E_TIMEOUT",
        message: format!(
            "the render queue did not start the job within {} ms",
            duration_ms(waited)
        ),
        detail: Some(serde_json::json!({ "waitedMs": duration_ms(waited) })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external_pool(queue_timeout: Duration) -> Arc<RenderPool> {
        RenderPool::new(
            PoolConfig::new(1, 8, queue_timeout).with_warmup(false),
            Arc::new(|| -> Arc<dyn printkit_platform::RenderHost> {
                unreachable!("external jobs never build a render host")
            }),
        )
    }

    fn collect_updates(updates: &Arc<Mutex<Vec<JobUpdate>>>) -> JobUpdateHook {
        let updates = Arc::clone(updates);
        Box::new(move |_, update| {
            updates
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(update);
        })
    }

    #[test]
    fn inflight_counts_reserved_jobs_until_they_finish() {
        let pool = external_pool(Duration::from_secs(5));
        assert_eq!(pool.inflight(), 0);
        let job = pool.reserve_external(Box::new(|_, _| {})).unwrap();
        assert_eq!(pool.inflight(), 1);
        drop(job);
        assert_eq!(pool.inflight(), 0, "an abandoned reservation is released");
    }

    #[test]
    fn an_external_job_times_out_instead_of_waiting_forever_for_a_slot() {
        let pool = external_pool(Duration::from_millis(60));
        let mut first = pool
            .reserve_external(Box::new(|_, _| {}))
            .unwrap()
            .activate();
        assert_eq!(first.start(), ExternalAcquire::Permit);

        let updates = Arc::new(Mutex::new(Vec::new()));
        let waiting = pool.reserve_external(collect_updates(&updates)).unwrap();
        let waiting_id = waiting.job_id().clone();
        let mut waiting = waiting.activate();

        let started = Instant::now();
        assert_eq!(waiting.start(), ExternalAcquire::TimedOut);
        waiting.finish_expired();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the timeout must bound the slot wait"
        );
        let status = pool.status(&waiting_id).unwrap();
        assert_eq!(status.phase, JobPhase::Failed);
        assert_eq!(status.queue_pos, None);
        assert_eq!(
            status.failure.as_ref().map(|failure| failure.code),
            Some("E_TIMEOUT")
        );
        let failed_updates = updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|update| matches!(update, JobUpdate::Failed { .. }))
            .count();
        assert_eq!(failed_updates, 1, "the timeout is reported exactly once");

        // Releasing the held permit lets a later external job acquire the slot.
        first.finish(JobOutcome::Done {
            warnings: Vec::new(),
        });
        let mut later = pool
            .reserve_external(Box::new(|_, _| {}))
            .unwrap()
            .activate();
        assert_eq!(later.start(), ExternalAcquire::Permit);
        later.finish(JobOutcome::Done {
            warnings: Vec::new(),
        });
    }
}
