//! Fail-open hook runner contract.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use printkit_plugin_api::{HookCtx, HookError, HookOutcome, HookPhase, JobHook};
use printkit_plugin_host::HookRunner;

struct PanicHook;
static PANIC: PanicHook = PanicHook;

impl JobHook for PanicHook {
    fn id(&self) -> &'static str {
        "panic"
    }
    fn phases(&self) -> &'static [HookPhase] {
        &[HookPhase::BeforeRender]
    }
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        panic!("intentional hook panic")
    }
}

struct SleepHook;
static SLEEP: SleepHook = SleepHook;

impl JobHook for SleepHook {
    fn id(&self) -> &'static str {
        "sleep"
    }
    fn phases(&self) -> &'static [HookPhase] {
        &[HookPhase::BeforeRender]
    }
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        thread::sleep(Duration::from_millis(200));
        Ok(HookOutcome::Continue)
    }
}

struct ShortSleepHook;
static SHORT_SLEEP: ShortSleepHook = ShortSleepHook;

impl JobHook for ShortSleepHook {
    fn id(&self) -> &'static str {
        "short-sleep"
    }
    fn phases(&self) -> &'static [HookPhase] {
        &[HookPhase::BeforeRender]
    }
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        thread::sleep(Duration::from_millis(25));
        Ok(HookOutcome::Continue)
    }
}

struct ErrorHook;
static ERROR: ErrorHook = ErrorHook;

impl JobHook for ErrorHook {
    fn id(&self) -> &'static str {
        "error"
    }
    fn phases(&self) -> &'static [HookPhase] {
        &[HookPhase::BeforeRender]
    }
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        Err(HookError::Failed {
            hook: "error",
            message: "deliberate".to_owned(),
        })
    }
}

struct Recorder {
    name: &'static str,
    seen: &'static Mutex<Vec<&'static str>>,
}
static RECORDED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static RECORDER_A: Recorder = Recorder {
    name: "a",
    seen: &RECORDED,
};
static RECORDER_B: Recorder = Recorder {
    name: "b",
    seen: &RECORDED,
};

impl JobHook for Recorder {
    fn id(&self) -> &'static str {
        self.name
    }
    fn phases(&self) -> &'static [HookPhase] {
        &[HookPhase::BeforeRender]
    }
    fn run(&self, _ctx: &HookCtx) -> Result<HookOutcome, HookError> {
        self.seen.lock().unwrap().push(self.name);
        Ok(HookOutcome::Continue)
    }
}

fn ctx() -> HookCtx {
    HookCtx {
        job_id: "job".to_owned(),
        trace_id: "trace".to_owned(),
        printer: None,
        pages: None,
    }
}

#[test]
fn hook_panic_is_contained() {
    let runner = HookRunner::new(Duration::from_millis(100));
    runner.register(&PANIC);
    let outcomes = runner.run(HookPhase::BeforeRender, &ctx());
    assert!(
        matches!(outcomes.as_slice(), [HookOutcome::Warn(message)] if message.contains("panicked"))
    );
    assert_eq!(runner.warnings(), 1);
}

#[test]
fn hook_timeout_is_contained() {
    let runner = HookRunner::new(Duration::from_millis(20));
    runner.register(&SLEEP);
    let started = Instant::now();
    let outcomes = runner.run(HookPhase::BeforeRender, &ctx());
    assert!(started.elapsed() < Duration::from_millis(150));
    assert!(
        matches!(outcomes.as_slice(), [HookOutcome::Warn(message)] if message.contains("timed out"))
    );
    assert_eq!(runner.warnings(), 1);
}

#[test]
fn hook_error_is_warn_only() {
    let runner = HookRunner::new(Duration::from_millis(100));
    runner.register(&ERROR);
    let outcomes = runner.run(HookPhase::BeforeRender, &ctx());
    assert!(
        matches!(outcomes.as_slice(), [HookOutcome::Warn(message)] if message.contains("deliberate"))
    );
    assert_eq!(runner.warnings(), 1);
}

#[test]
fn no_hooks_no_change() {
    let runner = HookRunner::new(Duration::from_millis(20));
    let outcomes = runner.run(HookPhase::AfterSubmit, &ctx());
    assert!(outcomes.is_empty());
    assert_eq!(runner.warnings(), 0);
}

#[test]
fn hook_order_is_phase_order() {
    RECORDED.lock().unwrap().clear();
    let runner = HookRunner::new(Duration::from_millis(100));
    runner.register(&RECORDER_A);
    runner.register(&RECORDER_B);
    let outcomes = runner.run(HookPhase::BeforeRender, &ctx());
    assert_eq!(outcomes.len(), 2);
    assert_eq!(RECORDED.lock().unwrap().as_slice(), ["a", "b"]);
}

#[test]
fn worker_count_constant_under_timeouts() {
    let runner = HookRunner::new(Duration::from_millis(20));
    runner.register(&SLEEP);
    assert_eq!(runner.workers(), 2);
    for _ in 0..5 {
        let _ = runner.run(HookPhase::BeforeRender, &ctx());
    }
    assert_eq!(runner.workers(), 2);
    assert_eq!(runner.warnings(), 5);
}

#[test]
fn queue_full_fail_open() {
    let runner = HookRunner::new(Duration::from_millis(1));
    runner.register(&SHORT_SLEEP);
    let mut outcomes = Vec::new();
    for _ in 0..80 {
        outcomes = runner.run(HookPhase::BeforeRender, &ctx());
    }
    assert!(!outcomes.is_empty());
    assert!(matches!(outcomes.as_slice(), [HookOutcome::Warn(_)]));
    assert!(runner.warnings() > 0);
}

#[test]
fn drop_joins_workers() {
    let runner = HookRunner::new(Duration::from_millis(20));
    runner.register(&SLEEP);
    let _ = runner.run(HookPhase::BeforeRender, &ctx());
    let started = Instant::now();
    drop(runner);
    assert!(started.elapsed() >= Duration::from_millis(100));
}
