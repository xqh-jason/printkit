//! Print jobs after the renderer: copies, PDF assembly, spooler submission.
//!
//! `doc.print` renders one document through the pool and submits the merged
//! PDF from the pool's finish hook. `doc.printPdf` and `doc.printBatch` either
//! skip rendering (P0) or render several documents and submit one merged PDF;
//! they register an external job, answer the client, then run on a dedicated
//! thread.
//!
//! [`PrintControl`] is the per-job handle cancellation needs: the flag stops a
//! job before submission, and the stored [`SubmittedJob`] lets the service ask
//! the platform to retract one after submission. Windows WebView2 `Print`
//! produces no spooler id, so those jobs store `job_id: None`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use printkit_core::limits::{
    MAX_BATCH_DOCS, MAX_BATCH_PAGES, MAX_BATCH_PDF_BYTES, MAX_COPIES, MIN_COPIES,
};
use printkit_core::{Document, DocumentError};
use printkit_platform::{
    HtmlPrintJob, PlatformError, PrintJob, PrinterBackend, RawJob, RenderHost, SubmittedJob,
};
use printkit_trace::{FieldKey, NullSink, Scalar, Sink, Stage, TraceId, Tracer};

use super::{
    EngineError, FailureReport, FetchPolicy, JobId, JobOutcome, RenderPool, TracedError,
    UsageRecorder, UsageRow, Warning, finish_pdf, job_failure, prepare_document, report_failure,
    report_reserve_failure, write_usage,
};

/// Upper bound for retained job controls. `job.status` keeps the same count;
/// an evicted id answers `E_NOT_FOUND` like one that never existed.
const MAX_CONTROLLED_JOBS: usize = 1024;

/// Cancellation and submission state of one print job.
#[derive(Debug, Default)]
pub struct PrintControl {
    cancel_requested: AtomicBool,
    submitted: Mutex<Option<SubmittedJob>>,
    /// Origin that reserved the job; `None` for CLI-only jobs. Status and
    /// cancel must match exactly, so one origin cannot read another's job.
    origin: Mutex<Option<String>>,
}

impl PrintControl {
    /// Create a control with no origin, no cancel request and no submission
    /// yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the job not to submit; if it already submitted, the caller retracts
    /// that spooler job.
    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
    }

    /// Whether a cancel was requested; the flag stays raised once set.
    pub fn cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    /// Bind the job to the origin that reserved it.
    pub fn bind_origin(&self, origin: Option<&str>) {
        let mut stored = self
            .origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stored.is_none() {
            *stored = origin.map(str::to_owned);
        }
    }

    /// Whether `origin` may see this job. A job reserved without an origin is
    /// only visible to callers without one.
    pub fn owned_by(&self, origin: Option<&str>) -> bool {
        self.origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            == origin
    }

    /// The spooler's handle once the job was submitted, kept for `job.cancel`
    /// to retract it.
    ///
    /// `None` before submission, and always `None` for the WebView2 HTML
    /// path, which reports no spooler id.
    pub fn submitted(&self) -> Option<SubmittedJob> {
        self.submitted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn record_submitted(&self, job: SubmittedJob) {
        *self
            .submitted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(job);
    }
}

/// Process-wide map from job id to its print control.
pub struct PrintJobs {
    inner: Mutex<Controls>,
}

struct Controls {
    controls: BTreeMap<JobId, Arc<PrintControl>>,
    order: VecDeque<JobId>,
}

impl PrintJobs {
    /// Create an empty registry that retains at most 1024 controls.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Controls {
                controls: BTreeMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Store, or replace, the control for `job_id`.
    ///
    /// Controls past the 1024-job bound are evicted oldest first; those ids
    /// then answer `E_NOT_FOUND`, like ids that never existed.
    pub fn insert(&self, job_id: JobId, control: Arc<PrintControl>) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.controls.insert(job_id.clone(), control).is_none() {
            state.order.push_back(job_id);
        }
        while state.order.len() > MAX_CONTROLLED_JOBS {
            if let Some(oldest) = state.order.pop_front() {
                state.controls.remove(&oldest);
            }
        }
    }

    /// The control for `job_id`, or `None` when it was never inserted or has
    /// been evicted.
    pub fn get(&self, job_id: &JobId) -> Option<Arc<PrintControl>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .controls
            .get(job_id)
            .cloned()
    }

    /// Control for `job_id` only when `origin` reserved it.
    pub fn get_owned(&self, job_id: &JobId, origin: Option<&str>) -> Option<Arc<PrintControl>> {
        self.get(job_id).filter(|control| control.owned_by(origin))
    }

    /// Record the reserving origin after the job has been created.
    pub fn bind_origin(&self, job_id: &JobId, origin: Option<&str>) {
        if let Some(control) = self.get(job_id) {
            control.bind_origin(origin);
        }
    }

    /// Forget `job_id`.
    ///
    /// This does not cancel anything: the caller cancels through the control
    /// first, then removes it.
    pub fn remove(&self, job_id: &JobId) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.controls.remove(job_id);
        state.order.retain(|id| id != job_id);
    }
}

impl Default for PrintJobs {
    fn default() -> Self {
        Self::new()
    }
}

/// One part of a batch: a document that needs rendering, or a finished PDF.
#[derive(Debug, Clone)]
pub enum BatchPart {
    /// A document to validate and render as one batch part.
    Document(Vec<u8>),
    /// A finished PDF used as one batch part without rendering.
    Pdf(Bytes),
}

