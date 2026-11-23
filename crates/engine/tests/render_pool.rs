#![allow(clippy::panic, clippy::unwrap_used)]

//! Queue bounds, position reporting, timeout and crash recovery of the render
//! pool. The fake host stands in for a WebView instance.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use printkit_engine::{
    FetchPolicy, HostFactory, JobFailure, JobHooks, JobPhase, JobPriority, JobUpdate, PoolConfig,
    PreparedRender, RenderPool, ReserveError,
};
use printkit_platform::test_support::{DELAY_MARKER, FakeRenderHost};
use printkit_platform::{RenderHost, RenderSpec};
use printkit_trace::{MemorySink, Stage};

fn factory(host: &FakeRenderHost) -> HostFactory {
    let host = Arc::new(host.clone());
    Arc::new(move || host.clone() as Arc<dyn RenderHost>)
}

fn job() -> PreparedRender {
    job_with_updates(Arc::new(Mutex::new(Vec::new())))
}

fn job_with_updates(updates: Arc<Mutex<Vec<JobUpdate>>>) -> PreparedRender {
    PreparedRender {
        priority: JobPriority::Print,
        html: "<html></html>".to_owned(),
        spec: RenderSpec::new(210.0, 297.0),
        render_pdf: true,
        pdf_only: false,
        hooks: JobHooks {
            update: Box::new(move |_, update| {
                updates
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(update);
            }),
            finished: Box::new(|report| match report.result {
                Ok(_) => printkit_engine::JobOutcome::Done {
                    warnings: Vec::new(),
                },
                Err(failure) => printkit_engine::JobOutcome::Failed(JobFailure::new(
                    failure.code(),
                    failure.message(),
                )),
            }),
        },
    }
}

fn delayed_job(millis: u64) -> PreparedRender {
    let mut job = job();
    job.html = format!("<html>{DELAY_MARKER}{millis}</html>");
    job
}

fn reserve_and_activate(pool: &RenderPool) -> printkit_engine::JobId {
    let reservation = pool.reserve(job()).unwrap();
    let id = reservation.job_id().clone();
    reservation.activate();
    id
}

fn phase(pool: &RenderPool, id: &printkit_engine::JobId) -> JobPhase {
    pool.status(id)
        .map(|status| status.phase)
        .unwrap_or(JobPhase::Failed)
}

fn wait_for<F>(pool: &RenderPool, timeout: Duration, id: &printkit_engine::JobId, done: F)
where
    F: Fn(JobPhase) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done(phase(pool, id)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("job {id} did not reach the expected phase in {timeout:?}");
}

fn tagged_job(
    priority: JobPriority,
    tag: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
) -> PreparedRender {
    let mut job = job_with_updates(Arc::new(Mutex::new(Vec::new())));
    job.priority = priority;
    job.hooks.update = Box::new(move |_, update| {
        if update == JobUpdate::Printing {
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(tag);
        }
    });
    job
}

#[test]
fn priority_prefers_preview_then_print_then_export() {
    let host = FakeRenderHost::new().with_delay(Duration::from_millis(200));
    let pool = RenderPool::new(
        PoolConfig::new(1, 8, Duration::from_secs(10)),
        factory(&host),
    );
    let order = Arc::new(Mutex::new(Vec::new()));
    // Hold the only instance with an export, then queue the other two while it
    // runs: otherwise whichever activation the worker picks first would decide
    // the order.
    let export = pool
        .reserve(tagged_job(
            JobPriority::Export,
            "export",
            Arc::clone(&order),
        ))
        .unwrap();
    let export_id = export.job_id().clone();
    export.activate();
    let mut ids = vec![export_id.clone()];
    wait_for(&pool, Duration::from_secs(5), &export_id, |phase| {
        phase == JobPhase::Printing
    });
    for (priority, tag) in [
        (JobPriority::Print, "print"),
        (JobPriority::Preview, "preview"),
    ] {
        let reservation = pool
            .reserve(tagged_job(priority, tag, Arc::clone(&order)))
            .unwrap();
        ids.push(reservation.job_id().clone());
        reservation.activate();
    }
    for id in &ids {
        wait_for(&pool, Duration::from_secs(10), id, |phase| {
            phase == JobPhase::Done
        });
    }
    let order = order
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(order, vec!["export", "preview", "print"]);
}

