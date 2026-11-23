//! Fail-open hook execution.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use printkit_plugin_api::{
    DEFAULT_HOOK_TIMEOUT, HookCtx, HookError, HookOutcome, HookPhase, JobHook,
};

/// Fixed number of workers, so a slow hook cannot create unbounded threads.
pub const WORKER_COUNT: usize = 2;
const QUEUE_CAPACITY: usize = 64;

type HookResult = Result<Result<HookOutcome, HookError>, ()>;

struct HookJob {
    hook: &'static dyn JobHook,
    ctx: HookCtx,
    reply: SyncSender<HookResult>,
}

/// Runs registered hooks with a per-hook timeout and panic containment.
///
/// A hook failure never becomes an error: it is reported as
/// [`HookOutcome::Warn`], printed to stderr, and counted in [`Self::warnings`].
/// Hooks run on a small fixed worker pool; a timed-out hook keeps at most one
/// worker busy and cannot add threads.
pub struct HookRunner {
    timeout: Duration,
    hooks: RwLock<Vec<&'static dyn JobHook>>,
    warnings: Arc<AtomicU64>,
    sender: Option<SyncSender<HookJob>>,
    workers: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for HookRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hooks = self
            .hooks
            .read()
            .map(|hooks| hooks.len())
            .unwrap_or_default();
        formatter
            .debug_struct("HookRunner")
            .field("timeout", &self.timeout)
            .field("hooks", &hooks)
            .field("warnings", &self.warnings())
            .field("workers", &self.workers.len())
            .finish()
    }
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new(DEFAULT_HOOK_TIMEOUT)
    }
}

impl HookRunner {
    /// A runner that waits `timeout` for each hook.
    pub fn new(timeout: Duration) -> Self {
        let (sender, receiver) = sync_channel::<HookJob>(QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = (0..WORKER_COUNT)
            .filter_map(|index| {
                let receiver = Arc::clone(&receiver);
                thread::Builder::new()
                    .name(format!("printkit-hook-{index}"))
                    .spawn(move || worker_loop(receiver))
                    .ok()
            })
            .collect();
        Self {
            timeout,
            hooks: RwLock::new(Vec::new()),
            warnings: Arc::new(AtomicU64::new(0)),
            sender: Some(sender),
            workers,
        }
    }

    /// Register one static hook. The same id is registered once.
    pub fn register(&self, hook: &'static dyn JobHook) {
        let mut hooks = self
            .hooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if hooks.iter().any(|known| known.id() == hook.id()) {
            return;
        }
        hooks.push(hook);
    }

    /// How many hook failures/warnings have been contained so far.
    pub fn warnings(&self) -> u64 {
        self.warnings.load(Ordering::Relaxed)
    }

    /// Number of fixed workers. A bounded value proves slow hooks cannot grow
    /// the thread count.
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Run every registered hook that observes `phase`, in registration order.
    ///
    /// The result is always a list of observations; this method never returns
    /// an error and never panics because of a hook.
    pub fn run(&self, phase: HookPhase, ctx: &HookCtx) -> Vec<HookOutcome> {
        let hooks = self
            .hooks
            .read()
            .map(|hooks| hooks.clone())
            .unwrap_or_default();
        let mut outcomes = Vec::new();
        for hook in hooks {
            if !hook.phases().contains(&phase) {
                continue;
            }
            let id = hook.id();
            let (reply, receiver) = sync_channel::<HookResult>(1);
            let job = HookJob {
                hook,
                ctx: ctx.clone(),
                reply,
            };
            let Some(sender) = &self.sender else {
                self.contain(
                    &mut outcomes,
                    HookOutcome::Warn(format!("hook {id} has no worker queue")),
                );
                continue;
            };
            match sender.try_send(job) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    self.contain(
                        &mut outcomes,
                        HookOutcome::Warn(format!("hook {id} queue is full")),
                    );
                    continue;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.contain(
                        &mut outcomes,
                        HookOutcome::Warn(format!("hook {id} workers stopped")),
                    );
                    continue;
                }
            }
            match receiver.recv_timeout(self.timeout) {
                Ok(Ok(Ok(outcome))) => {
                    if matches!(outcome, HookOutcome::Warn(_)) {
                        self.contain(&mut outcomes, outcome);
                    } else {
                        outcomes.push(outcome);
                    }
                }
                Ok(Ok(Err(error))) => {
                    self.contain(&mut outcomes, HookOutcome::Warn(error.to_string()))
                }
                Ok(Err(_panic)) => self.contain(
                    &mut outcomes,
                    HookOutcome::Warn(HookError::Panic { hook: id }.to_string()),
                ),
                Err(RecvTimeoutError::Timeout) => self.contain(
                    &mut outcomes,
                    HookOutcome::Warn(
                        HookError::Timeout {
                            hook: id,
                            timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
                        }
                        .to_string(),
                    ),
                ),
                Err(RecvTimeoutError::Disconnected) => self.contain(
                    &mut outcomes,
                    HookOutcome::Warn(format!("hook {id} stopped without a result")),
                ),
            }
        }
        outcomes
    }

    fn contain(&self, outcomes: &mut Vec<HookOutcome>, outcome: HookOutcome) {
        self.warnings.fetch_add(1, Ordering::Relaxed);
        if let HookOutcome::Warn(message) = &outcome {
            eprintln!("printkit: plugin hook warning: {message}");
        }
        outcomes.push(outcome);
    }
}