/// A batch request before it is reserved.
#[derive(Debug, Clone)]
pub struct PrintBatch {
    /// Parts in the order they will appear in the merged PDF.
    pub parts: Vec<BatchPart>,
    /// Destination; `None` selects the system default printer.
    pub printer: Option<String>,
    /// Copies the spooler produces; validated against the protocol range
    /// before the job is reserved.
    pub copies: u32,
    /// Whether the spooler collates the copies as sets.
    pub collate: bool,
    /// Job title shown in the spooler queue.
    pub title: String,
}

/// A zero-render PDF request before it is reserved.
#[derive(Debug, Clone)]
pub struct PrintPdf {
    /// The finished PDF to submit without rendering.
    pub pdf: Bytes,
    /// Destination; `None` selects the system default printer.
    pub printer: Option<String>,
    /// Copies the spooler produces; validated against the protocol range
    /// before the job is reserved.
    pub copies: u32,
    /// Whether the spooler collates the copies as sets.
    pub collate: bool,
    /// Job title shown in the spooler queue.
    pub title: String,
}

/// A reserved job whose work starts only after the caller answered.
pub struct ReservedPrintJob {
    /// The id the client is answered with; the `queued` event follows the
    /// response.
    pub job_id: JobId,
    /// Trace opened at validation; the runner continues it through `queue`,
    /// `render` and `result`.
    pub trace_id: TraceId,
    /// Pages in the submitted PDF, copies excluded. Copies are a spooler
    /// option (`PrintJob::copies`); per-document `output.copies` is ignored in
    /// a batch.
    pub pages: u32,
    job: Option<super::ExternalJob>,
    runner: Box<dyn FnOnce(super::ExternalJobHandle) + Send>,
}

impl ReservedPrintJob {
    pub(crate) fn new(
        job_id: JobId,
        trace_id: TraceId,
        pages: u32,
        job: super::ExternalJob,
        runner: Box<dyn FnOnce(super::ExternalJobHandle) + Send>,
    ) -> Self {
        Self {
            job_id,
            trace_id,
            pages,
            job: Some(job),
            runner,
        }
    }
    /// Publish the `queued` event and start the work on its own thread.
    pub fn activate(self) {
        let ReservedPrintJob {
            job_id: _,
            trace_id: _,
            pages: _,
            job,
            runner,
        } = self;
        if let Some(job) = job {
            let handle = job.activate();
            std::thread::spawn(move || runner(handle));
        }
    }
}

/// Result of a synchronous CLI print.
#[derive(Debug, Clone)]
pub struct PrintDone {
    /// Trace closed when the spooler accepted or refused the job.
    pub trace_id: TraceId,
    /// The platform's handle to the spooled job.
    pub submitted: SubmittedJob,
    /// Pages in the submitted PDF.
    pub pages: u32,
    /// Copies handed to the spooler; [`PrintDone::sheets`] combines them with
    /// the page count.
    pub copies: u32,
    /// Warnings from the render, for example estimated table row heights.
    pub warnings: Vec<Warning>,
}

impl PrintDone {
    /// Sheets the spooler will produce: pages repeated per copy.
    pub fn sheets(&self) -> u64 {
        u64::from(self.pages) * u64::from(self.copies)
    }
}

/// Batch merges per-part PDFs into one spooler submission. It needs a backend
/// that can print PDF bytes; an HTML-only backend refuses instead of
/// submitting a PDF nobody can print.
fn reject_html_batch(backend: &dyn PrinterBackend) -> Result<(), EngineError> {
    if !backend.prints_pdf() {
        return Err(EngineError::Platform(PlatformError::Unsupported(
            "batch printing needs a backend that can print PDF bytes; this platform only \
             prints HTML"
                .to_owned(),
        )));
    }
    Ok(())
}

/// Reject a copy count outside the range the protocol documents.
fn check_copies(copies: u32) -> Result<(), EngineError> {
    if !(MIN_COPIES..=MAX_COPIES).contains(&copies) {
        return Err(EngineError::InvalidRequest {
            pointer: None,
            message: format!("copies must be between {MIN_COPIES} and {MAX_COPIES}"),
        });
    }
    Ok(())
}

