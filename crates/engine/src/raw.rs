//! The raw channel: device bytes handed to the spooler without rendering.
//!
//! Raw jobs share the bookkeeping of PDF jobs - the pool's external job record,
//! [`PrintJobs`] and its [`PrintControl`] - so `job.status` and `job.cancel`
//! describe the real spooler job instead of a second private state.
//!
//! Validation happens before the job is reserved: an oversized payload or a
//! destination that does not accept raw bytes fails the request, and nothing
//! reaches the spooler. Graphic printing is never a fallback here: rendering
//! the bytes would print the device commands as a document.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use printkit_platform::{PlatformError, PrinterBackend, RawJob, SubmittedJob};
use printkit_trace::{FieldKey, Scalar, Sink, Stage, TraceId, Tracer};

use super::print::{PrintControl, finish_cancelled, submit_raw_job};
use super::{
    EngineError, ExternalJobHandle, FailureReport, JobOutcome, JobUpdateHook, PrintJobs,
    RenderPool, ReservedPrintJob, UsageRecorder, UsageRow, job_failure, report_failure,
    report_reserve_failure, write_usage,
};

/// One raw submission before it is reserved.
#[derive(Debug, Clone)]
pub struct RawSend {
    /// Destination; `None` selects the system default printer.
    pub printer: Option<String>,
    /// Payload handed to the spooler unchanged, within the protocol's raw
    /// byte limit.
    pub bytes: Bytes,
    /// Title shown in the spooler queue.
    pub title: String,
}

/// Result of a synchronous CLI raw submission.
#[derive(Debug, Clone)]
pub struct RawDone {
    /// Trace opened for this submission; the CLI closes it after the spooler
    /// answers.
    pub trace_id: TraceId,
    /// The platform's handle to the spooled job.
    pub submitted: SubmittedJob,
    /// Size of the payload that was submitted, in bytes.
    pub bytes: u64,
}

/// Reject a payload the protocol cannot carry.
fn check_bytes(bytes: &[u8]) -> Result<(), EngineError> {
    if bytes.is_empty() {
        return Err(EngineError::InvalidRequest {
            pointer: None,
            message: "raw bytes must not be empty".to_owned(),
        });
    }
    let limit = super::limits::MAX_RAW_BYTES;
    if bytes.len() > limit {
        return Err(EngineError::RawTooLarge {
            bytes: bytes.len(),
            limit,
        });
    }
    Ok(())
}

/// Resolve the destination and confirm it accepts raw bytes.
///
/// The check runs before anything is reserved, so `E_RAW_UNSUPPORTED` cannot
/// leave a queued job behind that a client would then cancel.
fn raw_target(backend: &dyn PrinterBackend, printer: Option<&str>) -> Result<String, EngineError> {
    let printers = backend.printers()?;
    let target = match printer {
        Some(name) => printers
            .iter()
            .find(|info| info.name == name)
            .ok_or_else(|| PlatformError::PrinterNotFound(name.to_owned()))?,
        None => printers
            .iter()
            .find(|info| info.is_default)
            .ok_or(PlatformError::NoDefaultPrinter)?,
    };
    if !target.raw {
        return Err(PlatformError::RawUnsupported(target.name.clone()).into());
    }
    Ok(target.name.clone())
}

fn validate(
    request: &RawSend,
    backend: &dyn PrinterBackend,
    tracer: &Tracer<'_>,
    started: Instant,
) -> Result<String, EngineError> {
    let checked =
        check_bytes(&request.bytes).and_then(|()| raw_target(backend, request.printer.as_deref()));
    match checked {
        Ok(printer) => {
            tracer
                .event(Stage::Validate)
                .dur_ms(started.elapsed())
                .field(
                    FieldKey::Bytes,
                    Scalar::Int(i64::try_from(request.bytes.len()).unwrap_or(i64::MAX)),
                )
                .emit();
            Ok(printer)
        }
        Err(error) => {
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: Some(started.elapsed()),
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: error.code(),
                    detail: error.to_string(),
                },
            );
            Err(error)
        }
    }
}