#[test]
fn the_request_past_queue_max_is_refused() {
    let host = FakeRenderHost::new().with_delay(Duration::from_millis(300));
    let pool = RenderPool::new(
        PoolConfig::new(1, 32, Duration::from_secs(60)),
        factory(&host),
    );
    for _ in 0..32 {
        reserve_and_activate(&pool);
    }
    let error = match pool.reserve(job()) {
        Ok(_) => panic!("a 33rd slot must not be granted"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        ReserveError::Full {
            queue_max: 32,
            queue_pos: 33
        }
    );
}

const MINIMAL: &[u8] = br#"{
    "version": 1,
    "pages": [{"spec": {"size": "A4"}, "items": []}]
}"#;

#[test]
fn a_refused_reservation_closes_the_trace_chain() {
    let host = FakeRenderHost::new();
    let pool = RenderPool::new(
        PoolConfig::new(1, 1, Duration::from_secs(60)),
        factory(&host),
    );
    let backend: Arc<dyn printkit_platform::PrinterBackend> =
        Arc::new(printkit_platform::test_support::FakePrinterBackend::new());
    let jobs = printkit_engine::PrintJobs::new();
    let _held = printkit_engine::reserve_print(
        MINIMAL,
        &pool,
        &host,
        backend.clone(),
        &jobs,
        Arc::new(MemorySink::default()),
        &FetchPolicy::default(),
        Box::new(|_, _| {}),
        None,
    )
    .unwrap();
    let sink = Arc::new(MemorySink::default());
    let error = match printkit_engine::reserve_print(
        MINIMAL,
        &pool,
        &host,
        backend,
        &jobs,
        sink.clone(),
        &FetchPolicy::default(),
        Box::new(|_, _| {}),
        None,
    ) {
        Ok(_) => panic!("a request past the queue max must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "E_TIMEOUT");
    let events = sink.events();
    let stages: Vec<(Stage, bool)> = events.iter().map(|event| (event.stage, event.ok)).collect();
    assert_eq!(
        stages,
        vec![
            (Stage::Recv, true),
            (Stage::Auth, true),
            (Stage::Validate, true),
            (Stage::Queue, false),
            (Stage::Result, false),
        ],
        "{events:?}"
    );
    assert_eq!(events[3].code.as_deref(), Some("E_TIMEOUT"));
    assert_eq!(events[3].queue_pos, Some(2));
    assert_eq!(events[4].code.as_deref(), Some("E_TIMEOUT"));
    assert_eq!(events[0].trace_id.as_str(), events[4].trace_id.as_str());
    assert!(
        events.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "{events:?}"
    );
}

#[test]
fn queue_positions_shrink_while_jobs_wait() {
    let host = FakeRenderHost::new().with_delay(Duration::from_millis(200));
    let pool = RenderPool::new(
        PoolConfig::new(1, 32, Duration::from_secs(60)),
        factory(&host),
    );
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(reserve_and_activate(&pool));
    }
    let last = ids.last().unwrap().clone();
    let mut positions = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let status = pool.status(&last).unwrap();
        if status.phase != JobPhase::Queued {
            break;
        }
        if let Some(position) = status.queue_pos {
            positions.push(position);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        positions.windows(2).all(|pair| pair[1] <= pair[0]),
        "{positions:?}"
    );
    assert!(
        positions.first().is_some_and(|first| *first > 1),
        "{positions:?}"
    );
    assert!(positions.len() >= 2, "{positions:?}");
    wait_for(&pool, Duration::from_secs(10), &last, |phase| {
        phase == JobPhase::Done
    });
}

#[test]
fn only_the_crashed_instance_is_rebuilt_and_the_job_is_retried() {
    let creations = Arc::new(AtomicUsize::new(0));
    let crashing = Arc::new(FakeRenderHost::new().with_panic());
    let factory: HostFactory = {
        let creations = Arc::clone(&creations);
        let crashing = Arc::clone(&crashing);
        Arc::new(move || {
            if creations.fetch_add(1, Ordering::SeqCst) == 0 {
                Arc::clone(&crashing) as Arc<dyn RenderHost>
            } else {
                Arc::new(FakeRenderHost::new()) as Arc<dyn RenderHost>
            }
        })
    };
    let pool = RenderPool::new(PoolConfig::new(2, 16, Duration::from_secs(10)), factory);
    // Fill both instances before queueing the rest: with immediately returning
    // hosts a worker could otherwise drain the queue before the panicking
    // instance ever renders, leaving the recovery path untested.
    let mut reservations = (0..2)
        .map(|_| pool.reserve(delayed_job(200)).unwrap())
        .collect::<Vec<_>>();
    let mut ids = reservations
        .iter()
        .map(|reservation| reservation.job_id().clone())
        .collect::<Vec<_>>();
    for reservation in reservations.drain(..) {
        reservation.activate();
    }
    for id in &ids {
        wait_for(&pool, Duration::from_secs(10), id, |phase| {
            phase == JobPhase::Printing
        });
    }
    let mut reservations = (0..4)
        .map(|_| pool.reserve(delayed_job(200)).unwrap())
        .collect::<Vec<_>>();
    ids.extend(
        reservations
            .iter()
            .map(|reservation| reservation.job_id().clone()),
    );
    for reservation in reservations.drain(..) {
        reservation.activate();
    }
    for id in &ids {
        wait_for(&pool, Duration::from_secs(10), id, |phase| {
            phase == JobPhase::Done
        });
    }
    // Two instances at start plus exactly one replacement for the crashed one;
    // the panicking instance rendered exactly once and was never reused.
    assert_eq!(creations.load(Ordering::SeqCst), 3);
    assert_eq!(crashing.calls(), 1);
}

#[test]
fn a_cold_pool_creates_its_host_before_the_first_job() {
    let creations = Arc::new(AtomicUsize::new(0));
    let factory: HostFactory = {
        let creations = Arc::clone(&creations);
        Arc::new(move || {
            creations.fetch_add(1, Ordering::SeqCst);
            Arc::new(FakeRenderHost::new()) as Arc<dyn RenderHost>
        })
    };
    let pool = RenderPool::new(
        PoolConfig::new(1, 8, Duration::from_secs(10)).with_warmup(false),
        factory,
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(creations.load(Ordering::SeqCst), 0);
    let id = reserve_and_activate(&pool);
    wait_for(&pool, Duration::from_secs(10), &id, |phase| {
        phase == JobPhase::Done
    });
    assert_eq!(creations.load(Ordering::SeqCst), 1);
}

#[test]
fn two_crashes_fail_the_job_with_render_crash() {
    let host = FakeRenderHost::new().with_panic();
    let pool = RenderPool::new(
        PoolConfig::new(1, 8, Duration::from_secs(10)),
        factory(&host),
    );
    let id = reserve_and_activate(&pool);
    wait_for(&pool, Duration::from_secs(10), &id, |phase| {
        phase == JobPhase::Failed
    });
    let status = pool.status(&id).unwrap();
    assert_eq!(
        status.failure.as_ref().map(|failure| failure.code),
        Some("E_RENDER_CRASH")
    );
    assert_eq!(host.calls(), 2, "one retry means exactly two attempts");
}

#[test]
fn a_job_that_waits_past_the_timeout_fails() {
    let host = FakeRenderHost::new().with_delay(Duration::from_millis(600));
    let pool = RenderPool::new(
        PoolConfig::new(1, 8, Duration::from_millis(150)),
        factory(&host),
    );
    let first = reserve_and_activate(&pool);
    let second = reserve_and_activate(&pool);
    wait_for(&pool, Duration::from_secs(5), &second, |phase| {
        phase == JobPhase::Failed
    });
    let status = pool.status(&second).unwrap();
    assert_eq!(
        status.failure.as_ref().map(|failure| failure.code),
        Some("E_TIMEOUT")
    );
    wait_for(&pool, Duration::from_secs(5), &first, |phase| {
        phase == JobPhase::Done
    });
}

fn external(pool: &RenderPool) -> printkit_engine::ExternalJob {
    pool.reserve_external(Box::new(|_, _| {})).unwrap()
}

fn external_slot_is_free(pool: &RenderPool) -> bool {
    match pool.reserve_external(Box::new(|_, _| {})) {
        Ok(job) => {
            drop(job);
            true
        }
        Err(_) => false,
    }
}

#[test]
fn dropping_an_unactivated_external_job_releases_its_slot() {
    let host = FakeRenderHost::new();
    let pool = RenderPool::new(
        PoolConfig::new(1, 1, Duration::from_secs(60)),
        factory(&host),
    );
    let pending = external(&pool);
    let id = pending.job_id().clone();
    assert_eq!(
        pool.status(&id).map(|status| status.phase),
        Some(JobPhase::Queued),
        "the slot must be visible while the response is being written"
    );
    // The client never learned this id, so it must be forgotten, not marked
    // Cancelled.
    drop(pending);
    assert_eq!(pool.status(&id), None);
    // queue_max is 1: this only succeeds if the dropped job gave its slot back.
    let next = external(&pool);
    drop(next);
    assert!(external_slot_is_free(&pool));
}

#[test]
fn repeated_external_drops_do_not_leak_slots() {
    let host = FakeRenderHost::new();
    let pool = RenderPool::new(
        PoolConfig::new(1, 1, Duration::from_secs(60)),
        factory(&host),
    );
    for _ in 0..5 {
        let pending = external(&pool);
        let id = pending.job_id().clone();
        drop(pending);
        assert_eq!(pool.status(&id), None);
        assert!(external_slot_is_free(&pool));
    }
}

#[test]
fn cancel_job_then_drop_releases_an_external_slot_only_once() {
    let host = FakeRenderHost::new();
    let pool = RenderPool::new(
        PoolConfig::new(1, 2, Duration::from_secs(60)),
        factory(&host),
    );
    let cancelled = external(&pool).activate();
    let cancelled_id = cancelled.job_id().clone();
    let other = external(&pool).activate();
    let other_id = other.job_id().clone();
    assert_eq!(
        pool.cancel_job(&cancelled_id),
        Some(printkit_engine::CancelDisposition::ExternalQueued)
    );
    assert_eq!(phase(&pool, &cancelled_id), JobPhase::Cancelled);
    // The handle drop must not release `other`'s slot as well.
    drop(cancelled);
    assert_eq!(phase(&pool, &other_id), JobPhase::Queued);
    let third = external(&pool);
    assert_eq!(
        pool.status(third.job_id()).map(|status| status.phase),
        Some(JobPhase::Queued)
    );
    assert!(
        pool.reserve_external(Box::new(|_, _| {})).is_err(),
        "two live external jobs plus the cancelled one must fill queue_max=2"
    );
    drop(third);
    drop(other);
}

#[test]
fn activating_an_external_job_keeps_the_slot_until_it_finishes() {
    let host = FakeRenderHost::new();
    let pool = RenderPool::new(
        PoolConfig::new(1, 1, Duration::from_secs(60)),
        factory(&host),
    );
    let handle = external(&pool).activate();
    let id = handle.job_id().clone();
    assert_eq!(phase(&pool, &id), JobPhase::Queued);
    drop(handle);
    assert_eq!(phase(&pool, &id), JobPhase::Failed);
    let next = external(&pool);
    drop(next);
}

#[test]
fn dropping_an_unactivated_reservation_releases_its_slot() {
    let host = FakeRenderHost::new().with_delay(Duration::from_millis(100));
    let pool = RenderPool::new(
        PoolConfig::new(1, 1, Duration::from_secs(60)),
        factory(&host),
    );
    let reservation = pool.reserve(job()).unwrap();
    let id = reservation.job_id().clone();
    drop(reservation);
    assert_eq!(pool.status(&id), None);
    // queue_max is 1: the slot must be reusable before a worker sees the job.
    let reservation = pool.reserve(job()).unwrap();
    let id = reservation.job_id().clone();
    reservation.activate();
    wait_for(&pool, Duration::from_secs(5), &id, |phase| {
        phase == JobPhase::Done
    });
}