/// Validate and reserve a `doc.printPdf` job.
#[allow(clippy::too_many_arguments)]
pub fn begin_print_pdf(
    request: PrintPdf,
    pool: &RenderPool,
    backend: Arc<dyn PrinterBackend>,
    jobs: &PrintJobs,
    sink: Arc<dyn Sink>,
    notify: super::JobUpdateHook,
    origin: Option<String>,
    usage: Option<UsageRecorder>,
) -> Result<ReservedPrintJob, EngineError> {
    check_copies(request.copies)?;
    // `doc.printPdf` hands PDF bytes straight to the print path; a backend
    // that cannot print PDFs refuses instead of rendering it again.
    if !backend.prints_pdf() {
        return Err(EngineError::Platform(PlatformError::Unsupported(
            "this platform cannot print PDF bytes directly; use doc.print".to_owned(),
        )));
    }
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
    let mut recv = tracer.event(Stage::Recv).field(
        FieldKey::Bytes,
        Scalar::Int(i64::try_from(request.pdf.len()).unwrap_or(i64::MAX)),
    );
    if let Some(origin) = origin.as_deref() {
        recv = recv.origin(origin.to_owned());
    }
    recv.emit();
    let pages = match printkit_pdf::page_count(&request.pdf) {
        Ok(pages) => pages,
        Err(error) => {
            let failure = EngineError::Pdf(error);
            report_failure(
                &tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: failure.to_string(),
                },
            );
            return Err(failure);
        }
    };
    if pages == 0 {
        let failure = EngineError::InvalidRequest {
            pointer: Some("/pdf/pages".to_owned()),
            message: "the PDF contains no pages".to_owned(),
        };
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        return Err(failure);
    }
    if let Err(failure) = check_pdf_passthrough_limits(pages, request.pdf.len()) {
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        return Err(failure);
    }
    tracer
        .event(Stage::Validate)
        .pages(pages)
        .dur_ms(started.elapsed())
        .emit();

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
    let printer = request.printer;
    let usage_printer = printer.clone();
    let copies = request.copies;
    let collate = request.collate;
    let pdf = request.pdf;
    let runner = Box::new(move |mut handle: super::ExternalJobHandle| {
        if matches!(handle.start(), super::ExternalAcquire::TimedOut) {
            handle.finish_expired();
            return;
        }
        let tracer = Tracer::new(runner_sink.as_ref(), runner_trace.clone());
        let row = UsageRow {
            pages,
            copies,
            printer: usage_printer.as_deref(),
            template: None,
            tags: &[],
        };
        if runner_control.cancelled() {
            finish_cancelled(&tracer, started, row, usage.as_ref(), &runner_trace);
            handle.finish(JobOutcome::Cancelled);
            return;
        }
        match submit_pdf(
            backend.as_ref(),
            PrintJob {
                printer,
                pdf,
                copies,
                collate,
                title,
                paper_mm: None,
                duplex: false,
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
                let seq = tracer
                    .event(Stage::Result)
                    .dur_ms(started.elapsed())
                    .pages(pages)
                    .emit();
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

    Ok(ReservedPrintJob {
        job_id,
        trace_id,
        pages,
        job: Some(job),
        runner,
    })
}

/// Validate and reserve a `doc.printBatch` job or a `POST /print/batch` body.
#[allow(clippy::too_many_arguments)]
pub fn begin_print_batch(
    request: PrintBatch,
    pool: &RenderPool,
    backend: Arc<dyn PrinterBackend>,
    host: Arc<dyn RenderHost>,
    jobs: &PrintJobs,
    sink: Arc<dyn Sink>,
    policy: FetchPolicy,
    notify: super::JobUpdateHook,
    origin: Option<String>,
    usage: Option<UsageRecorder>,
) -> Result<ReservedPrintJob, EngineError> {
    if request.parts.is_empty() {
        return Err(EngineError::InvalidRequest {
            pointer: None,
            message: "a batch needs at least one part".to_owned(),
        });
    }
    check_copies(request.copies)?;
    reject_html_batch(backend.as_ref())?;
    if request.parts.len() > MAX_BATCH_DOCS {
        return Err(EngineError::BatchTooLarge {
            reason: format!(
                "a batch holds at most {MAX_BATCH_DOCS} parts, got {}",
                request.parts.len()
            ),
        });
    }
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
    let total_bytes: usize = request
        .parts
        .iter()
        .map(|part| match part {
            BatchPart::Document(bytes) => bytes.len(),
            BatchPart::Pdf(bytes) => bytes.len(),
        })
        .sum();
    let mut recv = tracer.event(Stage::Recv).field(
        FieldKey::Bytes,
        Scalar::Int(i64::try_from(total_bytes).unwrap_or(i64::MAX)),
    );
    if let Some(origin) = origin.as_deref() {
        recv = recv.origin(origin.to_owned());
    }
    recv.emit();

    // Validation happens for every part before anything renders. A batch is
    // all-or-nothing: one bad document must not produce a partial print.
    let validated = match validate_batch(&request.parts, &policy) {
        Ok(validated) => validated,
        Err(failure) => {
            report_failure(
                &tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: match failure.pointer() {
                        Some(pointer) => format!("{pointer}: {failure}"),
                        None => failure.to_string(),
                    },
                },
            );
            return Err(failure);
        }
    };
    let duplex = validated.iter().any(part_duplex);
    super::ensure_duplex_supported(backend.as_ref(), duplex, &tracer, started)?;
    let total_pages: u64 = validated
        .iter()
        .map(|entry| u64::from(part_pages(entry)))
        .sum();
    if total_pages > MAX_BATCH_PAGES as u64 {
        let error = EngineError::BatchTooLarge {
            reason: format!("a batch has at most {MAX_BATCH_PAGES} pages, got {total_pages}"),
        };
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: error.code(),
                detail: error.to_string(),
            },
        );
        return Err(error);
    }
    tracer
        .event(Stage::Validate)
        .dur_ms(started.elapsed())
        .pages(u32::try_from(total_pages).unwrap_or(u32::MAX))
        .emit();

    let control = Arc::new(PrintControl::new());
    let job = pool
        .reserve_external(notify)
        .map_err(|error| report_reserve_failure(&tracer, started, error))?;
    let job_id = job.job_id().clone();
    job.set_docs_total(u32::try_from(validated.len()).unwrap_or(u32::MAX));
    jobs.insert(job_id.clone(), Arc::clone(&control));

    let runner_sink = Arc::clone(&sink);
    let runner_control = Arc::clone(&control);
    let runner_trace = trace_id.clone();
    let copies = request.copies;
    let collate = request.collate;
    let printer = request.printer;
    let usage_printer = printer.clone();
    let title = request.title;
    let fallback_pages = u32::try_from(total_pages).unwrap_or(u32::MAX);
    let runner = Box::new(move |mut handle: super::ExternalJobHandle| {
        if matches!(handle.start(), super::ExternalAcquire::TimedOut) {
            handle.finish_expired();
            return;
        }
        let tracer = Tracer::new(runner_sink.as_ref(), runner_trace.clone());
        let row = UsageRow {
            pages: fallback_pages,
            copies,
            printer: usage_printer.as_deref(),
            template: None,
            tags: &[],
        };
        if runner_control.cancelled() {
            finish_cancelled(&tracer, started, row, usage.as_ref(), &runner_trace);
            handle.finish(JobOutcome::Cancelled);
            return;
        }
        let paper = uniform_paper(&validated);
        let (pdf, pages, warnings) = match assemble_batch(
            validated,
            host.as_ref(),
            started,
            &tracer,
            &title,
            &|| runner_control.cancelled(),
            &|done, total| handle.progress(done, total),
        ) {
            Ok(Some(result)) => result,
            Ok(None) => {
                finish_cancelled(&tracer, started, row, usage.as_ref(), &runner_trace);
                handle.finish(JobOutcome::Cancelled);
                return;
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
                return;
            }
        };
        let row = UsageRow { pages, ..row };
        if runner_control.cancelled() {
            finish_cancelled(&tracer, started, row, usage.as_ref(), &runner_trace);
            handle.finish(JobOutcome::Cancelled);
            return;
        }
        match submit_pdf(
            backend.as_ref(),
            PrintJob {
                printer,
                pdf,
                copies,
                collate,
                title,
                paper_mm: paper,
                duplex,
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
                let seq = tracer
                    .event(Stage::Result)
                    .dur_ms(started.elapsed())
                    .pages(pages)
                    .emit();
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
                handle.finish(JobOutcome::Done { warnings });
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

    Ok(ReservedPrintJob {
        job_id,
        trace_id,
        pages: u32::try_from(total_pages).unwrap_or(u32::MAX),
        job: Some(job),
        runner,
    })
}

/// Render one document and submit it, for the one-shot CLI.
#[allow(clippy::too_many_arguments)]
pub fn print_document_now(
    input: &[u8],
    host: &dyn RenderHost,
    backend: &dyn PrinterBackend,
    sink: &dyn Sink,
    policy: &FetchPolicy,
    printer: Option<String>,
    copies: Option<u32>,
    usage: Option<UsageRecorder>,
) -> Result<PrintDone, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink, trace_id.clone());
    super::emit_recv(&tracer, input, None);
    let prepared = prepare_document(input, &tracer, trace_id.clone(), started, policy)?;
    let duplex = prepared.finish.duplex;
    super::ensure_duplex_supported(backend, duplex, &tracer, started)?;
    let document: Document = printkit_core::parse(input).map_err(DocumentError::Structure)?;
    let printer = printer.or_else(|| {
        document
            .output
            .printer
            .as_ref()
            .and_then(|printer| printer.name.clone())
    });
    let copies = copies.unwrap_or(document.output.copies);
    check_copies(copies)?;
    let title = document
        .meta
        .title
        .clone()
        .or_else(|| document.meta.template_id.clone())
        .unwrap_or_else(|| "printkit".to_owned());
    // One HTML print operation cannot change paper size per page. Mixed paper
    // goes through the PDF path when the backend can print PDFs; otherwise the
    // request fails before rendering.
    let mixed = !super::uniform_paper(&prepared.spec);
    let embedded = !prepared.finish.embedded.is_empty();
    let paper = if mixed {
        None
    } else {
        document.pages.first().map(|page| page.spec.size_mm())
    };
    if (mixed || embedded) && !backend.prints_pdf() {
        let detail = if embedded {
            "embedded PDF pages need a backend that can print PDFs"
        } else {
            "mixed paper sizes need a backend that can print PDFs"
        };
        let failure = EngineError::Platform(PlatformError::Unsupported(detail.to_owned()));
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Render),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        return Err(failure);
    }
    let use_html = backend.prints_html() && !mixed && !embedded;
    let (pages, warnings, submitted) = if use_html {
        // No PDF and no merge on this path: the backend prints the same HTML
        // and spec the renderer produced.
        let rendered = render_html(prepared, host, &tracer)?;
        let pages = rendered.pages;
        let warnings = rendered.warnings;
        if rendered.embedded || !super::uniform_paper(&rendered.spec) {
            let detail = if rendered.embedded {
                "embedded PDF pages cannot be printed through the platform's HTML print path"
            } else {
                "mixed paper sizes cannot be printed through one platform print operation yet"
            };
            let failure = EngineError::Platform(PlatformError::Unsupported(detail.to_owned()));
            report_failure(
                &tracer,
                FailureReport {
                    stage: Some(Stage::Render),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: failure.to_string(),
                },
            );
            return Err(failure);
        }
        let submitted = submit_html(
            backend,
            HtmlPrintJob {
                printer,
                copies,
                collate: document.output.collate,
                title,
                paper_mm: paper,
                duplex,
                html: rendered.html,
                spec: rendered.spec,
            },
            &tracer,
            started,
        )
        .map_err(|failure| failure.error)?;
        (pages, warnings, submitted)
    } else {
        let rendered = render_prepared(prepared, host, started, &tracer)?;
        let pages = rendered.pages;
        let warnings = rendered.warnings;
        let submitted = submit_pdf(
            backend,
            PrintJob {
                printer,
                pdf: rendered.pdf,
                copies,
                collate: document.output.collate,
                title,
                paper_mm: paper,
                duplex,
            },
            &tracer,
            started,
        )
        .map_err(|failure| failure.error)?;
        (pages, warnings, submitted)
    };
    let seq = tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(pages)
        .emit();
    write_usage(
        usage.as_ref(),
        &trace_id,
        seq,
        started,
        UsageRow {
            pages,
            copies,
            printer: Some(submitted.printer.as_str()),
            template: document.meta.template_id.as_deref(),
            tags: document.meta.tags.as_deref().unwrap_or(&[]),
        },
        "done",
        None,
    );
    Ok(PrintDone {
        trace_id,
        submitted,
        pages,
        copies,
        warnings,
    })
}

/// Render several documents, merge them and submit one spooler job.
#[allow(clippy::too_many_arguments)]
pub fn print_batch_now(
    parts: Vec<BatchPart>,
    host: &dyn RenderHost,
    backend: &dyn PrinterBackend,
    sink: &dyn Sink,
    policy: &FetchPolicy,
    printer: Option<String>,
    copies: u32,
    collate: bool,
    title: String,
    usage: Option<UsageRecorder>,
) -> Result<PrintDone, EngineError> {
    if parts.is_empty() {
        return Err(EngineError::InvalidRequest {
            pointer: None,
            message: "a batch needs at least one part".to_owned(),
        });
    }
    check_copies(copies)?;
    reject_html_batch(backend)?;
    if parts.len() > MAX_BATCH_DOCS {
        return Err(EngineError::BatchTooLarge {
            reason: format!("a batch holds at most {MAX_BATCH_DOCS} parts"),
        });
    }
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink, trace_id.clone());
    let bytes: usize = parts
        .iter()
        .map(|part| match part {
            BatchPart::Document(bytes) => bytes.len(),
            BatchPart::Pdf(bytes) => bytes.len(),
        })
        .sum();
    tracer
        .event(Stage::Recv)
        .field(
            FieldKey::Bytes,
            Scalar::Int(i64::try_from(bytes).unwrap_or(i64::MAX)),
        )
        .emit();
    let validated = validate_batch(&parts, policy)?;
    let duplex = validated.iter().any(part_duplex);
    super::ensure_duplex_supported(backend, duplex, &tracer, started)?;
    let total_pages: u64 = validated
        .iter()
        .map(|entry| u64::from(part_pages(entry)))
        .sum();
    if total_pages > MAX_BATCH_PAGES as u64 {
        return Err(EngineError::BatchTooLarge {
            reason: format!("a batch has at most {MAX_BATCH_PAGES} pages, got {total_pages}"),
        });
    }
    tracer
        .event(Stage::Validate)
        .dur_ms(started.elapsed())
        .pages(u32::try_from(total_pages).unwrap_or(u32::MAX))
        .emit();
    let paper = uniform_paper(&validated);
    let (pdf, pages, warnings) = match assemble_batch(
        validated,
        host,
        started,
        &tracer,
        &title,
        &|| false,
        &|_, _| {},
    ) {
        Ok(Some(result)) => result,
        Ok(None) => {
            return Err(EngineError::InvalidRequest {
                pointer: None,
                message: "the batch was cancelled".to_owned(),
            });
        }
        Err(failure) => return Err(failure.error),
    };
    let submitted = submit_pdf(
        backend,
        PrintJob {
            printer,
            pdf,
            copies,
            collate,
            title,
            paper_mm: paper,
            duplex,
        },
        &tracer,
        started,
    )
    .map_err(|failure| failure.error)?;
    let seq = tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(pages)
        .emit();
    write_usage(
        usage.as_ref(),
        &trace_id,
        seq,
        started,
        UsageRow {
            pages,
            copies,
            printer: Some(submitted.printer.as_str()),
            template: None,
            tags: &[],
        },
        "done",
        None,
    );
    Ok(PrintDone {
        trace_id,
        submitted,
        pages,
        copies,
        warnings,
    })
}