/// Validate and reserve a raw job. The work starts when the caller activates
/// the reservation, so the response always carries the job id first.
#[allow(clippy::too_many_arguments)]
pub fn begin_raw_send(
    request: RawSend,
    pool: &RenderPool,
    backend: Arc<dyn PrinterBackend>,
    jobs: &PrintJobs,
    sink: Arc<dyn Sink>,
    notify: JobUpdateHook,
    origin: Option<String>,
    usage: Option<UsageRecorder>,
) -> Result<ReservedPrintJob, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
    super::emit_recv(&tracer, &request.bytes, origin.as_deref());
    let printer = validate(&request, backend.as_ref(), &tracer, started)?;

    let control = Arc::new(PrintControl::new());
    let job = pool
        .reserve_external(notify)
        .map_err(|error| report_reserve_failure(&tracer, started, error))?;
    let job_id = job.job_id().clone();
    jobs.insert(job_id.clone(), Arc::clone(&control));

    let runner_sink = Arc::clone(&sink);
    let runner_control = Arc::clone(&control);
    let runner_trace = trace_id.clone();
    let title = request.title;
    let bytes = request.bytes;
    let runner = Box::new(move |mut handle: ExternalJobHandle| {
        if matches!(handle.start(), super::ExternalAcquire::TimedOut) {
            handle.finish_expired();
            return;
        }
        let tracer = Tracer::new(runner_sink.as_ref(), runner_trace.clone());
        let row = UsageRow {
            // A raw payload is an opaque device command stream, so the sheet
            // count is unknown; one submission counts as one sheet instead of
            // vanishing from the `sheets` sum.
            pages: 1,
            copies: 1,
            printer: Some(printer.as_str()),
            template: None,
            tags: &[],
        };
        if runner_control.cancelled() {
            finish_cancelled(&tracer, started, row, usage.as_ref(), &runner_trace);
            handle.finish(JobOutcome::Cancelled);
            return;
        }
        match submit_raw_job(
            backend.as_ref(),
            RawJob {
                printer: Some(printer.clone()),
                bytes,
                title,
            },
            &tracer,
            started,
        ) {
            Ok(submitted) => {
                runner_control.record_submitted(submitted.clone());
                if runner_control.cancelled() {
                    let _ = backend.cancel_submitted(&submitted);
                    finish_cancelled(
                        &tracer,
                        started,
                        UsageRow {
                            printer: Some(submitted.printer.as_str()),
                            ..row
                        },
                        usage.as_ref(),
                        &runner_trace,
                    );
                    handle.finish(JobOutcome::Cancelled);
                    return;
                }
                let mut result_event = tracer.event(Stage::Result).dur_ms(started.elapsed());
                if let Some(job_id) = submitted.job_id {
                    result_event =
                        result_event.field(FieldKey::SpoolerJob, Scalar::Int(i64::from(job_id)));
                }
                let seq = result_event.emit();
                write_usage(
                    usage.as_ref(),
                    &runner_trace,
                    seq,
                    started,
                    UsageRow {
                        printer: Some(submitted.printer.as_str()),
                        ..row
                    },
                    "done",
                    None,
                );
                handle.finish(JobOutcome::Done {
                    warnings: Vec::new(),
                });
            }
            Err(failure) => {
                write_usage(
                    usage.as_ref(),
                    &runner_trace,
                    failure.result_seq,
                    started,
                    row,
                    "failed",
                    Some(failure.error.code()),
                );
                handle.finish(JobOutcome::Failed(job_failure(&failure.error)));
            }
        }
    });

    Ok(ReservedPrintJob::new(job_id, trace_id, 0, job, runner))
}

/// Submit raw bytes and wait for the spooler, for the one-shot CLI.
pub fn raw_send_now(
    bytes: Bytes,
    backend: &dyn PrinterBackend,
    sink: &dyn Sink,
    printer: Option<String>,
    title: String,
    usage: Option<UsageRecorder>,
) -> Result<RawDone, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink, trace_id.clone());
    let request = RawSend {
        printer,
        bytes,
        title,
    };
    super::emit_recv(&tracer, &request.bytes, None);
    let printer = validate(&request, backend, &tracer, started)?;
    let length = request.bytes.len();
    let submitted = submit_raw_job(
        backend,
        RawJob {
            printer: Some(printer),
            bytes: request.bytes,
            title: request.title,
        },
        &tracer,
        started,
    )
    .map_err(|failure| failure.error)?;
    let mut result_event = tracer.event(Stage::Result).dur_ms(started.elapsed());
    if let Some(job_id) = submitted.job_id {
        result_event = result_event.field(FieldKey::SpoolerJob, Scalar::Int(i64::from(job_id)));
    }
    let seq = result_event.emit();
    write_usage(
        usage.as_ref(),
        &trace_id,
        seq,
        started,
        UsageRow {
            pages: 1,
            copies: 1,
            printer: Some(submitted.printer.as_str()),
            template: None,
            tags: &[],
        },
        "done",
        None,
    );
    Ok(RawDone {
        trace_id,
        submitted,
        bytes: u64::try_from(length).unwrap_or(u64::MAX),
    })
}