impl Drop for HookRunner {
    fn drop(&mut self) {
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(receiver: Arc<Mutex<Receiver<HookJob>>>) {
    loop {
        let job = {
            let guard = receiver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.recv_timeout(Duration::from_millis(5))
        };
        match job {
            Ok(job) => {
                let result =
                    catch_unwind(AssertUnwindSafe(|| job.hook.run(&job.ctx))).map_err(|_| ());
                let _ = job.reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(debug_assertions)]
const ALL_PHASES: &[HookPhase] = &[
    HookPhase::BeforeRender,
    HookPhase::AfterRender,
    HookPhase::BeforeSubmit,
    HookPhase::AfterSubmit,
    HookPhase::AfterRecord,
];

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy)]
enum TestMode {
    Sleep,
    Panic,
    Warn,
}

#[cfg(debug_assertions)]
#[derive(Debug)]
struct TestHook {
    mode: TestMode,
}

#[cfg(debug_assertions)]
impl JobHook for TestHook {
    fn id(&self) -> &'static str {
        match self.mode {
            TestMode::Sleep => "test-hook-sleep",
            TestMode::Panic => "test-hook-panic",
            TestMode::Warn => "test-hook-warn",
        }
    }

    fn phases(&self) -> &'static [HookPhase] {
        ALL_PHASES
    }

    #[allow(clippy::panic)]
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        match self.mode {
            TestMode::Sleep => {
                thread::sleep(Duration::from_millis(200));
                Ok(HookOutcome::Continue)
            }
            TestMode::Panic => panic!("test hook panic"),
            TestMode::Warn => Ok(HookOutcome::Warn("test hook warning".to_owned())),
        }
    }
}

#[cfg(debug_assertions)]
static SLEEP_HOOK: TestHook = TestHook {
    mode: TestMode::Sleep,
};
#[cfg(debug_assertions)]
static PANIC_HOOK: TestHook = TestHook {
    mode: TestMode::Panic,
};
#[cfg(debug_assertions)]
static WARN_HOOK: TestHook = TestHook {
    mode: TestMode::Warn,
};

/// Test-only seam: `PRINTKIT_TEST_HOOK=sleep|panic|warn` selects one hook.
///
/// Release builds do not compile this function or the hooks.
#[cfg(debug_assertions)]
pub fn test_hook_from_env() -> Option<&'static dyn JobHook> {
    match std::env::var("PRINTKIT_TEST_HOOK").as_deref() {
        Ok("sleep") => Some(&SLEEP_HOOK),
        Ok("panic") => Some(&PANIC_HOOK),
        Ok("warn") => Some(&WARN_HOOK),
        _ => None,
    }
}