/// Print one already-finished PDF without rendering or re-assembling it.
///
/// This is the CLI's single-PDF fast path: bytes reach the spooler exactly as
/// supplied, so forms, signatures and metadata survive. `copies` is the
/// spooler copy count; the page count excludes copies.
pub fn print_pdf_now(
    pdf: Bytes,
    backend: &dyn PrinterBackend,
    sink: &dyn Sink,
    printer: Option<String>,
    copies: u32,
    title: String,
    usage: Option<UsageRecorder>,
) -> Result<PrintDone, EngineError> {
    check_copies(copies)?;
    if !backend.prints_pdf() {
        return Err(EngineError::Platform(PlatformError::Unsupported(
            "this platform cannot print PDF bytes directly; use doc.print".to_owned(),
        )));
    }
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink, trace_id.clone());
    tracer
        .event(Stage::Recv)
        .field(
            FieldKey::Bytes,
            Scalar::Int(i64::try_from(pdf.len()).unwrap_or(i64::MAX)),
        )
        .emit();
    let pages = match printkit_pdf::page_count(&pdf) {
        Ok(pages) => pages,
        Err(error) => {
            let failure = EngineError::Pdf(error);
            report_failure(
                &tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: failure.to_string(),
                },
            );
            return Err(failure);
        }
    };
    if pages == 0 {
        let failure = EngineError::InvalidRequest {
            pointer: Some("/pdf/pages".to_owned()),
            message: "the PDF contains no pages".to_owned(),
        };
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        return Err(failure);
    }
    // The passthrough path must keep the batch ceilings: without them a
    // single oversized PDF would bypass every bound the batch path applies.
    if let Err(failure) = check_pdf_passthrough_limits(pages, pdf.len()) {
        report_failure(
            &tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        return Err(failure);
    }
    tracer
        .event(Stage::Validate)
        .pages(pages)
        .dur_ms(started.elapsed())
        .emit();
    let submitted = submit_pdf(
        backend,
        PrintJob {
            printer,
            pdf,
            copies,
            collate: true,
            title,
            paper_mm: None,
            duplex: false,
        },
        &tracer,
        started,
    )
    .map_err(|failure| failure.error)?;
    let seq = tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(pages)
        .emit();
    write_usage(
        usage.as_ref(),
        &trace_id,
        seq,
        started,
        UsageRow {
            pages,
            copies,
            printer: Some(submitted.printer.as_str()),
            template: None,
            tags: &[],
        },
        "done",
        None,
    );
    Ok(PrintDone {
        trace_id,
        submitted,
        pages,
        copies,
        warnings: Vec::new(),
    })
}

/// Apply the batch page and byte ceilings to one pass-through PDF.
///
/// The CLI fast path and the desktop single-PDF route use the same numbers as
/// `doc.printBatch`, so neither can submit a PDF the batch path would refuse.
pub(crate) fn check_pdf_passthrough_limits(pages: u32, bytes: usize) -> Result<(), EngineError> {
    if pages > u32::try_from(MAX_BATCH_PAGES).unwrap_or(u32::MAX) {
        return Err(EngineError::BatchTooLarge {
            reason: format!("the PDF has {pages} pages, at most {MAX_BATCH_PAGES}"),
        });
    }
    if bytes > MAX_BATCH_PDF_BYTES {
        return Err(EngineError::BatchTooLarge {
            reason: format!("the PDF exceeds {MAX_BATCH_PDF_BYTES} bytes"),
        });
    }
    Ok(())
}

/// One validated batch part. Documents keep their prepared HTML so the
/// layout work is not repeated when the batch runs.
enum ValidatedPart {
    Document(Box<super::PreparedDocument>),
    Pdf {
        bytes: Bytes,
        pages: u32,
        metadata: bool,
    },
}

fn part_pages(part: &ValidatedPart) -> u32 {
    match part {
        ValidatedPart::Document(prepared) => prepared.finish.pages,
        ValidatedPart::Pdf { pages, .. } => *pages,
    }
}

fn part_duplex(part: &ValidatedPart) -> bool {
    match part {
        ValidatedPart::Document(prepared) => prepared.finish.duplex,
        ValidatedPart::Pdf { .. } => false,
    }
}

fn part_paper(part: &ValidatedPart) -> Option<(f64, f64)> {
    match part {
        ValidatedPart::Document(prepared) => prepared.finish.paper_mm,
        ValidatedPart::Pdf { .. } => None,
    }
}

fn validate_batch(
    parts: &[BatchPart],
    policy: &FetchPolicy,
) -> Result<Vec<ValidatedPart>, EngineError> {
    let null = NullSink;
    let tracer = Tracer::new(&null, TraceId::new());
    let mut validated = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        match part {
            BatchPart::Document(bytes) => {
                // Batch copies belong to the spooler, not to each document.
                // A per-document count would either multiply twice or be
                // silently dropped; refuse it instead of guessing.
                if let Ok(document) = printkit_core::parse(bytes)
                    && document.output.copies != 1
                {
                    return Err(EngineError::BatchPart {
                        code: "E_SCHEMA",
                        message: format!(
                            "part {index}: output.copies is {}; batch parts must use 1 and the top-level copies",
                            document.output.copies
                        ),
                        pointer: Some(format!("/documents/{index}/output/copies")),
                    });
                }
                let prepared =
                    prepare_document(bytes, &tracer, TraceId::new(), Instant::now(), policy)
                        .map_err(|error| batch_engine_error(index, error))?;
                validated.push(ValidatedPart::Document(Box::new(prepared)));
            }
            BatchPart::Pdf(bytes) => {
                // One parse supplies both the page count and the metadata
                // check; assemble reuses the cached count.
                let (pages, metadata) =
                    printkit_pdf::page_count_with_metadata(bytes).map_err(|error| {
                        EngineError::BatchPart {
                            code: super::pdf_code(&error),
                            message: format!("part {index}: {error}"),
                            pointer: Some(format!("/documents/{index}")),
                        }
                    })?;
                if pages == 0 {
                    return Err(EngineError::BatchPart {
                        code: "E_SCHEMA",
                        message: format!("part {index}: the PDF contains no pages"),
                        pointer: Some(format!("/documents/{index}")),
                    });
                }
                validated.push(ValidatedPart::Pdf {
                    bytes: bytes.clone(),
                    pages,
                    metadata,
                });
            }
        }
    }
    Ok(validated)
}

fn batch_engine_error(index: usize, error: EngineError) -> EngineError {
    match error {
        EngineError::Document(document_error) => EngineError::BatchPart {
            code: document_error.code(),
            message: document_error.to_string(),
            pointer: document_error
                .pointer()
                .map(|pointer| format!("/docs/{index}{pointer}")),
        },
        other => EngineError::BatchPart {
            code: other.code(),
            message: other.to_string(),
            pointer: Some(format!("/docs/{index}")),
        },
    }
}

/// The assembled batch must not exceed the input-side PDF ceiling.
pub(crate) fn check_merged_batch_bytes(bytes: usize) -> Result<(), EngineError> {
    if bytes > MAX_BATCH_PDF_BYTES {
        return Err(EngineError::BatchTooLarge {
            reason: format!("the merged batch exceeds {MAX_BATCH_PDF_BYTES} bytes"),
        });
    }
    Ok(())
}

/// Render and concatenate every part. Returns the merged PDF and page count.
fn assemble_batch(
    validated: Vec<ValidatedPart>,
    host: &dyn RenderHost,
    started: Instant,
    tracer: &Tracer<'_>,
    title: &str,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(u32, u32),
) -> Result<Option<(Bytes, u32, Vec<Warning>)>, TracedError> {
    let total_docs = u32::try_from(validated.len()).unwrap_or(u32::MAX);
    let mut done = 0_u32;
    let render_started = Instant::now();
    let null = NullSink;
    let null_trace = TraceId::new();
    let aggregate = Tracer::new(&null, null_trace);
    let mut pdfs = Vec::new();
    let mut warnings = Vec::new();
    let mut pages = 0_u32;
    let mut html_ms = 0_i64;
    let mut pdf_ms = 0_i64;
    let mut metadata_lost = false;
    for entry in validated {
        if cancelled() {
            return Ok(None);
        }
        // Batch copies are spooler-level (`PrintJob.copies`); per-document
        // `output.copies` is intentionally ignored so a batch is not
        // multiplied twice.
        let (pdf, part_pages, part_warnings) = match entry {
            ValidatedPart::Document(prepared) => {
                html_ms = html_ms.saturating_add(duration_ms(prepared.finish.html_ms));
                let part_started = Instant::now();
                let rendered =
                    render_prepared(*prepared, host, started, &aggregate).map_err(|error| {
                        traced_render_failure(tracer, started, render_started, error)
                    })?;
                pdf_ms = pdf_ms.saturating_add(duration_ms(part_started.elapsed()));
                (rendered.pdf, rendered.pages, rendered.warnings)
            }
            ValidatedPart::Pdf {
                bytes,
                pages,
                metadata,
            } => {
                metadata_lost |= metadata;
                (bytes, pages, Vec::new())
            }
        };
        warnings.extend(part_warnings);
        pdfs.push(pdf);
        pages = pages.saturating_add(part_pages);
        done = done.saturating_add(1);
        progress(done, total_docs);
        let bytes: usize = pdfs.iter().map(Bytes::len).sum();
        if bytes > MAX_BATCH_PDF_BYTES {
            let error = EngineError::BatchTooLarge {
                reason: format!("the merged batch exceeds {MAX_BATCH_PDF_BYTES} bytes"),
            };
            return Err(traced_render_failure(
                tracer,
                started,
                render_started,
                error,
            ));
        }
    }
    if cancelled() {
        return Ok(None);
    }
    let (merged, pages) = printkit_pdf::concat_with_title(&pdfs, Some(title)).map_err(|error| {
        traced_render_failure(tracer, started, render_started, EngineError::Pdf(error))
    })?;
    // The merged object graph can be larger than the sum of its inputs; the
    // output itself must satisfy the same ceiling before it is submitted.
    if let Err(error) = check_merged_batch_bytes(merged.len()) {
        return Err(traced_render_failure(
            tracer,
            started,
            render_started,
            error,
        ));
    }
    if metadata_lost {
        warnings.push(Warning {
            code: "W_PDF_MERGE_METADATA_LOST".to_owned(),
            message: "merging rebuilt the PDF from pages; input AcroForm, Outlines or Metadata were not carried over, while page content is preserved".to_owned(),
        });
    }
    if pages > MAX_BATCH_PAGES as u32 {
        let error = EngineError::BatchTooLarge {
            reason: format!("the merged batch has {pages} pages, at most {MAX_BATCH_PAGES}"),
        };
        return Err(traced_render_failure(
            tracer,
            started,
            render_started,
            error,
        ));
    }
    tracer
        .event(Stage::Render)
        .dur_ms(render_started.elapsed())
        .pages(pages)
        .field(FieldKey::HtmlMs, Scalar::Int(html_ms))
        .field(FieldKey::PdfMs, Scalar::Int(pdf_ms))
        .emit();
    Ok(Some((merged, pages, warnings)))
}

fn traced_render_failure(
    tracer: &Tracer<'_>,
    started: Instant,
    render_started: Instant,
    error: EngineError,
) -> TracedError {
    let result_seq = report_failure(
        tracer,
        FailureReport {
            stage: Some(Stage::Render),
            stage_dur: Some(render_started.elapsed()),
            stage_fields: &[],
            queue_pos: None,
            result_dur: Some(started.elapsed()),
            code: error.code(),
            detail: error.to_string(),
        },
    );
    TracedError { error, result_seq }
}

fn uniform_paper(validated: &[ValidatedPart]) -> Option<(f64, f64)> {
    let mut sizes = validated.iter();
    let first = part_paper(sizes.next()?)?;
    sizes
        .all(|part| {
            part_paper(part).is_some_and(|size| {
                (size.0 - first.0).abs() < 0.5 && (size.1 - first.1).abs() < 0.5
            })
        })
        .then_some(first)
}

/// One laid-out document for a backend that prints HTML itself.
struct RenderedHtml {
    pages: u32,
    warnings: Vec<Warning>,
    html: String,
    spec: printkit_platform::RenderSpec,
    embedded: bool,
}

/// Lay the document out and measure tables, but never produce PDF bytes.
fn render_html(
    prepared: super::PreparedDocument,
    host: &dyn RenderHost,
    tracer: &Tracer<'_>,
) -> Result<RenderedHtml, EngineError> {
    let prepared =
        futures::executor::block_on(super::refine_table_heights(prepared, host, tracer))?;
    let super::PreparedDocument {
        html, spec, finish, ..
    } = prepared;
    let embedded = !finish.embedded.is_empty();
    super::emit_render_event(&finish, Duration::ZERO, None, tracer);
    Ok(RenderedHtml {
        pages: finish.pages,
        warnings: finish.warnings,
        html,
        spec,
        embedded,
    })
}

/// One PDF-rendered document for the PDF submission path.
struct RenderedPrepared {
    pdf: Bytes,
    pages: u32,
    warnings: Vec<Warning>,
}

fn render_prepared(
    prepared: super::PreparedDocument,
    host: &dyn RenderHost,
    started: Instant,
    tracer: &Tracer<'_>,
) -> Result<RenderedPrepared, EngineError> {
    let prepared =
        futures::executor::block_on(super::refine_table_heights(prepared, host, tracer))?;
    let super::PreparedDocument {
        trace_id: _,
        started: prepared_started,
        html,
        spec,
        mut finish,
        pdf_only,
        ..
    } = prepared;
    if pdf_only {
        let bytes = printkit_pdf::blank_pages(&finish.page_sizes).map_err(EngineError::Pdf)?;
        let pages = finish.pages;
        let pdf = finish_pdf(&finish, bytes, Duration::ZERO, prepared_started, tracer)
            .map_err(|failure| failure.error)?;
        return Ok(RenderedPrepared {
            pdf,
            pages,
            warnings: finish.warnings,
        });
    }
    let render_started = Instant::now();
    let bytes = futures::executor::block_on(host.render_to_pdf(&html, &spec)).map_err(|error| {
        let failure = EngineError::Platform(error);
        report_failure(
            tracer,
            FailureReport {
                stage: Some(Stage::Render),
                stage_dur: Some(render_started.elapsed()),
                stage_fields: &[(FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms)))],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: failure.to_string(),
            },
        );
        failure
    })?;
    let pdf_ms = render_started.elapsed();
    if let Ok(actual) = printkit_pdf::page_count(&bytes) {
        finish.pages = actual;
    }
    let pages = finish.pages;
    let pdf = finish_pdf(&finish, bytes, pdf_ms, prepared_started, tracer)
        .map_err(|failure| failure.error)?;
    Ok(RenderedPrepared {
        pdf,
        pages,
        warnings: finish.warnings,
    })
}

/// Submit with a traced `submit` stage. The stage carries the spooler job id,
/// which is the only success code CUPS reports.
pub(crate) fn submit_pdf(
    backend: &dyn PrinterBackend,
    job: PrintJob,
    tracer: &Tracer<'_>,
    started: Instant,
) -> Result<SubmittedJob, TracedError> {
    let submit_started = Instant::now();
    match backend.submit(&job) {
        Ok(submitted) => {
            let mut event = tracer.event(Stage::Submit).dur_ms(submit_started.elapsed());
            if let Some(job_id) = submitted.job_id {
                event = event.field(FieldKey::SpoolerJob, Scalar::Int(i64::from(job_id)));
            }
            event.emit();
            Ok(submitted)
        }
        Err(error) => Err(submit_failure(tracer, started, submit_started, error)),
    }
}

/// Submit HTML for a renderer that prints to a driver itself.
pub(crate) fn submit_html(
    backend: &dyn PrinterBackend,
    job: HtmlPrintJob,
    tracer: &Tracer<'_>,
    started: Instant,
) -> Result<SubmittedJob, TracedError> {
    let submit_started = Instant::now();
    match backend.submit_html(&job) {
        Ok(submitted) => {
            let mut event = tracer.event(Stage::Submit).dur_ms(submit_started.elapsed());
            if let Some(job_id) = submitted.job_id {
                event = event.field(FieldKey::SpoolerJob, Scalar::Int(i64::from(job_id)));
            }
            event.emit();
            Ok(submitted)
        }
        Err(error) => Err(submit_failure(tracer, started, submit_started, error)),
    }
}

/// Report a failed submission with the same stage shape for every job kind.
fn submit_failure(
    tracer: &Tracer<'_>,
    started: Instant,
    submit_started: Instant,
    error: PlatformError,
) -> TracedError {
    let failure = EngineError::Platform(error);
    let result_seq = report_failure(
        tracer,
        FailureReport {
            stage: Some(Stage::Submit),
            stage_dur: Some(submit_started.elapsed()),
            stage_fields: &[],
            queue_pos: None,
            result_dur: Some(started.elapsed()),
            code: failure.code(),
            detail: failure.to_string(),
        },
    );
    TracedError {
        error: failure,
        result_seq,
    }
}

/// Submit device bytes with a traced `submit` stage, exactly like a PDF job.
pub(crate) fn submit_raw_job(
    backend: &dyn PrinterBackend,
    job: RawJob,
    tracer: &Tracer<'_>,
    started: Instant,
) -> Result<SubmittedJob, TracedError> {
    let submit_started = Instant::now();
    let length = i64::try_from(job.bytes.len()).unwrap_or(i64::MAX);
    match backend.submit_raw(&job) {
        Ok(submitted) => {
            let mut event = tracer
                .event(Stage::Submit)
                .dur_ms(submit_started.elapsed())
                .field(FieldKey::Bytes, Scalar::Int(length));
            if let Some(job_id) = submitted.job_id {
                event = event.field(FieldKey::SpoolerJob, Scalar::Int(i64::from(job_id)));
            }
            event.emit();
            Ok(submitted)
        }
        Err(error) => {
            let failure = EngineError::Platform(error);
            let result_seq = report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Submit),
                    stage_dur: Some(submit_started.elapsed()),
                    stage_fields: &[(FieldKey::Bytes, Scalar::Int(length))],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: failure.to_string(),
                },
            );
            Err(TracedError {
                error: failure,
                result_seq,
            })
        }
    }
}

pub(crate) fn finish_cancelled(
    tracer: &Tracer<'_>,
    started: Instant,
    row: UsageRow<'_>,
    usage: Option<&UsageRecorder>,
    trace_id: &TraceId,
) {
    let seq = tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(row.pages)
        .detail("cancelled")
        .emit();
    write_usage(usage, trace_id, seq, started, row, "cancelled", None);
}

fn duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
