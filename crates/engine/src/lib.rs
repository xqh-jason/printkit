#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! Job orchestration.
//!
//! The engine owns the sequence and the trace events: receive, parse, validate,
//! queue, render, result. It talks to the platform only through [`RenderHost`],
//! so the same orchestration works with the real renderer or the test double.
//! The service renders through [`RenderPool`]; the one-shot CLI renders on the
//! calling thread because its main loop must stay free to answer the host.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use printkit_core::data_uri::decode_data_uri;
use printkit_core::{DocumentError, Duplex, Item, Validated, Violation};
use printkit_pdf::{EmbeddedPage, EmbeddedPdf, EmbeddedProblem, PtRect};
use printkit_platform::{
    HtmlPrintJob, MM_TO_PT, PlatformError, PrintJob, PrinterBackend, RenderHost, RenderSpec,
};
use printkit_render::Fetched;
use printkit_store::{NewPrintRecord, Store};
use printkit_trace::{FieldKey, Scalar, Sink, Stage, TraceId, Tracer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

mod export_path;
mod fetch;
mod pool;
mod print;
mod raw;

pub use export_path::ExportPathError;
#[cfg(feature = "url-fetch")]
pub use fetch::Fetcher;
pub use fetch::{FetchError, FetchPolicy};
pub use pool::{
    CancelDisposition, ExternalAcquire, ExternalJob, ExternalJobHandle, HostFactory, JobFailure,
    JobFinishHook, JobHooks, JobId, JobOutcome, JobPhase, JobPriority, JobStatus, JobUpdate,
    JobUpdateHook, PoolConfig, PreparedRender, QueueTiming, RenderFailure, RenderPool,
    RenderReport, Reservation, ReserveError,
};
pub use print::{
    BatchPart, PrintBatch, PrintControl, PrintDone, PrintJobs, PrintPdf, ReservedPrintJob,
    begin_print_batch, begin_print_pdf, print_batch_now, print_document_now, print_pdf_now,
};
pub use printkit_core::limits;
pub use printkit_core::limits::MAX_FETCH_BYTES;
pub use raw::{RawDone, RawSend, begin_raw_send, raw_send_now};

/// A problem code and message carried in a successful result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Warning code in `W_*` form, e.g. `W_TABLE_ROW_TOO_TALL`.
    pub code: String,
    /// One-line message shown to the client.
    pub message: String,
}

/// Successful export.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// Trace of the export, from `recv` through `result`.
    pub trace_id: TraceId,
    /// The rendered PDF bytes.
    pub pdf: Bytes,
    /// The intermediate HTML, kept for `--dump-html`.
    pub html: String,
    /// Pages in `pdf`.
    pub pages: u32,
    /// Degraded-render findings from the render pass.
    pub warnings: Vec<Warning>,
}

/// HTML and geometry prepared for a preview window.
///
/// Produced by the same validation and serialisation as a print job, so the
/// window cannot show a document that differs from what printing would render.
#[derive(Debug, Clone)]
pub struct PreviewRender {
    /// Serialised HTML handed to the preview window.
    pub html: String,
    /// Paper geometry and page count, continuation pages included.
    pub spec: RenderSpec,
    /// Resolved window title: the document title, template id or `printkit`.
    pub title: String,
    /// Findings from the shared render pass. Preview and print serialise the
    /// same HTML, so the two must not disagree about what to warn on.
    pub warnings: Vec<Warning>,
    /// Trace of this preview. It ends at `render`; only the caller knows
    /// whether the window opened, so the `result` event is the caller's.
    pub trace_id: TraceId,
}

/// Successful `exportPdf` call.
#[derive(Debug, Clone)]
pub struct ExportedFile {
    /// Trace of the export; it ends at `result`, successful or failed.
    pub trace_id: TraceId,
    /// Path relative to the data directory, always under `exports/`.
    pub path: String,
    /// Canonical absolute path of the written file.
    pub absolute_path: String,
    /// Pages written to the file.
    pub pages: u32,
    /// Size of the written file, in bytes.
    pub bytes: u64,
    /// Degraded-render findings from the render pass.
    pub warnings: Vec<Warning>,
}

/// Everything the engine can fail with, in protocol terms.
///
/// [`EngineError::code`] maps each variant to the `E_*` code the client sees;
/// [`EngineError::detail`] carries structured data where a variant has it.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The document is not JSON, fails the schema, or violates a semantic
    /// rule. The wrapped error carries the pointer and violations.
    #[error(transparent)]
    Document(#[from] DocumentError),
    /// An embedded PDF is encrypted, unsupported, too deep, or targets a page
    /// outside the document.
    #[error(transparent)]
    Pdf(#[from] printkit_pdf::PdfError),
    /// The platform backend or renderer refused or failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
    /// The renderer could not serialise the document.
    #[error(transparent)]
    Render(#[from] printkit_render::RenderError),
    /// A `data.url` body could not be fetched under the policy.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// The `exportPdf` path breaks the `exports/` contract, or the filesystem
    /// failed while resolving it.
    #[error(transparent)]
    Path(#[from] ExportPathError),
    /// The pool had no free slot, or had already stopped.
    #[error("{0}")]
    Queue(ReserveError),
    /// A pooled job failed after entering the queue.
    #[error("{message}", message = .0.message)]
    Job(JobFailure),
    /// A batch exceeded a documented runtime limit.
    #[error("{reason}")]
    BatchTooLarge {
        /// The limit that was exceeded, named together with the actual
        /// count.
        reason: String,
    },
    /// One part of a batch failed validation; `pointer` names that part.
    #[error("{message}")]
    BatchPart {
        /// `E_*` code of the part's own failure.
        code: &'static str,
        /// Failure message, prefixed with the part's index when known.
        message: String,
        /// JSON Pointer into the offending part, when the failure has one.
        pointer: Option<String>,
    },
    /// A raw submission exceeded the documented byte limit.
    #[error("raw bytes are at most {limit} bytes, got {bytes}")]
    RawTooLarge {
        /// Payload size received, in bytes.
        bytes: usize,
        /// The protocol's raw payload limit, in bytes.
        limit: usize,
    },
    /// A document exceeded the runtime byte limit.
    #[error("document is {bytes} bytes, limit is {limit}")]
    DocumentTooLarge {
        /// Request size received, in bytes.
        bytes: usize,
        /// The runtime limit for one document, in bytes.
        limit: usize,
    },
    /// A request field is outside its documented range.
    #[error("{message}")]
    InvalidRequest {
        /// Which field is out of range and what the allowed range is.
        message: String,
        /// JSON Pointer to the offending field, when known.
        pointer: Option<String>,
    },
    /// A valid request asks for a capability this build cannot provide.
    #[error("{message}")]
    Unsupported {
        /// The missing capability and why it is unavailable.
        message: String,
        /// JSON Pointer to the field that asked for it, when known.
        pointer: Option<String>,
    },
}

impl EngineError {
    /// The `E_*` code reported to the caller and written to the trace.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Document(error) => error.code(),
            Self::Path(error) => error.code(),
            Self::Fetch(error) => error.code(),
            Self::Pdf(error) => pdf_code(error),
            Self::Render(_) => "E_JOB_FAILED",
            Self::Platform(error) => platform_code(error),
            Self::Queue(error) => match error {
                ReserveError::Full { .. } => "E_TIMEOUT",
                ReserveError::Stopped => "E_JOB_FAILED",
            },
            Self::Job(failure) => failure.code,
            Self::BatchTooLarge { .. } => "E_TOO_LARGE",
            Self::RawTooLarge { .. } => "E_TOO_LARGE",
            Self::DocumentTooLarge { .. } => "E_TOO_LARGE",
            Self::BatchPart { code, .. } => code,
            Self::Unsupported { .. } => "E_UNSUPPORTED",
            Self::InvalidRequest { .. } => "E_SCHEMA",
        }
    }

    /// The first JSON Pointer for a schema failure, if any.
    pub fn pointer(&self) -> Option<&str> {
        match self {
            Self::Path(_) | Self::Fetch(_) | Self::Queue(_) | Self::Job(_) => None,
            Self::Document(error) => error.pointer(),
            Self::Unsupported { pointer, .. } => pointer.as_deref(),
            Self::BatchPart { pointer, .. } => pointer.as_deref(),
            Self::Pdf(_) | Self::Platform(_) | Self::Render(_) | Self::BatchTooLarge { .. } => None,
            Self::RawTooLarge { .. } | Self::DocumentTooLarge { .. } => None,
            Self::InvalidRequest { pointer, .. } => pointer.as_deref(),
        }
    }

    /// Every semantic violation; empty for a structural or runtime failure.
    pub fn violations(&self) -> &[Violation] {
        match self {
            Self::Document(error) => error.violations(),
            Self::Pdf(_)
            | Self::Path(_)
            | Self::Platform(_)
            | Self::Render(_)
            | Self::Fetch(_)
            | Self::Queue(_)
            | Self::Job(_)
            | Self::BatchTooLarge { .. }
            | Self::RawTooLarge { .. }
            | Self::DocumentTooLarge { .. }
            | Self::BatchPart { .. }
            | Self::Unsupported { .. }
            | Self::InvalidRequest { .. } => &[],
        }
    }

    /// Structured `detail` for a failure that names an object or a pointer.
    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            Self::Document(error) => Some(serde_json::json!({
                "pointer": error.pointer(),
                "violations": error.violations(),
            })),
            Self::Pdf(printkit_pdf::PdfError::UnsupportedObject { object, reason }) => {
                Some(serde_json::json!({
                    "object": format!("{} {} R", object.0, object.1),
                    "reason": reason,
                }))
            }
            Self::Pdf(printkit_pdf::PdfError::TooDeep { limit }) => Some(
                serde_json::json!({ "reason": format!("object graph is deeper than {limit}") }),
            ),
            Self::Pdf(printkit_pdf::PdfError::EmbeddedPageOutOfRange { page, pages }) => {
                Some(serde_json::json!({ "page": page, "pages": pages }))
            }
            Self::Queue(ReserveError::Full {
                queue_max,
                queue_pos,
            }) => Some(serde_json::json!({
                "queueMax": queue_max,
                "queuePos": queue_pos,
            })),
            Self::Job(failure) => failure.detail.clone(),
            Self::BatchTooLarge { reason } => Some(serde_json::json!({ "reason": reason })),
            Self::BatchPart {
                message, pointer, ..
            } => Some(serde_json::json!({
                "pointer": pointer,
                "message": message,
            })),
            Self::RawTooLarge { bytes, limit } => {
                Some(serde_json::json!({ "bytes": bytes, "limit": limit }))
            }
            Self::DocumentTooLarge { bytes, limit } => {
                Some(serde_json::json!({ "bytes": bytes, "limit": limit }))
            }
            Self::InvalidRequest { message, pointer } => {
                let mut detail = serde_json::json!({ "message": message });
                if let Some(pointer) = pointer {
                    detail["pointer"] = serde_json::Value::String(pointer.clone());
                }
                Some(detail)
            }
            Self::Unsupported { message, pointer } => Some(serde_json::json!({
                "pointer": pointer,
                "message": message,
            })),
            Self::Pdf(_)
            | Self::Path(_)
            | Self::Fetch(_)
            | Self::Platform(_)
            | Self::Render(_)
            | Self::Queue(_) => None,
        }
    }
}

impl From<JobFailure> for EngineError {
    fn from(failure: JobFailure) -> Self {
        Self::Job(failure)
    }
}

/// Parse, validate, render and trace one document.
pub async fn export_pdf(
    input: &[u8],
    host: &dyn RenderHost,
    sink: &dyn Sink,
    policy: &FetchPolicy,
) -> Result<ExportResult, EngineError> {
    let trace_id = TraceId::new();
    let tracer = Tracer::new(sink, trace_id.clone());
    let started = Instant::now();
    emit_recv(&tracer, input, None);
    run_export(input, host, &tracer, trace_id, started, policy).await
}

/// Parse, validate and serialise one document for a preview window.
///
/// The `render` stage reports only the HTML phase: no PDF exists until the
/// window's Print control reserves a job through [`reserve_print`]. That is
/// deliberate, not a missing field - pushing bytes through a renderer here
/// would make preview and print two paths.
///
/// The trace stops at `render` on success. A preparation failure ends its own
/// trace at a failed `result`; a prepared document is only known to be
/// previewable once a window exists, so the caller emits `result`.
pub fn prepare_preview(
    input: &[u8],
    sink: &dyn Sink,
    origin: Option<&str>,
    policy: &FetchPolicy,
    host: &dyn RenderHost,
) -> Result<PreviewRender, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink, trace_id.clone());
    emit_recv(&tracer, input, origin);
    let mut auth = tracer.event(Stage::Auth).dur_ms(started.elapsed());
    if let Some(origin) = origin {
        auth = auth.origin(origin.to_owned());
    }
    auth.emit();
    let prepared = prepare_document(input, &tracer, trace_id.clone(), started, policy)?;
    let prepared = futures::executor::block_on(refine_table_heights(prepared, host, &tracer))?;
    let PreparedDocument {
        html, spec, finish, ..
    } = prepared;
    let mut render = tracer
        .event(Stage::Render)
        .dur_ms(finish.html_ms)
        .pages(finish.pages)
        .field(FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms)));
    if let Some(fetch_ms) = finish.fetch_ms {
        render = render.field(FieldKey::FetchMs, Scalar::Int(duration_ms(fetch_ms)));
    }
    render.emit();
    Ok(PreviewRender {
        html,
        spec,
        title: finish.title,
        warnings: finish.warnings,
        trace_id,
    })
}

/// Trace a preview the service refused before parsing the document.
///
/// The window limit is a service rule, so the refusal happens before the
/// engine sees the document; without this the client would receive
/// `E_TOO_LARGE` and leave nothing behind to look at.
pub fn trace_preview_refusal(
    input: &[u8],
    sink: &dyn Sink,
    origin: Option<&str>,
    code: &str,
    detail: &str,
) {
    let tracer = Tracer::new(sink, TraceId::new());
    let started = Instant::now();
    emit_recv(&tracer, input, origin);
    tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .failed(code)
        .detail(detail)
        .emit();
}

fn emit_recv(tracer: &Tracer<'_>, input: &[u8], origin: Option<&str>) {
    let mut event = tracer
        .event(Stage::Recv)
        .field(FieldKey::Bytes, Scalar::Int(bytes_as_i64(input.len())))
        .field(FieldKey::DocumentSha256, Scalar::string(sha256_hex(input)));
    if let Some(origin) = origin {
        event = event.origin(origin);
    }
    event.emit();
}

/// Reject a duplex request until a backend reports verified duplex support.
///
/// Both shipped platforms report `duplex=false`; silently submitting simplex
/// would violate the document's explicit `output.printer.duplex` field.
pub(crate) fn ensure_duplex_supported(
    backend: &dyn PrinterBackend,
    duplex: bool,
    tracer: &Tracer<'_>,
    started: Instant,
) -> Result<(), EngineError> {
    if !duplex || backend.duplex_supported() {
        return Ok(());
    }
    let pointer = "/output/printer/duplex";
    let message = format!(
        "duplex printing is not supported by this build; {pointer} requires a verified duplex printer"
    );
    let failure = EngineError::Unsupported {
        message: message.clone(),
        pointer: Some(pointer.to_owned()),
    };
    report_failure(
        tracer,
        FailureReport {
            stage: Some(Stage::Validate),
            stage_dur: None,
            stage_fields: &[],
            queue_pos: None,
            result_dur: Some(started.elapsed()),
            code: failure.code(),
            detail: format!("{pointer}: {message}"),
        },
    );
    Err(failure)
}

/// A document after validation and layout, split between what the renderer
/// consumes and what the closing stages need. The split lets the pool own the
/// HTML without cloning it.
struct PreparedDocument {
    trace_id: TraceId,
    started: Instant,
    html: String,
    spec: RenderSpec,
    finish: FinishState,
    /// Kept for the second pass: measured row heights invalidate the first
    /// HTML, so the renderer must serialise the same document again.
    document: Validated,
    fetched: Fetched,
    /// Whether any page carries a table; documents without one skip the
    /// measurement WebView load entirely.
    tables: bool,
    /// Whether the document can be assembled without opening a WebView.
    pdf_only: bool,
}

struct FinishState {
    embedded: Vec<EmbeddedPage>,
    pages: u32,
    /// One size per paper page, used when no renderer is needed.
    page_sizes: Vec<(f64, f64)>,
    html_ms: Duration,
    fetch_ms: Option<Duration>,
    warnings: Vec<Warning>,
    template: Option<String>,
    tags: Vec<String>,
    copies: u32,
    collate: bool,
    duplex: bool,
    printer: Option<String>,
    title: String,
    paper_mm: Option<(f64, f64)>,
}

/// One serialisation pass: HTML, paper geometry and findings.
struct Layout {
    html: String,
    spec: RenderSpec,
    embedded: Vec<EmbeddedPage>,
    pages: u32,
    warnings: Vec<printkit_render::RenderWarning>,
    html_ms: Duration,
}

/// Serialise one document with optional measured table row heights.
fn layout_document(
    document: &Validated,
    fetched: &Fetched,
    heights: Option<&BTreeMap<String, f64>>,
) -> Result<Layout, EngineError> {
    let started = Instant::now();
    let rendered = printkit_render::render_document_with_heights(document, fetched, heights)?;
    let html_ms = started.elapsed();
    let printkit_render::Rendered {
        html,
        pages: rendered_pages,
        page_starts,
        page_sizes,
        warnings,
    } = rendered;
    let (width_mm, height_mm) = document.document().pages[0].spec.size_mm();
    let spec = RenderSpec::new(width_mm, height_mm)
        .with_pages(rendered_pages)
        .with_page_sizes(page_sizes);
    let embedded = embedded_pages(document, &page_starts)?;
    Ok(Layout {
        html,
        spec,
        embedded,
        pages: u32::try_from(rendered_pages).unwrap_or(u32::MAX),
        warnings,
        html_ms,
    })
}

/// A page mixing PDF overlays with HTML items cannot preserve declaration
/// order: the PDF Form XObjects are appended after the renderer's content.
fn pdf_overlay_warning(document: &Validated) -> Option<Warning> {
    document.document().pages.iter().find_map(|page| {
        let has_pdf = page
            .items
            .iter()
            .any(|item| matches!(item, Item::Pdf { .. }));
        let has_other = page
            .items
            .iter()
            .any(|item| !matches!(item, Item::Pdf { .. }));
        (has_pdf && has_other).then(|| Warning {
            code: "W_PDF_OVERLAY_TOP".to_owned(),
            message: "pdf items are painted above HTML items on the same page regardless of array order; check the exported PDF for the intended overlay".to_owned(),
        })
    })
}

fn engine_warnings(warnings: Vec<printkit_render::RenderWarning>) -> Vec<Warning> {
    warnings
        .into_iter()
        .map(|warning| Warning {
            code: warning.code.to_owned(),
            message: warning.message,
        })
        .collect()
}

fn has_tables(document: &Validated) -> bool {
    document.document().pages.iter().any(|page| {
        page.items
            .iter()
            .any(|item| matches!(item, Item::Table { .. }))
    })
}

/// Fallback warning for a table rendered with estimated row heights.
fn estimated_heights_warning() -> Warning {
    Warning {
        code: "W_TABLE_HEIGHTS_ESTIMATED".to_owned(),
        message: "table row heights are estimated, so a page break may land on the \
                  wrong row; the renderer did not return row measurements"
            .to_owned(),
    }
}

/// The traced part of an export that runs on the caller's thread.
///
/// Split out so the caller can emit `recv` first and then report a failure of
/// its own (an export path outside `exports/`) on the same trace instead of
/// leaving a chain that starts at `validate`.
fn prepare_document(
    input: &[u8],
    tracer: &Tracer<'_>,
    trace_id: TraceId,
    started: Instant,
    policy: &FetchPolicy,
) -> Result<PreparedDocument, EngineError> {
    let limit = printkit_core::limits::MAX_DOCUMENT_BYTES;
    if input.len() > limit {
        let message = format!("document is {} bytes, limit is {limit}", input.len());
        report_failure(
            tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: None,
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: "E_TOO_LARGE",
                detail: message,
            },
        );
        return Err(EngineError::DocumentTooLarge {
            bytes: input.len(),
            limit,
        });
    }
    let parse_started = Instant::now();
    let document = match printkit_core::parse(input) {
        Ok(document) => document,
        Err(error) => {
            let failure = DocumentError::Structure(error);
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: Some(parse_started.elapsed()),
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: document_detail(&failure),
                },
            );
            return Err(EngineError::Document(failure));
        }
    };

    let validate_started = Instant::now();
    let document: Validated = match document.validated() {
        Ok(document) => document,
        Err(violations) => {
            let failure = DocumentError::Semantic(violations);
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Validate),
                    stage_dur: Some(validate_started.elapsed()),
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: failure.code(),
                    detail: document_detail(&failure),
                },
            );
            return Err(EngineError::Document(failure));
        }
    };
    let violations = embedded_pdf_violations(document.document());
    if !violations.is_empty() {
        let failure = DocumentError::Semantic(violations);
        report_failure(
            tracer,
            FailureReport {
                stage: Some(Stage::Validate),
                stage_dur: Some(validate_started.elapsed()),
                stage_fields: &[],
                queue_pos: None,
                result_dur: Some(started.elapsed()),
                code: failure.code(),
                detail: document_detail(&failure),
            },
        );
        return Err(EngineError::Document(failure));
    }
    let document_pages = page_count(&document);
    tracer
        .event(Stage::Validate)
        .dur_ms(validate_started.elapsed())
        .pages(document_pages)
        .emit();

    let fetch_started = Instant::now();
    let fetched = match fetch_html(&document, policy) {
        Ok(fetched) => fetched,
        Err(error) => {
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Render),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: error.code(),
                    detail: error.to_string(),
                },
            );
            return Err(EngineError::Fetch(error));
        }
    };
    let fetch_ms = (!fetched.is_empty()).then(|| fetch_started.elapsed());

    let layout = match layout_document(&document, &fetched, None) {
        Ok(layout) => layout,
        Err(error) => {
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Render),
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
    };
    let Layout {
        html,
        spec,
        embedded,
        pages,
        warnings,
        html_ms,
    } = layout;

    let source = document.document();
    let template = source.meta.template_id.clone();
    let tags = source.meta.tags.clone().unwrap_or_default();
    let copies = source.output.copies;
    let collate = source.output.collate;
    let duplex = source
        .output
        .printer
        .as_ref()
        .and_then(|printer| printer.duplex)
        .is_some_and(|duplex| matches!(duplex, Duplex::DuplexLong | Duplex::DuplexShort));
    let printer = source
        .output
        .printer
        .as_ref()
        .and_then(|printer| printer.name.clone());
    let title = source
        .meta
        .title
        .clone()
        .or_else(|| template.clone())
        .unwrap_or_else(|| "printkit".to_owned());
    // A uniform document can carry a driver media hint. Mixed paper must not:
    // the PDF itself describes each page's size.
    let pdf_only = !document.document().pages.is_empty()
        && document.document().pages.iter().all(|page| {
            !page.items.is_empty()
                && page
                    .items
                    .iter()
                    .all(|item| matches!(item, Item::Pdf { .. }))
        });
    let page_sizes = spec.page_sizes.clone();
    let paper_mm = if uniform_paper(&spec) {
        source.pages.first().map(|page| page.spec.size_mm())
    } else {
        None
    };
    let tables = has_tables(&document);

    Ok(PreparedDocument {
        trace_id,
        started,
        html,
        spec,
        finish: FinishState {
            embedded,
            // Paper pages can exceed document pages when tables continue.
            pages,
            page_sizes,
            html_ms,
            fetch_ms,
            warnings: {
                let mut warnings = engine_warnings(warnings);
                if let Some(warning) = pdf_overlay_warning(&document) {
                    warnings.push(warning);
                }
                warnings
            },
            template,
            tags,
            copies,
            collate,
            duplex,
            printer,
            title,
            paper_mm,
        },
        document,
        fetched,
        tables,
        pdf_only,
    })
}
/// Replace estimated table heights with the renderer's measurements.
///
/// One first pass lays every table out; the host then reports each `<tr>`'s
/// real height, and a second pass re-paginates with those numbers. Without
/// measurements the document keeps the estimate and gains a warning instead of
/// silently splitting rows at the wrong place.
async fn refine_table_heights(
    mut prepared: PreparedDocument,
    host: &dyn RenderHost,
    tracer: &Tracer<'_>,
) -> Result<PreparedDocument, EngineError> {
    if !prepared.tables {
        return Ok(prepared);
    }
    let heights = match host.measure_rows(&prepared.html, &prepared.spec).await {
        Ok(heights) => heights,
        Err(_) => {
            prepared.finish.warnings.push(estimated_heights_warning());
            return Ok(prepared);
        }
    };
    if heights.is_empty() {
        prepared.finish.warnings.push(estimated_heights_warning());
        return Ok(prepared);
    }
    let layout = match layout_document(&prepared.document, &prepared.fetched, Some(&heights)) {
        Ok(layout) => layout,
        Err(error) => {
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Render),
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(prepared.started.elapsed()),
                    code: error.code(),
                    detail: error.to_string(),
                },
            );
            return Err(error);
        }
    };
    let Layout {
        html,
        spec,
        embedded,
        pages,
        warnings,
        html_ms,
    } = layout;
    prepared.html = html;
    prepared.spec = spec;
    prepared.finish.embedded = embedded;
    prepared.finish.pages = pages;
    prepared.finish.html_ms += html_ms;
    prepared.finish.warnings = engine_warnings(warnings);
    Ok(prepared)
}

/// Direct renderer path, used by the one-shot CLI.
async fn run_export(
    input: &[u8],
    host: &dyn RenderHost,
    tracer: &Tracer<'_>,
    trace_id: TraceId,
    started: Instant,
    policy: &FetchPolicy,
) -> Result<ExportResult, EngineError> {
    let prepared = prepare_document(input, tracer, trace_id, started, policy)?;
    let prepared = refine_table_heights(prepared, host, tracer).await?;
    let PreparedDocument {
        trace_id,
        started,
        html,
        spec,
        mut finish,
        ..
    } = prepared;
    let render_started = Instant::now();
    let pdf_bytes = match host.render_to_pdf(&html, &spec).await {
        Ok(rendered) => rendered,
        Err(error) => {
            report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Render),
                    stage_dur: Some(render_started.elapsed()),
                    stage_fields: &[(FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms)))],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: platform_code(&error),
                    detail: error.to_string(),
                },
            );
            return Err(EngineError::Platform(error));
        }
    };
    // The host has finished: `pdf_ms` covers producing the renderer's PDF and
    // stops before anything is merged into it.
    let pdf_ms = render_started.elapsed();
    // Report the pages the renderer actually produced; a table may continue
    // beyond the pre-render plan.
    if let Ok(actual) = printkit_pdf::page_count(&pdf_bytes) {
        finish.pages = actual;
    }
    let pdf =
        finish_pdf(&finish, pdf_bytes, pdf_ms, started, tracer).map_err(|failure| failure.error)?;

    tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(finish.pages)
        .emit();

    Ok(ExportResult {
        trace_id,
        pdf,
        html,
        pages: finish.pages,
        warnings: finish.warnings,
    })
}

/// Merge embedded pages and emit the successful `render` event.
fn finish_pdf(
    finish: &FinishState,
    pdf_bytes: Bytes,
    pdf_ms: Duration,
    started: Instant,
    tracer: &Tracer<'_>,
) -> Result<Bytes, TracedError> {
    let merge_started = Instant::now();
    let pdf = match printkit_pdf::assemble_embedded_with_title(
        pdf_bytes,
        &finish.embedded,
        Some(finish.title.as_str()),
    ) {
        Ok(pdf) => pdf,
        Err(error) => {
            let mut fields = vec![
                (FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms))),
                (FieldKey::PdfMs, Scalar::Int(duration_ms(pdf_ms))),
            ];
            if !finish.embedded.is_empty() {
                fields.push((
                    FieldKey::MergeMs,
                    Scalar::Int(duration_ms(merge_started.elapsed())),
                ));
            }
            let result_seq = report_failure(
                tracer,
                FailureReport {
                    stage: Some(Stage::Render),
                    stage_dur: Some(finish.html_ms + pdf_ms),
                    stage_fields: &fields,
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: pdf_code(&error),
                    detail: error.to_string(),
                },
            );
            return Err(TracedError {
                error: EngineError::Pdf(error),
                result_seq,
            });
        }
    };
    let merge_ms = merge_started.elapsed();

    emit_render_event(
        finish,
        pdf_ms,
        (!finish.embedded.is_empty()).then_some(merge_ms),
        tracer,
    );
    Ok(pdf)
}

/// Emit the successful `render` event shared by the PDF and HTML print paths.
fn emit_render_event(
    finish: &FinishState,
    pdf_ms: Duration,
    merge_ms: Option<Duration>,
    tracer: &Tracer<'_>,
) {
    let mut render_event = tracer
        .event(Stage::Render)
        .dur_ms(finish.html_ms + pdf_ms + merge_ms.unwrap_or_default())
        .pages(finish.pages)
        .field(FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms)))
        .field(FieldKey::PdfMs, Scalar::Int(duration_ms(pdf_ms)));
    if let Some(fetch_ms) = finish.fetch_ms {
        render_event = render_event.field(FieldKey::FetchMs, Scalar::Int(duration_ms(fetch_ms)));
    }
    if let Some(merge_ms) = merge_ms {
        render_event = render_event.field(FieldKey::MergeMs, Scalar::Int(duration_ms(merge_ms)));
    }
    render_event.emit();
}
/// Where statistics rows go. The engine writes them through one queue instead
/// of the trace sink: the two files have different retention and different
/// readers, and `store` must stay independent of `trace`.
#[derive(Clone)]
pub struct UsageRecorder {
    inner: Arc<UsageInner>,
    origin: String,
}

/// One owned row crossing the writer queue.
#[derive(Debug)]
struct OwnedUsage {
    trace_id: String,
    seq: u64,
    at: String,
    origin: String,
    template: Option<String>,
    tags: Vec<String>,
    printer: Option<String>,
    pages: u32,
    copies: u32,
    status: String,
    dur_ms: u64,
    error_code: Option<String>,
}

/// What happened when the writer queue was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainResult {
    /// The writer joined before the deadline and wrote every queued row.
    pub drained: bool,
    /// The writer was still running after the 100 ms deadline.
    pub timed_out: bool,
    /// Rows offered after the writer was closed.
    pub dropped_after_close: u64,
}

struct UsageInner {
    store: Arc<Store>,
    retention_days: u16,
    max_rows: u32,
    sender: Mutex<Option<SyncSender<OwnedUsage>>>,
    closed: AtomicBool,
    dropped_after_close: AtomicU64,
    dropped_full: AtomicU64,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl UsageRecorder {
    /// Open the statistics database for a one-shot CLI submission.
    ///
    /// The CLI has no pool to own the store, so it opens one here. A failure
    /// is reported to the caller as a string: statistics are a side channel
    /// and must not keep a print from being submitted.
    pub fn open(
        data_dir: &Path,
        origin: String,
        retention_days: u16,
        max_rows: u32,
    ) -> Result<Self, String> {
        let store = Store::open(data_dir).map_err(|error| error.to_string())?;
        Self::try_from_store(Arc::new(store), origin, retention_days, max_rows)
    }

    /// Wrap an already-open store with the single background writer.
    pub fn from_store(
        store: Arc<Store>,
        origin: String,
        retention_days: u16,
        max_rows: u32,
    ) -> Self {
        Self::try_from_store(store, origin, retention_days, max_rows)
            .expect("spawning the usage writer")
    }

    /// Fallible form used before a plugin-state commit: a thread failure must
    /// leave the old writer and file untouched.
    pub fn try_from_store(
        store: Arc<Store>,
        origin: String,
        retention_days: u16,
        max_rows: u32,
    ) -> Result<Self, String> {
        let (sender, receiver) = sync_channel::<OwnedUsage>(1024);
        let inner = Arc::new(UsageInner {
            store,
            retention_days,
            max_rows,
            sender: Mutex::new(Some(sender)),
            closed: AtomicBool::new(false),
            dropped_after_close: AtomicU64::new(0),
            dropped_full: AtomicU64::new(0),
            writer: Mutex::new(None),
        });
        let store = Arc::clone(&inner.store);
        let retention_days = inner.retention_days;
        let max_rows = inner.max_rows;
        let handle = thread::Builder::new()
            .name("printkit-usage".to_owned())
            .spawn(move || {
                while let Ok(row) = receiver.recv() {
                    if let Err(error) = write_usage_row(&store, retention_days, max_rows, &row) {
                        eprintln!("printkit: cannot write a print record: {error}");
                    }
                }
            })
            .map_err(|error| format!("cannot spawn the usage writer: {error}"))?;
        *inner
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(handle);
        Ok(Self { inner, origin })
    }

    /// A clone that records the same queue under another origin.
    pub fn with_origin(&self, origin: String) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            origin,
        }
    }

    /// Origin used by the trace events of this clone.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The store this writer owns.
    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.inner.store)
    }

    /// Enqueue one row without blocking. Never fails a print.
    pub fn record(&self, record: &NewPrintRecord<'_>) {
        if self.inner.closed.load(Ordering::SeqCst) {
            self.inner
                .dropped_after_close
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let owned = OwnedUsage {
            trace_id: record.trace_id.to_owned(),
            seq: record.seq,
            at: record.at.to_owned(),
            origin: record.origin.to_owned(),
            template: record.template.map(str::to_owned),
            tags: record.tags.to_vec(),
            printer: record.printer.map(str::to_owned),
            pages: record.pages,
            copies: record.copies,
            status: record.status.to_owned(),
            dur_ms: record.dur_ms,
            error_code: record.error_code.map(str::to_owned),
        };
        let guard = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(sender) = guard.as_ref() else {
            self.inner
                .dropped_after_close
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        match sender.try_send(owned) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.inner.dropped_full.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner
                    .dropped_after_close
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Stop accepting rows and wait up to 100 ms for the writer to drain.
    pub fn close_and_drain(&self) -> DrainResult {
        self.inner.close_and_drain()
    }

    /// Rows lost after the queue closed.
    pub fn dropped_after_close(&self) -> u64 {
        self.inner.dropped_after_close.load(Ordering::Relaxed)
    }

    /// Rows lost because the queue was full or the writer died.
    pub fn dropped_full(&self) -> u64 {
        self.inner.dropped_full.load(Ordering::Relaxed)
    }
}

fn write_usage_row(
    store: &Store,
    retention_days: u16,
    max_rows: u32,
    row: &OwnedUsage,
) -> Result<(), String> {
    let record = NewPrintRecord {
        trace_id: &row.trace_id,
        seq: row.seq,
        at: &row.at,
        origin: &row.origin,
        template: row.template.as_deref(),
        tags: &row.tags,
        printer: row.printer.as_deref(),
        pages: row.pages,
        copies: row.copies,
        status: &row.status,
        dur_ms: row.dur_ms,
        error_code: row.error_code.as_deref(),
    };
    store
        .insert_print_record(&record)
        .map_err(|error| error.to_string())?;
    store
        .cleanup_print_records(retention_days, max_rows)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

impl UsageInner {
    fn close_and_drain(&self) -> DrainResult {
        let was_open = !self.closed.swap(true, Ordering::SeqCst);
        if was_open {
            let sender = self
                .sender
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take();
            drop(sender);
        }
        let handle = self
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        let Some(handle) = handle else {
            return DrainResult {
                drained: true,
                timed_out: false,
                dropped_after_close: self.dropped_after_close(),
            };
        };
        let (done, wait) = channel();
        thread::spawn(move || {
            let _ = handle.join();
            let _ = done.send(());
        });
        let drained = wait.recv_timeout(Duration::from_millis(100)).is_ok();
        DrainResult {
            drained,
            timed_out: !drained,
            dropped_after_close: self.dropped_after_close(),
        }
    }

    fn dropped_after_close(&self) -> u64 {
        self.dropped_after_close.load(Ordering::Relaxed)
    }
}

impl Drop for UsageInner {
    fn drop(&mut self) {
        self.close_and_drain();
    }
}

impl std::fmt::Debug for UsageRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsageRecorder")
            .field("origin", &self.origin)
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .finish()
    }
}

/// A reserved `doc.print` job. The caller answers the client with the job id
/// and only then calls [`Reservation::activate`], so the response always
/// precedes the job's `queued` event.
pub struct ReservedPrint {
    /// The id the client is answered with; the `queued` event follows the
    /// response.
    pub job_id: JobId,
    /// Trace opened at validation; the job continues it through `queue`,
    /// `render` and `result`.
    pub trace_id: TraceId,
    /// Pages in the submitted document, continuation pages included.
    pub pages: u32,
    /// The held pool slot. Dropping it without activating releases the slot
    /// and forgets the job.
    pub reservation: Reservation,
}

/// Parse, validate and queue one document as a print job.
///
/// Rendering happens in the pool; the caller never blocks on the renderer.
#[allow(clippy::too_many_arguments)]
pub fn reserve_print(
    input: &[u8],
    pool: &RenderPool,
    host: &dyn RenderHost,
    backend: Arc<dyn PrinterBackend>,
    jobs: &PrintJobs,
    sink: Arc<dyn Sink>,
    policy: &FetchPolicy,
    notify: JobUpdateHook,
    usage: Option<UsageRecorder>,
) -> Result<ReservedPrint, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
    let origin = usage.as_ref().map(|usage| usage.origin());
    emit_recv(&tracer, input, origin);
    // Authorisation of this request is a stage of the same timeline as the
    // render; the handshake event belongs to the connection, not to the job.
    let mut auth = tracer.event(Stage::Auth).dur_ms(started.elapsed());
    if let Some(origin) = origin {
        auth = auth.origin(origin);
    }
    auth.emit();
    let prepared = prepare_document(input, &tracer, trace_id.clone(), started, policy)?;
    ensure_duplex_supported(backend.as_ref(), prepared.finish.duplex, &tracer, started)?;
    let prepared = futures::executor::block_on(refine_table_heights(prepared, host, &tracer))?;
    let PreparedDocument {
        trace_id,
        started,
        html,
        spec,
        finish,
        pdf_only: pdf_only_for_pool,
        ..
    } = prepared;
    let update = job_update(trace_id.clone(), Arc::clone(&sink), notify);
    let control = Arc::new(PrintControl::new());
    // Mixed paper and embedded PDF pages cannot go through one HTML print
    // operation. When the backend can print PDFs, render the PDF instead;
    // otherwise the HTML path stays and refuses those cases explicitly.
    let mixed = !uniform_paper(&spec);
    let has_embedded = !finish.embedded.is_empty();
    let pdf_fallback = (mixed || has_embedded) && backend.prints_html() && backend.prints_pdf();
    let render_pdf = !backend.prints_html() || pdf_fallback;
    let finished = print_finished(
        trace_id.clone(),
        started,
        finish,
        Arc::clone(&sink),
        backend,
        Arc::clone(&control),
        usage,
        pdf_fallback,
        pdf_only_for_pool,
    );
    let pages = spec.pages as u32;
    let reservation = pool
        .reserve(PreparedRender {
            priority: JobPriority::Print,
            html,
            spec,
            render_pdf,
            pdf_only: pdf_only_for_pool,
            hooks: JobHooks { update, finished },
        })
        .map_err(|error| report_reserve_failure(&tracer, started, error))?;
    jobs.insert(reservation.job_id().clone(), control);
    Ok(ReservedPrint {
        pages,
        job_id: reservation.job_id().clone(),
        trace_id,
        reservation,
    })
}

#[allow(clippy::too_many_arguments)]
fn print_finished(
    trace_id: TraceId,
    started: Instant,
    finish: FinishState,
    sink: Arc<dyn Sink>,
    backend: Arc<dyn PrinterBackend>,
    control: Arc<PrintControl>,
    usage: Option<UsageRecorder>,
    pdf_fallback: bool,
    pdf_only: bool,
) -> JobFinishHook {
    // Decided once: a backend either accepts HTML or the PDF path. Mixed paper
    // with a PDF-capable backend is forced onto the PDF path even when HTML
    // printing is available.
    let use_html = backend.prints_html() && !pdf_fallback;
    Box::new(move |report| {
        let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
        // A queued cancellation calls this hook without renderer bytes.
        if control.cancelled() {
            return cancelled_outcome(
                &tracer,
                &trace_id,
                started,
                &finish,
                usage.as_ref(),
                finish.printer.as_deref(),
            );
        }
        let submitted = match report.result {
            Ok(bytes) => {
                if use_html {
                    match report.prepared {
                        Some((html, spec))
                            if finish.embedded.is_empty() && uniform_paper(&spec) =>
                        {
                            emit_render_event(&finish, report.rendered, None, &tracer);
                            if control.cancelled() {
                                return cancelled_outcome(
                                    &tracer,
                                    &trace_id,
                                    started,
                                    &finish,
                                    usage.as_ref(),
                                    finish.printer.as_deref(),
                                );
                            }
                            print::submit_html(
                                backend.as_ref(),
                                HtmlPrintJob {
                                    printer: finish.printer.clone(),
                                    copies: finish.copies,
                                    collate: finish.collate,
                                    title: finish.title.clone(),
                                    paper_mm: finish.paper_mm,
                                    duplex: finish.duplex,
                                    html,
                                    spec,
                                },
                                &tracer,
                                started,
                            )
                        }
                        Some((_, spec)) => {
                            let detail = if !finish.embedded.is_empty() {
                                "embedded PDF pages cannot be printed through the platform's HTML print path"
                            } else {
                                let _ = spec;
                                "mixed paper sizes cannot be printed through one platform print operation yet"
                            };
                            Err(reject_html_print(
                                &finish,
                                report.rendered,
                                &tracer,
                                started,
                                detail,
                            ))
                        }
                        None => Err(reject_html_print(
                            &finish,
                            report.rendered,
                            &tracer,
                            started,
                            "the renderer did not return the HTML for a direct print",
                        )),
                    }
                } else {
                    let bytes = if pdf_only {
                        match printkit_pdf::blank_pages(&finish.page_sizes) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                return JobOutcome::Failed(job_failure(&EngineError::Pdf(error)));
                            }
                        }
                    } else {
                        bytes
                    };
                    match finish_pdf(&finish, bytes, report.rendered, started, &tracer) {
                        Ok(pdf) => {
                            if control.cancelled() {
                                return cancelled_outcome(
                                    &tracer,
                                    &trace_id,
                                    started,
                                    &finish,
                                    usage.as_ref(),
                                    finish.printer.as_deref(),
                                );
                            }
                            print::submit_pdf(
                                backend.as_ref(),
                                PrintJob {
                                    printer: finish.printer.clone(),
                                    pdf,
                                    copies: finish.copies,
                                    collate: finish.collate,
                                    title: finish.title.clone(),
                                    paper_mm: finish.paper_mm,
                                    duplex: finish.duplex,
                                },
                                &tracer,
                                started,
                            )
                        }
                        Err(failure) => Err(failure),
                    }
                }
            }
            Err(failure) => {
                let seq = report_failure(
                    &tracer,
                    FailureReport::from_render(started, report.timing, &failure),
                );
                write_usage(
                    usage.as_ref(),
                    &trace_id,
                    seq,
                    started,
                    usage_row(&finish, finish.printer.as_deref()),
                    "failed",
                    Some(failure.code()),
                );
                return JobOutcome::Failed(job_failure_from_render(&failure));
            }
        };

        match submitted {
            Ok(submitted) => {
                control.record_submitted(submitted.clone());
                if control.cancelled() {
                    let _ = backend.cancel_submitted(&submitted);
                    return cancelled_outcome(
                        &tracer,
                        &trace_id,
                        started,
                        &finish,
                        usage.as_ref(),
                        Some(submitted.printer.as_str()),
                    );
                }
                let seq = tracer
                    .event(Stage::Result)
                    .dur_ms(started.elapsed())
                    .pages(finish.pages)
                    .emit();
                write_usage(
                    usage.as_ref(),
                    &trace_id,
                    seq,
                    started,
                    usage_row(&finish, Some(submitted.printer.as_str())),
                    "done",
                    None,
                );
                JobOutcome::Done {
                    warnings: finish.warnings.clone(),
                }
            }
            Err(failure) => {
                write_usage(
                    usage.as_ref(),
                    &trace_id,
                    failure.result_seq,
                    started,
                    usage_row(&finish, finish.printer.as_deref()),
                    "failed",
                    Some(failure.error.code()),
                );
                JobOutcome::Failed(job_failure(&failure.error))
            }
        }
    })
}

/// Close a cancelled job's trace and usage row.
fn cancelled_outcome(
    tracer: &Tracer<'_>,
    trace_id: &TraceId,
    started: Instant,
    finish: &FinishState,
    usage: Option<&UsageRecorder>,
    printer: Option<&str>,
) -> JobOutcome {
    let seq = tracer
        .event(Stage::Result)
        .dur_ms(started.elapsed())
        .pages(finish.pages)
        .detail("cancelled")
        .emit();
    write_usage(
        usage,
        trace_id,
        seq,
        started,
        usage_row(finish, printer),
        "cancelled",
        None,
    );
    JobOutcome::Cancelled
}

/// Whether every page uses the document's own paper size. `PrintToPdfStream`
/// and WebView2 `Print` accept one media size per operation.
fn uniform_paper(spec: &RenderSpec) -> bool {
    spec.page_sizes.iter().all(|(width, height)| {
        (width - spec.width_mm).abs() <= 0.01 && (height - spec.height_mm).abs() <= 0.01
    })
}

/// Report why a direct HTML print cannot run, with the render stages it spent.
fn reject_html_print(
    finish: &FinishState,
    pdf_ms: Duration,
    tracer: &Tracer<'_>,
    started: Instant,
    detail: &str,
) -> TracedError {
    let failure = EngineError::Platform(PlatformError::Unsupported(detail.to_owned()));
    let fields = [
        (FieldKey::HtmlMs, Scalar::Int(duration_ms(finish.html_ms))),
        (FieldKey::PdfMs, Scalar::Int(duration_ms(pdf_ms))),
    ];
    let result_seq = report_failure(
        tracer,
        FailureReport {
            stage: Some(Stage::Render),
            stage_dur: Some(finish.html_ms + pdf_ms),
            stage_fields: &fields,
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

fn usage_row<'a>(finish: &'a FinishState, printer: Option<&'a str>) -> UsageRow<'a> {
    UsageRow {
        pages: finish.pages,
        copies: finish.copies,
        printer,
        template: finish.template.as_deref(),
        tags: &finish.tags,
    }
}

struct UsageRow<'a> {
    pages: u32,
    copies: u32,
    printer: Option<&'a str>,
    template: Option<&'a str>,
    tags: &'a [String],
}

/// Write one usage row and trim the table. A failure here must never fail a
/// print: statistics are a side channel, not part of the job result.
fn write_usage(
    usage: Option<&UsageRecorder>,
    trace_id: &TraceId,
    seq: u64,
    started: Instant,
    row: UsageRow<'_>,
    status: &str,
    error_code: Option<&str>,
) {
    let Some(usage) = usage else {
        return;
    };
    let at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let record = NewPrintRecord {
        trace_id: trace_id.as_str(),
        seq,
        at: &at,
        origin: usage.origin(),
        template: row.template,
        tags: row.tags,
        printer: row.printer,
        pages: row.pages,
        copies: row.copies,
        status,
        dur_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        error_code,
    };
    usage.record(&record);
}

/// Render a document through the pool and write the PDF under
/// `<data_dir>/exports`.
///
/// The path is validated before rendering: an escape attempt must not pay for
/// a WebView job. The returned path is data-directory-relative and always
/// carries the `exports/` prefix.
pub fn export_pdf_to_path(
    input: &[u8],
    data_dir: &Path,
    path: &str,
    pool: &RenderPool,
    host: &dyn RenderHost,
    sink: Arc<dyn Sink>,
    policy: &FetchPolicy,
) -> Result<ExportedFile, EngineError> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let tracer = Tracer::new(sink.as_ref(), trace_id.clone());
    emit_recv(&tracer, input, None);
    let target = match export_path::resolve(data_dir, path) {
        Ok(target) => target,
        Err(error) => {
            report_failure(
                &tracer,
                FailureReport {
                    stage: None,
                    stage_dur: None,
                    stage_fields: &[],
                    queue_pos: None,
                    result_dur: Some(started.elapsed()),
                    code: error.code(),
                    detail: error.to_string(),
                },
            );
            return Err(EngineError::Path(error));
        }
    };
    let prepared = prepare_document(input, &tracer, trace_id.clone(), started, policy)?;
    let prepared = futures::executor::block_on(refine_table_heights(prepared, host, &tracer))?;
    let PreparedDocument {
        trace_id: prepared_trace,
        started: prepared_started,
        html,
        spec,
        finish,
        pdf_only,
        ..
    } = prepared;
    let pages = finish.pages;
    let warnings = finish.warnings.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let finish_sink = Arc::clone(&sink);
    let absolute_path = target.to_string_lossy().into_owned();
    let finished = Box::new(move |report: RenderReport| -> JobOutcome {
        let tracer = Tracer::new(finish_sink.as_ref(), prepared_trace);
        let outcome = match report.result {
            Ok(bytes) => {
                let bytes = if pdf_only {
                    match printkit_pdf::blank_pages(&finish.page_sizes) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            return JobOutcome::Failed(job_failure(&EngineError::Pdf(error)));
                        }
                    }
                } else {
                    bytes
                };
                match finish_pdf(&finish, bytes, report.rendered, prepared_started, &tracer) {
                    Ok(pdf) => match export_path::write_atomic(&target, &pdf) {
                        Ok(()) => {
                            tracer
                                .event(Stage::Result)
                                .dur_ms(prepared_started.elapsed())
                                .pages(finish.pages)
                                .emit();
                            Ok(u64::try_from(pdf.len()).unwrap_or(u64::MAX))
                        }
                        Err(source) => {
                            let error = ExportPathError::Io {
                                path: target.display().to_string(),
                                source,
                            };
                            report_failure(
                                &tracer,
                                FailureReport {
                                    stage: None,
                                    stage_dur: None,
                                    stage_fields: &[],
                                    queue_pos: None,
                                    result_dur: Some(prepared_started.elapsed()),
                                    code: error.code(),
                                    detail: error.to_string(),
                                },
                            );
                            Err(job_failure(&EngineError::Path(error)))
                        }
                    },
                    Err(failure) => Err(job_failure(&failure.error)),
                }
            }
            Err(failure) => {
                report_failure(
                    &tracer,
                    FailureReport::from_render(prepared_started, report.timing, &failure),
                );
                Err(job_failure_from_render(&failure))
            }
        };
        let _ = sender.send(outcome.clone());
        match outcome {
            Ok(_) => JobOutcome::Done {
                warnings: finish.warnings.clone(),
            },
            Err(failure) => JobOutcome::Failed(failure),
        }
    });
    let update = job_update(trace_id.clone(), Arc::clone(&sink), Box::new(|_, _| {}));
    let reservation = pool
        .reserve(PreparedRender {
            priority: JobPriority::Export,
            html,
            spec,
            render_pdf: true,
            pdf_only,
            hooks: JobHooks { update, finished },
        })
        .map_err(|error| report_reserve_failure(&tracer, started, error))?;
    reservation.activate();
    let bytes = receiver.recv().map_err(|_| {
        EngineError::Job(JobFailure::new(
            "E_JOB_FAILED",
            "the render pool dropped the export job",
        ))
    })??;
    Ok(ExportedFile {
        trace_id,
        path: format!("exports/{path}"),
        absolute_path,
        pages,
        bytes,
        warnings,
    })
}

/// Trace the queue stage when the job becomes schedulable and forward the
/// protocol-facing update.
fn job_update(trace_id: TraceId, sink: Arc<dyn Sink>, notify: JobUpdateHook) -> JobUpdateHook {
    Box::new(move |job_id, update| {
        if let JobUpdate::Queued { queue_pos } = update {
            Tracer::new(sink.as_ref(), trace_id.clone())
                .event(Stage::Queue)
                .queue_pos(queue_pos)
                .emit();
        }
        notify(job_id, update);
    })
}

/// A terminal failure plus the sequence number of its `result` event, so the
/// usage row can point at the log line that explains it.
struct TracedError {
    error: EngineError,
    result_seq: u64,
}

fn job_failure(error: &EngineError) -> JobFailure {
    JobFailure {
        code: error.code(),
        message: error.to_string(),
        detail: error.detail(),
    }
}

fn job_failure_from_render(failure: &RenderFailure) -> JobFailure {
    JobFailure {
        code: failure.code(),
        message: failure.message(),
        detail: None,
    }
}

/// Report a failure as the failed stage plus the closing `result` event.
///
/// One helper keeps every failure chain in the same order: a stage event when
/// the failure belongs to a stage, then `result`. Measurements are optional
/// because a stage that fails before measuring has none, and a failure outside
/// the pipeline (a rejected export path) starts at `result`.
struct FailureReport<'a> {
    /// The stage that failed; `None` when the failure is outside the pipeline.
    stage: Option<Stage>,
    stage_dur: Option<Duration>,
    stage_fields: &'a [(FieldKey, Scalar)],
    /// Queue position reported by a refusal or a queue timeout.
    queue_pos: Option<u32>,
    result_dur: Option<Duration>,
    code: &'a str,
    detail: String,
}

impl<'a> FailureReport<'a> {
    /// A failure reported by the pool after the job was reserved: a timeout
    /// belongs to the queue stage, every other cause to the render stage.
    fn from_render(started: Instant, timing: QueueTiming, failure: &'a RenderFailure) -> Self {
        let timed_out = matches!(failure, RenderFailure::QueueTimeout { .. });
        Self {
            stage: Some(if timed_out {
                Stage::Queue
            } else {
                Stage::Render
            }),
            stage_dur: match failure {
                RenderFailure::QueueTimeout { waited } => Some(*waited),
                _ => None,
            },
            stage_fields: &[],
            queue_pos: if timed_out {
                Some(timing.queue_pos)
            } else {
                None
            },
            result_dur: Some(started.elapsed()),
            code: failure.code(),
            detail: failure.message(),
        }
    }
}

fn report_failure(tracer: &Tracer<'_>, report: FailureReport<'_>) -> u64 {
    if let Some(stage) = report.stage {
        let mut event = tracer
            .event(stage)
            .failed(report.code)
            .detail(report.detail.as_str());
        if let Some(duration) = report.stage_dur {
            event = event.dur_ms(duration);
        }
        if let Some(position) = report.queue_pos {
            event = event.queue_pos(position);
        }
        for (key, value) in report.stage_fields {
            event = event.field(*key, value.clone());
        }
        event.emit();
    }
    let mut event = tracer
        .event(Stage::Result)
        .failed(report.code)
        .detail(report.detail);
    if let Some(duration) = report.result_dur {
        event = event.dur_ms(duration);
    }
    event.emit()
}

/// Refuse a reservation on the same trace: a full queue or a stopped pool is
/// a failure of the queue stage, and the chain still ends at `result`.
fn report_reserve_failure(
    tracer: &Tracer<'_>,
    started: Instant,
    refused: ReserveError,
) -> EngineError {
    let error = EngineError::Queue(refused);
    report_failure(
        tracer,
        FailureReport {
            stage: Some(Stage::Queue),
            stage_dur: None,
            stage_fields: &[],
            queue_pos: match &error {
                EngineError::Queue(ReserveError::Full { queue_pos, .. }) => Some(*queue_pos),
                _ => None,
            },
            result_dur: Some(started.elapsed()),
            code: error.code(),
            detail: error.to_string(),
        },
    );
    error
}

/// A schema failure names the JSON Pointer first, so a log line carries the
/// failing field.
fn document_detail(error: &DocumentError) -> String {
    match error.pointer() {
        Some(pointer) => format!("{pointer}: {error}"),
        None => error.to_string(),
    }
}

/// Fetch every unique `data.url` before rendering. One failing URL fails the
/// export: a page with an empty frame would look like a successful print.
fn fetch_html(document: &Validated, policy: &FetchPolicy) -> Result<Fetched, FetchError> {
    let urls: BTreeSet<&str> = document
        .document()
        .pages
        .iter()
        .flat_map(|page| page.items.iter())
        .filter_map(|item| match item {
            Item::Html { data, .. } => data.url.as_deref(),
            _ => None,
        })
        .collect();
    if urls.len() > printkit_core::limits::MAX_FETCH_URLS {
        return Err(FetchError::TooManyUrls {
            count: urls.len(),
            limit: printkit_core::limits::MAX_FETCH_URLS,
        });
    }
    if urls.is_empty() {
        return Ok(Fetched::new());
    }
    fetch_bodies(&urls, policy)
}

/// Fill the renderer's body map from the network.
#[cfg(feature = "url-fetch")]
fn fetch_bodies(urls: &BTreeSet<&str>, policy: &FetchPolicy) -> Result<Fetched, FetchError> {
    let fetcher = Fetcher::new(policy)?;
    // One deadline for the whole document: a page full of URLs must not add
    // one timeout per URL to the job's wall clock.
    let deadline = std::time::Instant::now() + policy.total_timeout();
    let mut fetched = Fetched::new();
    let mut total = 0usize;
    for url in urls {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let body = fetcher.fetch_within(url, remaining)?;
        total = total.saturating_add(body.len());
        if total > fetch::MAX_DOCUMENT_FETCH_BYTES {
            return Err(FetchError::TooLarge {
                url: (*url).to_owned(),
                limit: fetch::MAX_DOCUMENT_FETCH_BYTES as u64,
            });
        }
        fetched.insert(*url, body);
    }
    Ok(fetched)
}

/// Refuse a URL document instead of rendering an empty frame. The binary was
/// built without `url-fetch`, so it has no TLS stack to fetch with.
#[cfg(not(feature = "url-fetch"))]
fn fetch_bodies(urls: &BTreeSet<&str>, _policy: &FetchPolicy) -> Result<Fetched, FetchError> {
    let url = urls
        .iter()
        .next()
        .map(|url| (*url).to_owned())
        .unwrap_or_default();
    Err(FetchError::Unavailable { url })
}

fn duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn bytes_as_i64(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

pub(crate) fn platform_code(error: &PlatformError) -> &'static str {
    match error {
        PlatformError::Timeout(_) => "E_TIMEOUT",
        PlatformError::Unavailable(_) | PlatformError::Unsupported(_) => "E_UNSUPPORTED",
        PlatformError::PrinterNotFound(_) | PlatformError::NoDefaultPrinter => {
            "E_PRINTER_NOT_FOUND"
        }
        PlatformError::Print(_) | PlatformError::Render(_) => "E_JOB_FAILED",
        PlatformError::CancelUnsupported => "E_CANCEL_UNSUPPORTED",
        PlatformError::RawUnsupported(_) => "E_RAW_UNSUPPORTED",
        PlatformError::WindowNotFound(_) | PlatformError::JobNotFound { .. } => "E_NOT_FOUND",
    }
}

fn pdf_code(error: &printkit_pdf::PdfError) -> &'static str {
    match error {
        printkit_pdf::PdfError::Encrypted => "E_PDF_ENCRYPTED",
        printkit_pdf::PdfError::UnsupportedObject { .. }
        | printkit_pdf::PdfError::TooDeep { .. } => "E_UNSUPPORTED",
        printkit_pdf::PdfError::EmbeddedPageOutOfRange { .. }
        | printkit_pdf::PdfError::InvalidFrame => "E_SCHEMA",
        _ => "E_JOB_FAILED",
    }
}

/// Preflight embedded PDFs. The document model stays free of `lopdf`; this
/// runs in the engine, where the graph is already parsed.
fn embedded_pdf_violations(document: &printkit_core::Document) -> Vec<Violation> {
    fn violation(
        code: &'static str,
        pointer: String,
        message: String,
        object: Option<String>,
    ) -> Violation {
        let mut violation = Violation::with_code(code, pointer, message);
        violation.object = object;
        violation
    }

    let mut cache: BTreeMap<String, Result<EmbeddedPdf, EmbeddedProblem>> = BTreeMap::new();
    let mut violations = Vec::new();
    for (page_index, page) in document.pages.iter().enumerate() {
        for (item_index, item) in page.items.iter().enumerate() {
            let Item::Pdf { data, .. } = item else {
                continue;
            };
            let src_pointer = format!("/pages/{page_index}/items/{item_index}/data/src");
            let outcome = {
                let entry = cache.entry(data.src.clone()).or_insert_with(|| {
                    decode_data_uri(&data.src)
                        .ok_or_else(|| {
                            EmbeddedProblem::Malformed(
                                "resource is not a base64 data URI".to_owned(),
                            )
                        })
                        .and_then(|bytes| EmbeddedPdf::parse(&bytes))
                });
                match entry {
                    Ok(pdf) => pdf.check_page(data.page),
                    Err(problem) => Err(problem.clone()),
                }
            };
            let page_pointer = || format!("/pages/{page_index}/items/{item_index}/data/page");
            match outcome {
                Ok(_) => {}
                Err(EmbeddedProblem::Encrypted) => violations.push(violation(
                    printkit_core::E_PDF_ENCRYPTED,
                    src_pointer,
                    "the embedded PDF is encrypted; decrypt it before printing".to_owned(),
                    None,
                )),
                Err(EmbeddedProblem::PageOutOfRange { page, pages }) => violations.push(violation(
                    printkit_core::E_SCHEMA,
                    page_pointer(),
                    format!("page {page} is past the end of the {pages}-page embedded PDF"),
                    None,
                )),
                Err(EmbeddedProblem::NotPdf) => violations.push(violation(
                    printkit_core::E_SCHEMA,
                    src_pointer,
                    "the embedded PDF must start with %PDF-".to_owned(),
                    None,
                )),
                Err(EmbeddedProblem::Unsupported { object, reason }) => {
                    let named = format!("{} {} R", object.0, object.1);
                    violations.push(violation(
                        printkit_core::E_UNSUPPORTED,
                        src_pointer,
                        format!("the embedded PDF object {named}: {reason}"),
                        Some(named),
                    ));
                }
                Err(EmbeddedProblem::TooDeep { limit }) => violations.push(violation(
                    printkit_core::E_UNSUPPORTED,
                    src_pointer,
                    format!("the embedded PDF object graph is deeper than {limit}"),
                    None,
                )),
                Err(EmbeddedProblem::Malformed(message)) => violations.push(violation(
                    printkit_core::E_SCHEMA,
                    src_pointer,
                    format!("the embedded PDF cannot be parsed: {message}"),
                    None,
                )),
            }
        }
    }
    violations
}

/// Embedded pages with their target paper page and frame in PDF points.
///
/// The renderer lays items out in millimetres from the page's top-left corner;
/// PDF user space starts at the bottom-left. The conversion happens here so
/// the PDF crate never needs to know about millimetres.
fn embedded_pages(
    document: &Validated,
    page_starts: &[usize],
) -> Result<Vec<EmbeddedPage>, printkit_pdf::PdfError> {
    let mut decoded: BTreeMap<String, Bytes> = BTreeMap::new();
    let mut pages = Vec::new();
    for (index, page) in document.document().pages.iter().enumerate() {
        let target_page = page_starts
            .get(index)
            .copied()
            .and_then(|page| u32::try_from(page).ok())
            .ok_or(printkit_pdf::PdfError::MissingPage {
                page: u32::try_from(index + 1).unwrap_or(u32::MAX),
            })?;
        let (_, height_mm) = page.spec.size_mm();
        let page_height_pt = height_mm * MM_TO_PT;
        for item in &page.items {
            let Item::Pdf { frame, data, .. } = item else {
                continue;
            };
            let bytes = match decoded.get(data.src.as_str()) {
                Some(bytes) => bytes.clone(),
                None => {
                    let bytes = decode_data_uri(&data.src).ok_or(printkit_pdf::PdfError::NotPdf)?;
                    let bytes = Bytes::from(bytes);
                    decoded.insert(data.src.clone(), bytes.clone());
                    bytes
                }
            };
            let x = frame.x.mm() * MM_TO_PT;
            let y = page_height_pt - (frame.y.mm() + frame.height.mm()) * MM_TO_PT;
            pages.push(EmbeddedPage {
                bytes,
                page: data.page,
                target_page,
                frame: PtRect::new(
                    x,
                    y,
                    frame.width.mm() * MM_TO_PT,
                    frame.height.mm() * MM_TO_PT,
                ),
            });
        }
    }
    Ok(pages)
}

fn page_count(document: &Validated) -> u32 {
    u32::try_from(document.document().pages.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use async_trait::async_trait;
    use base64::Engine as _;
    use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
    use printkit_trace::MemorySink;

    use super::*;

    const MINIMAL: &[u8] = br#"{
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "title",
                "type": "text",
                "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 10.0},
                "data": {"text": "printkit"}
            }]
        }]
    }"#;

    fn stages(sink: &MemorySink) -> Vec<(Stage, bool)> {
        sink.events()
            .into_iter()
            .map(|event| (event.stage, event.ok))
            .collect()
    }

    #[cfg(feature = "url-fetch")]
    fn serve_html(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://127.0.0.1:{port}/sample.html"), handle)
    }

    /// The fixture server listens on loopback, so tests opt into private
    /// networks exactly as the URL fixture script does.
    #[cfg(feature = "url-fetch")]
    fn local_fetch() -> FetchPolicy {
        FetchPolicy {
            allow_private_networks: true,
            ..FetchPolicy::default()
        }
    }

    #[cfg(feature = "url-fetch")]
    #[test]
    fn url_items_are_fetched_before_rendering() {
        let (url, server) = serve_html("<p>remote body</p>");
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[{{
                "id":"remote","type":"html",
                "frame":{{"x":20.0,"y":20.0,"width":100.0,"height":30.0}},
                "data":{{"url":"{url}"}}
            }}]}}]}}"#
        );
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let result = futures::executor::block_on(export_pdf(
            document.as_bytes(),
            &host,
            &sink,
            &local_fetch(),
        ))
        .unwrap();
        assert!(result.html.contains("<p>remote body</p>"));
        let render = sink
            .events()
            .into_iter()
            .find(|event| event.stage == Stage::Render)
            .unwrap();
        assert!(
            render
                .fields
                .iter()
                .any(|(key, _)| *key == FieldKey::FetchMs)
        );
        server.join().unwrap();
    }

    #[cfg(feature = "url-fetch")]
    #[test]
    fn a_fetch_failure_stops_before_the_webview() {
        let (url, server) = serve_html("<p>too large</p>");
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[{{
                "id":"remote","type":"html",
                "frame":{{"x":20.0,"y":20.0,"width":100.0,"height":30.0}},
                "data":{{"url":"{url}"}}
            }}]}}]}}"#
        );
        let policy = FetchPolicy {
            max_bytes: 4,
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error =
            futures::executor::block_on(export_pdf(document.as_bytes(), &host, &sink, &policy))
                .unwrap_err();
        assert_eq!(error.code(), "E_TOO_LARGE");
        assert_eq!(host.calls(), 0);
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Render, false),
                (Stage::Result, false),
            ]
        );
        server.join().unwrap();
    }

    #[cfg(feature = "url-fetch")]
    #[test]
    fn too_many_urls_are_refused_before_the_webview() {
        let items: Vec<String> = (0..=printkit_core::limits::MAX_FETCH_URLS)
            .map(|index| {
                format!(
                    r#"{{"id":"remote-{index}","type":"html",
                    "frame":{{"x":20.0,"y":20.0,"width":100.0,"height":30.0}},
                    "data":{{"url":"http://127.0.0.1/{index}.html"}}}}"#
                )
            })
            .collect();
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[{}]}}]}}"#,
            items.join(",")
        );
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error = futures::executor::block_on(export_pdf(
            document.as_bytes(),
            &host,
            &sink,
            &local_fetch(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_TOO_LARGE");
        assert_eq!(host.calls(), 0);
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Render, false),
                (Stage::Result, false),
            ]
        );
    }

    #[cfg(feature = "url-fetch")]
    #[test]
    fn all_url_fetches_share_one_deadline() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                std::thread::sleep(std::time::Duration::from_millis(150));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        let items: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| {
                format!(
                    r#"{{"id":"{name}","type":"html",
                    "frame":{{"x":20.0,"y":20.0,"width":100.0,"height":30.0}},
                    "data":{{"url":"http://127.0.0.1:{port}/{name}.html"}}}}"#
                )
            })
            .collect();
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[{}]}}]}}"#,
            items.join(",")
        );
        // One URL may spend 50 ms, and the whole document shares 4×50 ms, so
        // the second request gets only the remainder of that budget.
        let policy = FetchPolicy {
            timeout_ms: 50,
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error =
            futures::executor::block_on(export_pdf(document.as_bytes(), &host, &sink, &policy))
                .unwrap_err();
        assert_eq!(error.code(), "E_TIMEOUT");
        assert_eq!(host.calls(), 0);
        server.join().unwrap();
    }

    /// Without `url-fetch` the build carries no TLS stack, so a URL item must
    /// fail instead of being silently rendered as an empty frame.
    #[cfg(not(feature = "url-fetch"))]
    #[test]
    fn a_url_document_is_refused_without_url_fetch() {
        let document = br#"{
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "remote",
                    "type": "html",
                    "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 30.0},
                    "data": {"url": "http://127.0.0.1/sample.html"}
                }]
            }]
        }"#;
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error = futures::executor::block_on(export_pdf(
            document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
        assert_eq!(host.calls(), 0);
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Render, false),
                (Stage::Result, false),
            ]
        );
    }

    #[test]
    fn successful_export_emits_four_stages_with_both_render_durations() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let result =
            futures::executor::block_on(export_pdf(MINIMAL, &host, &sink, &FetchPolicy::default()))
                .unwrap();
        assert!(result.pdf.starts_with(b"%PDF-"));
        assert_eq!(result.pages, 1);
        assert!(result.html.contains("printkit"));

        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Render, true),
                (Stage::Result, true),
            ]
        );
        let events = sink.events();
        let render = events
            .iter()
            .find(|event| event.stage == Stage::Render)
            .unwrap();
        let fields: BTreeMap<_, _> = render
            .fields
            .iter()
            .map(|(key, value)| (format!("{key:?}"), value.clone()))
            .collect();
        assert!(fields.contains_key("HtmlMs"));
        assert!(fields.contains_key("PdfMs"));
        assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
        assert!(events.iter().all(|event| event.trace_id == result.trace_id));
    }

    #[test]
    fn invalid_json_stops_before_render() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error =
            futures::executor::block_on(export_pdf(b"{", &host, &sink, &FetchPolicy::default()))
                .unwrap_err();
        assert_eq!(error.code(), "E_SCHEMA");
        assert_eq!(host.calls(), 0);
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, false),
                (Stage::Result, false),
            ]
        );
    }

    #[test]
    fn structural_failure_reports_json_pointer() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let document = br#"{"version": 1, "pages": [], "bogus": true}"#;
        let error = futures::executor::block_on(export_pdf(
            document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_SCHEMA");
        assert_eq!(error.pointer(), Some("/bogus"));
        assert_eq!(host.calls(), 0);
    }

    #[test]
    fn multi_page_document_reports_every_page() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let document = br#"{
            "version": 1,
            "pages": [
                {"spec": {"size": "A4"}, "items": []},
                {"spec": {"size": "A5"}, "items": [{
                    "id": "second",
                    "type": "text",
                    "frame": {"x": 1.0, "y": 1.0, "width": 10.0, "height": 5.0},
                    "data": {"text": "printkit"}
                }]}
            ]
        }"#;
        let result = futures::executor::block_on(export_pdf(
            document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        assert_eq!(result.pages, 2);
        assert_eq!(result.html.matches("class=\"page\"").count(), 2);
    }

    #[test]
    fn oversized_document_is_rejected_before_parse() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let input = vec![b' '; printkit_core::limits::MAX_DOCUMENT_BYTES + 1];
        let error =
            futures::executor::block_on(export_pdf(&input, &host, &sink, &FetchPolicy::default()))
                .unwrap_err();
        assert_eq!(error.code(), "E_TOO_LARGE");
    }

    #[test]
    fn rejected_document_stops_before_render() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let document = br#"{
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "title",
                    "type": "text",
                    "frame": {"x": -1.0, "y": 0.0, "width": 10.0, "height": 5.0},
                    "data": {"text": "printkit"}
                }]
            }]
        }"#;
        let error = futures::executor::block_on(export_pdf(
            document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_SCHEMA");
        assert_eq!(host.calls(), 0);
        let events = sink.events();
        assert_eq!(events[1].code.as_deref(), Some("E_SCHEMA"));
    }

    #[test]
    fn platform_timeout_maps_to_e_timeout() {
        struct TimeoutHost;

        #[async_trait]
        impl RenderHost for TimeoutHost {
            async fn render_to_pdf(
                &self,
                _html: &str,
                _spec: &RenderSpec,
            ) -> Result<Bytes, PlatformError> {
                Err(PlatformError::Timeout(10_000))
            }

            fn preview_available(&self) -> bool {
                false
            }

            async fn show_window(
                &self,
                _request: printkit_platform::PreviewRequest,
            ) -> Result<printkit_platform::WindowId, PlatformError> {
                Err(PlatformError::Unsupported("no preview windows".to_owned()))
            }

            async fn window_png(
                &self,
                _window: printkit_platform::WindowId,
                _page: u32,
                _dpi: u32,
            ) -> Result<Bytes, PlatformError> {
                Err(PlatformError::Unsupported("no preview windows".to_owned()))
            }

            async fn close_window(
                &self,
                window: printkit_platform::WindowId,
            ) -> Result<(), PlatformError> {
                Err(PlatformError::WindowNotFound(window.to_string()))
            }
        }

        let sink = MemorySink::default();

        let error = futures::executor::block_on(export_pdf(
            MINIMAL,
            &TimeoutHost,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_TIMEOUT");
        let events = sink.events();
        let render = events
            .iter()
            .find(|event| event.stage == Stage::Render)
            .unwrap();
        assert!(!render.ok);
        assert_eq!(render.code.as_deref(), Some("E_TIMEOUT"));
    }

    #[test]
    fn render_duration_is_visible_in_pdf_ms() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new().with_delay(Duration::from_millis(20));
        let _ =
            futures::executor::block_on(export_pdf(MINIMAL, &host, &sink, &FetchPolicy::default()))
                .unwrap();
        let events = sink.events();
        let render = events
            .iter()
            .find(|event| event.stage == Stage::Render)
            .unwrap();
        let pdf_ms = render.fields.get(&FieldKey::PdfMs).cloned();
        assert!(matches!(pdf_ms, Some(Scalar::Int(value)) if value >= 20));
    }

    fn test_pool(host: FakeRenderHost) -> Arc<RenderPool> {
        let host = Arc::new(host);
        RenderPool::new(
            PoolConfig::new(1, 32, Duration::from_secs(10)),
            Arc::new(move || host.clone() as Arc<dyn RenderHost>),
        )
    }

    #[test]
    fn export_pdf_to_path_writes_a_relative_path_under_exports() {
        let data_dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(MemorySink::default());
        let host = FakeRenderHost::new();
        let pool = test_pool(host.clone());
        let result = export_pdf_to_path(
            MINIMAL,
            data_dir.path(),
            "quotes/2026-11.pdf",
            &pool,
            &host,
            sink.clone(),
            &FetchPolicy::default(),
        )
        .unwrap();
        let written = data_dir.path().join("exports/quotes/2026-11.pdf");
        assert!(written.is_file());
        let stored = std::fs::read(&written).unwrap();
        assert!(stored.starts_with(b"%PDF-"));
        assert_eq!(result.path, "exports/quotes/2026-11.pdf");
        let canonical = std::fs::canonicalize(&written).unwrap();
        assert_eq!(result.absolute_path, canonical.to_string_lossy());
        assert_eq!(result.pages, 1);
        assert!(result.bytes > 0);
        assert_eq!(
            stages(sink.as_ref()),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Queue, true),
                (Stage::Render, true),
                (Stage::Result, true),
            ]
        );
    }

    #[test]
    fn export_pdf_to_path_rejects_an_escape_before_rendering() {
        let data_dir = tempfile::tempdir().unwrap();
        for path in ["../escape.pdf", "/tmp/escape.pdf", "a/../../escape.pdf"] {
            let sink = Arc::new(MemorySink::default());
            let host = FakeRenderHost::new();
            let pool = test_pool(host.clone());
            let error = export_pdf_to_path(
                MINIMAL,
                data_dir.path(),
                path,
                &pool,
                &host,
                sink.clone(),
                &FetchPolicy::default(),
            )
            .unwrap_err();
            assert_eq!(error.code(), "E_SCHEMA", "{path}");
            assert_eq!(host.calls(), 0, "{path}");
            assert_eq!(
                stages(sink.as_ref()),
                vec![(Stage::Recv, true), (Stage::Result, false)],
                "{path}"
            );
        }
    }

    #[test]
    fn export_pdf_to_path_reports_a_failed_write_on_the_result_stage() {
        let data_dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(MemorySink::default());
        let host = FakeRenderHost::new();
        let pool = test_pool(host.clone());
        // A directory in place of the file makes the write fail after rendering.
        std::fs::create_dir_all(data_dir.path().join("exports/taken.pdf")).unwrap();
        let error = export_pdf_to_path(
            MINIMAL,
            data_dir.path(),
            "taken.pdf",
            &pool,
            &host,
            sink.clone(),
            &FetchPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_JOB_FAILED");
        assert_eq!(host.calls(), 1);
        let events = sink.events();
        assert_eq!(
            events.iter().map(|event| event.stage).collect::<Vec<_>>(),
            vec![
                Stage::Recv,
                Stage::Validate,
                Stage::Queue,
                Stage::Render,
                Stage::Result
            ]
        );
        let result = events.last().unwrap();
        assert!(!result.ok);
        assert_eq!(result.code.as_deref(), Some("E_JOB_FAILED"));
    }

    #[test]
    fn export_pdf_to_path_reports_a_refused_reservation() {
        let data_dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(MemorySink::default());
        let host = Arc::new(FakeRenderHost::new());
        let holder = Arc::clone(&host);
        let pool = RenderPool::new(
            PoolConfig::new(1, 1, Duration::from_secs(10)),
            Arc::new(move || holder.clone() as Arc<dyn RenderHost>),
        );
        let _held = pool
            .reserve(PreparedRender {
                priority: JobPriority::Export,
                html: "<html></html>".to_owned(),
                spec: RenderSpec::new(210.0, 297.0),
                render_pdf: true,
                pdf_only: false,
                hooks: JobHooks {
                    update: Box::new(|_, _| {}),
                    finished: Box::new(|_| JobOutcome::Done {
                        warnings: Vec::new(),
                    }),
                },
            })
            .unwrap();
        let error = export_pdf_to_path(
            MINIMAL,
            data_dir.path(),
            "out.pdf",
            &pool,
            host.as_ref(),
            sink.clone(),
            &FetchPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_TIMEOUT");
        assert_eq!(
            stages(sink.as_ref()),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Queue, false),
                (Stage::Result, false),
            ]
        );
        let events = sink.events();
        assert_eq!(events[2].queue_pos, Some(2));
        assert_eq!(events[3].code.as_deref(), Some("E_TIMEOUT"));
    }

    fn embedded_pdf(extra_resource: &str) -> Vec<u8> {
        let content = b"BT /F1 12 Tf 40 800 Td (embedded page) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
                 /Resources << /Font << /F1 5 0 R >> {extra_resource} >> >>"
            ),
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                content.len(),
                String::from_utf8_lossy(content)
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /AAAAAA+Embedded /FontDescriptor 6 0 R >>"
                .to_owned(),
            "<< /Type /FontDescriptor /FontName /AAAAAA+Embedded /Flags 4 \
             /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
             /CapHeight 700 /StemV 80 /FontFile 7 0 R >>"
                .to_owned(),
            "<< /Length 4 >>\nstream\nfake\nendstream".to_owned(),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!("{} 0 obj\n{object}\nendobj\n", index + 1),
            );
        }
        let xref_start = out.len();
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1),
        );
        for offset in offsets {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{offset:010} 00000 n \n"));
        }
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF\n",
                objects.len() + 1
            ),
        );
        out.into_bytes()
    }

    fn encrypted_pdf() -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.4");
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference((2, 0)));
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![lopdf::Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut page = lopdf::Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", lopdf::Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![
                lopdf::Object::Integer(0),
                lopdf::Object::Integer(0),
                lopdf::Object::Integer(595),
                lopdf::Object::Integer(842),
            ],
        );
        let mut encrypt = lopdf::Dictionary::new();
        encrypt.set("Filter", "Standard");
        encrypt.set("V", 1);
        encrypt.set("R", 2);
        encrypt.set(
            "O",
            lopdf::Object::String(vec![0; 32], lopdf::StringFormat::Literal),
        );
        encrypt.set(
            "U",
            lopdf::Object::String(vec![0; 32], lopdf::StringFormat::Literal),
        );
        encrypt.set("P", -1);
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        let encrypt_id = document.add_object(encrypt);
        document
            .trailer
            .set("Root", lopdf::Object::Reference((1, 0)));
        document
            .trailer
            .set("Encrypt", lopdf::Object::Reference(encrypt_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn pdf_item_document(pdf: &[u8], page: u32) -> Vec<u8> {
        let src = format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(pdf)
        );
        format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[
                {{"id":"host","type":"text","frame":{{"x":10.0,"y":10.0,"width":100.0,"height":10.0}},
                  "data":{{"text":"host"}}}},
                {{"id":"overlay","type":"pdf","frame":{{"x":20.0,"y":40.0,"width":100.0,"height":140.0}},
                  "data":{{"src":"{src}","page":{page}}}}}
            ]}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn an_embedded_page_is_merged_as_a_form_xobject() {
        let embedded = embedded_pdf("");
        let document = pdf_item_document(&embedded, 1);
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let result = futures::executor::block_on(export_pdf(
            &document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        assert_eq!(host.calls(), 1);

        let merged = lopdf::Document::load_mem(&result.pdf).unwrap();
        let page_id = merged.get_pages()[&1];
        let page = merged.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let form = xobjects
            .iter()
            .next()
            .and_then(|(_, value)| value.as_reference().ok())
            .and_then(|id| merged.get_object(id).ok())
            .and_then(|object| object.as_stream().ok())
            .expect("form xobject");
        assert_eq!(
            form.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form"
        );
        assert!(
            merged
                .get_page_content(page_id)
                .windows(3)
                .any(|window| window == b" Do")
        );
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Validate, true),
                (Stage::Render, true),
                (Stage::Result, true),
            ]
        );
    }

    /// A renderer-shaped PDF with `pages` pages and one embedded subset font.
    fn multipage_host_pdf(pages: u32) -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.4");
        let content_id = 3 + pages;
        let font_id = 4 + pages;
        let descriptor_id = 5 + pages;
        let font_file_id = 6 + pages;

        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference((2, 0)));
        let mut page_tree = lopdf::Dictionary::new();
        page_tree.set("Type", "Pages");
        page_tree.set(
            "Kids",
            (0..pages)
                .map(|index| lopdf::Object::Reference((3 + index, 0)))
                .collect::<Vec<_>>(),
        );
        page_tree.set("Count", pages);
        document.add_object(catalog);
        document.add_object(page_tree);
        for _ in 0..pages {
            let mut page = lopdf::Dictionary::new();
            page.set("Type", "Page");
            page.set("Parent", lopdf::Object::Reference((2, 0)));
            page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
            page.set("Contents", lopdf::Object::Reference((content_id, 0)));
            let mut resources = lopdf::Dictionary::new();
            let mut fonts = lopdf::Dictionary::new();
            fonts.set("F1", lopdf::Object::Reference((font_id, 0)));
            resources.set("Font", lopdf::Object::Dictionary(fonts));
            page.set("Resources", lopdf::Object::Dictionary(resources));
            document.add_object(page);
        }
        let content = b"BT /F1 24 Tf 72 770 Td (printkit continuation) Tj ET";
        document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            content.to_vec(),
        ));
        let mut font = lopdf::Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Fake");
        font.set(
            "FontDescriptor",
            lopdf::Object::Reference((descriptor_id, 0)),
        );
        document.add_object(font);
        let mut descriptor = lopdf::Dictionary::new();
        descriptor.set("Type", "FontDescriptor");
        descriptor.set("FontName", "AAAAAA+Fake");
        descriptor.set("Flags", 4);
        descriptor.set(
            "FontBBox",
            vec![0.into(), 0.into(), 1000.into(), 1000.into()],
        );
        descriptor.set("ItalicAngle", 0);
        descriptor.set("Ascent", 800);
        descriptor.set("Descent", -200);
        descriptor.set("CapHeight", 700);
        descriptor.set("StemV", 80);
        descriptor.set("FontFile", lopdf::Object::Reference((font_file_id, 0)));
        document.add_object(descriptor);
        document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"fake".to_vec(),
        ));
        document
            .trailer
            .set("Root", lopdf::Object::Reference((1, 0)));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("serialise");
        bytes
    }

    fn xobject_names(document: &lopdf::Document, page: u32) -> Vec<String> {
        let page_id = document.get_pages()[&page];
        let page = document.get_dictionary(page_id).unwrap();
        let Ok(resources) = page.get(b"Resources").and_then(lopdf::Object::as_dict) else {
            return Vec::new();
        };
        let Ok(xobjects) = resources.get(b"XObject").and_then(lopdf::Object::as_dict) else {
            return Vec::new();
        };
        xobjects
            .iter()
            .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
            .collect()
    }

    #[test]
    fn an_embedded_page_lands_on_the_paper_page_of_its_document_page() {
        let embedded = embedded_pdf("");
        let src = format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&embedded)
        );
        let rows: Vec<serde_json::Value> = (0..120)
            .map(|index| serde_json::json!({"no": format!("{index:03}"), "amount": "1.00"}))
            .collect();
        // The table spills onto three continuation pages, so document page 2
        // starts at paper page 4.
        let document = serde_json::json!({
            "version": 1,
            "pages": [
                {"spec": {"size": "A4"}, "items": [{
                    "id": "long",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 277.0},
                    "data": {
                        "columns": [
                            {"key": "no", "title": "No", "width": 40.0},
                            {"key": "amount", "title": "Amount", "width": "auto"}
                        ],
                        "rows": rows
                    },
                    "style": {"fontSize": 2.8, "rowHeight": 6.0}
                }]},
                {"spec": {"size": "A4"}, "items": [
                    {
                        "id": "host",
                        "type": "text",
                        "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 10.0},
                        "data": {"text": "after table"}
                    },
                    {
                        "id": "overlay",
                        "type": "pdf",
                        "frame": {"x": 20.0, "y": 40.0, "width": 100.0, "height": 140.0},
                        "data": {"src": src, "page": 1}
                    }
                ]}
            ]
        })
        .to_string()
        .into_bytes();

        let sink = MemorySink::default();
        let host = FakeRenderHost::new().with_pdf(multipage_host_pdf(4));
        let result = futures::executor::block_on(export_pdf(
            &document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        let merged = lopdf::Document::load_mem(&result.pdf).unwrap();
        assert_eq!(merged.get_pages().len(), 4);
        assert_eq!(xobject_names(&merged, 4), vec!["PdfForm1".to_owned()]);
        assert!(xobject_names(&merged, 1).is_empty());
    }
    #[test]
    fn an_unsupported_embedded_object_carries_its_number() {
        let embedded = embedded_pdf("/XObject << /Bad 99 0 R >>");
        let document = pdf_item_document(&embedded, 1);
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error = futures::executor::block_on(export_pdf(
            &document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
        let detail = error.detail().unwrap();
        assert_eq!(detail["violations"][0]["object"], "99 0 R");
        assert_eq!(host.calls(), 0);
    }

    #[test]
    fn an_encrypted_embedded_pdf_stops_before_the_webview() {
        let encrypted = encrypted_pdf();
        let document = pdf_item_document(&encrypted, 1);
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error = futures::executor::block_on(export_pdf(
            &document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_PDF_ENCRYPTED");
        assert_eq!(host.calls(), 0);
    }

    #[test]
    fn a_page_past_the_end_is_reported_before_the_webview() {
        let embedded = embedded_pdf("");
        let document = pdf_item_document(&embedded, 2);
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error = futures::executor::block_on(export_pdf(
            &document,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "E_SCHEMA");
        assert_eq!(error.pointer(), Some("/pages/0/items/1/data/page"));
        assert_eq!(host.calls(), 0);
    }

    fn render_fields(sink: &MemorySink) -> BTreeMap<FieldKey, Scalar> {
        sink.events()
            .into_iter()
            .find(|event| event.stage == Stage::Render)
            .map(|event| event.fields)
            .unwrap_or_default()
    }

    #[test]
    fn merge_time_is_reported_only_when_a_page_is_merged() {
        let embedded = embedded_pdf("");
        let document = pdf_item_document(&embedded, 1);
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        futures::executor::block_on(export_pdf(&document, &host, &sink, &FetchPolicy::default()))
            .unwrap();
        assert!(matches!(
            render_fields(&sink).get(&FieldKey::MergeMs),
            Some(Scalar::Int(_))
        ));

        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        futures::executor::block_on(export_pdf(MINIMAL, &host, &sink, &FetchPolicy::default()))
            .unwrap();
        assert!(!render_fields(&sink).contains_key(&FieldKey::MergeMs));
    }

    #[test]
    fn print_writes_one_usage_row_and_the_request_auth_stage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let sink = Arc::new(MemorySink::default());
        let pool = test_pool(FakeRenderHost::new());
        let document = br#"{
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": []}],
            "output": {"copies": 2, "printer": {"name": "lab"}},
            "meta": {"templateId": "invoice", "tags": ["store", "quote"]}
        }"#;
        let usage = UsageRecorder::from_store(
            Arc::clone(&store),
            "https://app.example".to_owned(),
            90,
            5000,
        );
        let backend: Arc<dyn PrinterBackend> = Arc::new(FakePrinterBackend::new());
        let jobs = PrintJobs::new();
        let reserved = reserve_print(
            document,
            &pool,
            &FakeRenderHost::new(),
            backend,
            &jobs,
            sink.clone(),
            &FetchPolicy::default(),
            Box::new(|_, _| {}),
            Some(usage.clone()),
        )
        .unwrap();
        reserved.reservation.activate();
        let mut record = None;
        for _ in 0..200 {
            let rows = store
                .list_print_records(&printkit_store::RecordFilter {
                    limit: 10,
                    ..printkit_store::RecordFilter::default()
                })
                .unwrap();
            if let Some(row) = rows.into_iter().next() {
                record = Some(row);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        usage.close_and_drain();
        let record = record.expect("no usage row was written");
        assert_eq!(record.origin, "https://app.example");
        assert_eq!(record.template.as_deref(), Some("invoice"));
        assert_eq!(record.tags, vec!["store".to_owned(), "quote".to_owned()]);
        assert_eq!(record.printer.as_deref(), Some("lab"));
        assert_eq!(record.copies, 2);
        assert_eq!(record.status, "done");

        let events = sink.events();
        let stages: Vec<Stage> = events.iter().map(|event| event.stage).collect();
        assert_eq!(
            stages,
            vec![
                Stage::Recv,
                Stage::Auth,
                Stage::Validate,
                Stage::Queue,
                Stage::Render,
                Stage::Submit,
                Stage::Result
            ],
            "{events:?}"
        );
        let result = events.last().unwrap();
        assert_eq!(u64::try_from(record.seq).unwrap(), result.seq);
        assert_eq!(result.trace_id.as_str(), record.trace_id);
    }

    #[test]
    fn print_without_a_usage_recorder_writes_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let sink = Arc::new(MemorySink::default());
        let pool = test_pool(FakeRenderHost::new());
        let backend: Arc<dyn PrinterBackend> = Arc::new(FakePrinterBackend::new());
        let jobs = PrintJobs::new();
        let reserved = reserve_print(
            MINIMAL,
            &pool,
            &FakeRenderHost::new(),
            backend,
            &jobs,
            sink,
            &FetchPolicy::default(),
            Box::new(|_, _| {}),
            None,
        )
        .unwrap();
        reserved.reservation.activate();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(store.print_record_count().unwrap().count, 0);
    }

    #[test]
    fn one_shot_print_records_usage() {
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
        let store = usage.store();
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let backend = FakePrinterBackend::new();
        print_document_now(
            MINIMAL,
            &host,
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            None,
            Some(usage.clone()),
        )
        .unwrap();
        usage.close_and_drain();
        let rows = store
            .list_print_records(&printkit_store::RecordFilter {
                limit: 10,
                ..printkit_store::RecordFilter::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        let record = &rows[0];
        assert_eq!(record.origin, "cli");
        assert_eq!(record.pages, 1);
        assert_eq!(record.copies, 1);
        assert_eq!(record.status, "done");
        assert_eq!(record.template, None);
        assert_eq!(record.tags, Vec::<String>::new());
        let events = sink.events();
        let result = events.last().unwrap();
        assert_eq!(u64::try_from(record.seq).unwrap(), result.seq);
        assert_eq!(result.trace_id.as_str(), record.trace_id);
    }

    #[test]
    fn one_shot_raw_records_one_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
        let store = usage.store();
        let sink = MemorySink::default();
        let backend = FakePrinterBackend::raw_capable();
        raw_send_now(
            Bytes::from_static(b"\x1b@label"),
            &backend,
            &sink,
            None,
            "label".to_owned(),
            Some(usage.clone()),
        )
        .unwrap();
        usage.close_and_drain();
        let rows = store
            .list_print_records(&printkit_store::RecordFilter {
                limit: 10,
                ..printkit_store::RecordFilter::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        let record = &rows[0];
        assert_eq!(record.origin, "cli");
        assert_eq!(record.pages, 1);
        assert_eq!(record.copies, 1);
        assert_eq!(record.status, "done");
    }

    fn usage_fixture<'a>(trace_id: &'a str, at: &'a str, tags: &'a [String]) -> NewPrintRecord<'a> {
        NewPrintRecord {
            trace_id,
            seq: 1,
            at,
            origin: "cli",
            template: None,
            tags,
            printer: None,
            pages: 1,
            copies: 1,
            status: "done",
            dur_ms: 0,
            error_code: None,
        }
    }

    #[test]
    fn close_rejects_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
        usage.close_and_drain();
        let tags = Vec::new();
        let row = usage_fixture("t", "2026-11-13T00:00:00Z", &tags);
        for _ in 0..3 {
            usage.record(&row);
        }
        let store = usage.store();
        assert_eq!(store.print_record_count().unwrap().count, 0);
        assert_eq!(usage.dropped_after_close(), 3);
    }

    #[test]
    fn close_drains_enqueued_rows_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
        let tags = Vec::new();
        let row = usage_fixture("t1", "2026-11-13T00:00:00Z", &tags);
        usage.record(&row);
        let row = usage_fixture("t2", "2026-11-13T00:00:01Z", &tags);
        usage.record(&row);
        let first = usage.close_and_drain();
        assert!(first.drained, "{first:?}");
        assert_eq!(usage.dropped_full(), 0);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(usage.store().print_record_count().unwrap().count, 2);
        assert!(usage.close_and_drain().drained);
    }

    #[test]
    fn dropping_the_recorder_drains_rows() {
        let dir = tempfile::tempdir().unwrap();
        let tags = Vec::new();
        let store = {
            let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
            let row = usage_fixture("t", "2026-11-13T00:00:00Z", &tags);
            usage.record(&row);
            usage.store()
        };
        assert_eq!(store.print_record_count().unwrap().count, 1);
    }

    #[test]
    fn concurrent_record_and_close_never_writes_twice() {
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageRecorder::open(dir.path(), "cli".to_owned(), 90, 5000).unwrap();
        let tags = Vec::new();
        let cloned = usage.clone();
        let sender = std::thread::spawn(move || {
            for index in 0..100 {
                let trace = format!("t{index}");
                let row = usage_fixture(&trace, "2026-11-13T00:00:00Z", &tags);
                cloned.record(&row);
            }
        });
        sender.join().unwrap();
        usage.close_and_drain();
        let written = u64::try_from(usage.store().print_record_count().unwrap().count).unwrap();
        let dropped = usage.dropped_after_close();
        assert_eq!(written + dropped, 100);
    }

    fn duplex_document(duplex: &str) -> Vec<u8> {
        format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[]}}],"output":{{"printer":{{"duplex":"{duplex}"}}}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn duplex_request_is_rejected_until_a_printer_is_verified() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let backend = FakePrinterBackend::new();
        let document = duplex_document("duplex_long");
        let error = print_document_now(
            &document,
            &host,
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
        assert_eq!(error.pointer(), Some("/output/printer/duplex"));
        let detail = error.detail().expect("detail");
        assert_eq!(detail["pointer"], "/output/printer/duplex");
        assert_eq!(host.calls(), 0, "a rejected duplex request must not render");
    }

    #[test]
    fn simplex_request_prints_normally() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let backend = FakePrinterBackend::new();
        let document = duplex_document("simplex");
        print_document_now(
            &document,
            &host,
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_too_deep_graph_detail_names_no_object() {
        let error = EngineError::Pdf(printkit_pdf::PdfError::TooDeep { limit: 64 });
        assert_eq!(error.code(), "E_UNSUPPORTED");
        let detail = error.detail().expect("detail");
        assert!(detail.get("object").is_none(), "{detail}");
        assert_eq!(detail["reason"], "object graph is deeper than 64");
    }

    #[test]
    fn preview_serialises_the_same_html_as_the_print_path() {
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let preview = prepare_preview(
            MINIMAL,
            &sink,
            None,
            &FetchPolicy::default(),
            &FakeRenderHost::new(),
        )
        .unwrap();
        let exported =
            futures::executor::block_on(export_pdf(MINIMAL, &host, &sink, &FetchPolicy::default()))
                .unwrap();
        assert_eq!(preview.html, exported.html);
        assert_eq!(preview.spec.pages, exported.pages as usize);
    }
    #[test]
    fn preview_reports_the_warnings_of_the_print_path() {
        const UNKNOWN_FACE: &[u8] = br#"{
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "title",
                    "type": "text",
                    "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 10.0},
                    "style": {"fontFamily": "No Such Face"},
                    "data": {"text": "printkit"}
                }]
            }]
        }"#;
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let preview = prepare_preview(
            UNKNOWN_FACE,
            &sink,
            None,
            &FetchPolicy::default(),
            &FakeRenderHost::new(),
        )
        .unwrap();
        let exported = futures::executor::block_on(export_pdf(
            UNKNOWN_FACE,
            &host,
            &sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        assert_eq!(preview.warnings, exported.warnings);
        assert_eq!(preview.warnings.len(), 1, "{:?}", preview.warnings);
        assert_eq!(preview.warnings[0].code, "W_FONT_UNKNOWN");
    }

    #[test]
    fn a_prepared_preview_stops_its_trace_at_render() {
        let sink = MemorySink::default();
        let preview = prepare_preview(
            MINIMAL,
            &sink,
            Some("https://app.example"),
            &FetchPolicy::default(),
            &FakeRenderHost::new(),
        )
        .unwrap();
        assert_eq!(
            stages(&sink),
            vec![
                (Stage::Recv, true),
                (Stage::Auth, true),
                (Stage::Validate, true),
                (Stage::Render, true),
            ]
        );
        let events = sink.events();
        assert!(
            events
                .iter()
                .all(|event| event.trace_id == preview.trace_id),
            "the preview spans more than one trace"
        );
        assert!(events[0].fields.contains_key(&FieldKey::DocumentSha256));
        assert_eq!(events[1].origin.as_deref(), Some("https://app.example"));
        assert_eq!(
            events[3].pages,
            Some(u32::try_from(preview.spec.pages).unwrap())
        );
    }

    #[test]
    fn a_refused_preview_leaves_recv_and_a_failed_result() {
        let sink = MemorySink::default();
        trace_preview_refusal(MINIMAL, &sink, None, "E_TOO_LARGE", "at most 8");
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].stage, Stage::Recv);
        assert!(events[0].fields.contains_key(&FieldKey::Bytes));
        assert_eq!(events[1].stage, Stage::Result);
        assert!(!events[1].ok);
        assert_eq!(events[1].code.as_deref(), Some("E_TOO_LARGE"));
        assert_eq!(events[0].trace_id, events[1].trace_id);
    }

    #[test]
    fn the_same_document_replays_byte_identically() {
        let first_sink = MemorySink::default();
        let first = futures::executor::block_on(export_pdf(
            MINIMAL,
            &FakeRenderHost::new(),
            &first_sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        let second_sink = MemorySink::default();
        let second = futures::executor::block_on(export_pdf(
            MINIMAL,
            &FakeRenderHost::new(),
            &second_sink,
            &FetchPolicy::default(),
        ))
        .unwrap();
        assert_eq!(first.pdf, second.pdf, "renderer output must replay");
        assert_eq!(first.html, second.html);
    }

    #[test]
    fn a_table_without_measurement_warns_instead_of_hiding_the_estimate() {
        const TABLE: &[u8] = br#"{
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "t",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 200.0},
                    "data": {
                        "columns": [{"key": "a", "title": "A", "width": "auto"}],
                        "rows": [{"a": "x"}]
                    }
                }]
            }]
        }"#;
        let sink = MemorySink::default();
        // FakeRenderHost keeps the default trait method: measurement is
        // unsupported, which is exactly the fallback under test.
        let host = FakeRenderHost::new();
        let result =
            futures::executor::block_on(export_pdf(TABLE, &host, &sink, &FetchPolicy::default()))
                .unwrap();
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.code == "W_TABLE_HEIGHTS_ESTIMATED"),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn a_print_job_carries_render_warnings_to_done_and_status() {
        const TABLE: &[u8] = br#"{
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "t",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 200.0},
                    "data": {
                        "columns": [{"key": "a", "title": "A", "width": "auto"}],
                        "rows": [{"a": "x"}]
                    }
                }]
            }]
        }"#;
        let host = FakeRenderHost::new();
        let pool = test_pool(host.clone());
        let sink = Arc::new(MemorySink::default());
        let backend: Arc<dyn PrinterBackend> = Arc::new(FakePrinterBackend::new());
        let jobs = PrintJobs::new();
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&updates);
        let reserved = reserve_print(
            TABLE,
            &pool,
            &host,
            backend,
            &jobs,
            sink,
            &FetchPolicy::default(),
            Box::new(move |_, update| {
                seen.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(update);
            }),
            None,
        )
        .unwrap();
        let job_id = reserved.job_id.clone();
        reserved.reservation.activate();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if pool
                .status(&job_id)
                .is_some_and(|status| status.phase == JobPhase::Done)
            {
                break;
            }
            assert!(Instant::now() < deadline, "the print job never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = pool.status(&job_id).unwrap();
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.code == "W_TABLE_HEIGHTS_ESTIMATED"),
            "{:?}",
            status.warnings
        );
        let updates = updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let done = updates.iter().find_map(|update| match update {
            JobUpdate::Done { warnings } => Some(warnings),
            _ => None,
        });
        let done = done.expect("no done update");
        assert!(
            done.iter()
                .any(|warning| warning.code == "W_TABLE_HEIGHTS_ESTIMATED"),
            "{done:?}"
        );
    }

    #[test]
    fn a_job_missing_from_the_spooler_maps_to_not_found() {
        let error = EngineError::Platform(PlatformError::JobNotFound {
            printer: "printkit-test".to_owned(),
            job: 42,
            detail: "the job finished".to_owned(),
        });
        assert_eq!(error.code(), "E_NOT_FOUND");
    }
}

#[cfg(test)]
mod html_print_tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use printkit_core::limits::{MAX_BATCH_PAGES, MAX_BATCH_PDF_BYTES};
    use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
    use printkit_platform::{
        HtmlPrintJob, PaperInfo, PlatformError, PrintJob, PrinterBackend, PrinterInfo, RawJob,
        RenderHost, RenderSpec, SubmittedJob,
    };
    use printkit_trace::{FieldKey, MemorySink, NullSink, Scalar, Sink, Stage};

    use super::print::{check_merged_batch_bytes, check_pdf_passthrough_limits};
    use super::{
        BatchPart, FetchPolicy, JobHooks, JobOutcome, JobPhase, JobPriority, PoolConfig,
        PreparedRender, PrintJobs, PrintPdf, RenderPool, begin_print_batch, begin_print_pdf,
        print_batch_now, print_document_now, print_pdf_now, reserve_print, uniform_paper,
    };

    const MINIMAL: &[u8] = br#"{
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "title",
                "type": "text",
                "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 10.0},
                "data": {"text": "printkit"}
            }]
        }]
    }"#;

    const MIXED: &[u8] = br#"{
        "version": 1,
        "pages": [
            {
                "spec": {"size": "A4"},
                "items": [{
                    "id": "a4",
                    "type": "text",
                    "frame": {"x": 20.0, "y": 20.0, "width": 100.0, "height": 10.0},
                    "data": {"text": "A4"}
                }]
            },
            {
                "spec": {"size": "A5"},
                "items": [{
                    "id": "a5",
                    "type": "text",
                    "frame": {"x": 20.0, "y": 20.0, "width": 60.0, "height": 10.0},
                    "data": {"text": "A5"}
                }]
            }
        ]
    }"#;

    const EMBEDDED: &[u8] = include_bytes!("../../../tests/fixtures/embedded.json");

    const TABLE_DOC: &[u8] = br#"{
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "t",
                "type": "table",
                "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 200.0},
                "data": {
                    "columns": [{"key": "a", "title": "A", "width": "auto"}],
                    "rows": [{"a": "x"}]
                }
            }]
        }]
    }"#;

    /// A backend whose renderer prints HTML itself, like WebView2 `Print`.
    #[derive(Default)]
    struct HtmlBackend {
        jobs: Mutex<Vec<HtmlPrintJob>>,
        pdf_jobs: Mutex<Vec<PrintJob>>,
        prints_pdf: bool,
        pdf_job_id: Option<u32>,
    }

    impl HtmlBackend {
        fn pdf_capable() -> Self {
            Self {
                prints_pdf: true,
                pdf_job_id: Some(1),
                ..Self::default()
            }
        }

        fn windows_like() -> Self {
            Self {
                prints_pdf: true,
                pdf_job_id: None,
                ..Self::default()
            }
        }
    }

    impl PrinterBackend for HtmlBackend {
        fn printers(&self) -> Result<Vec<PrinterInfo>, PlatformError> {
            Ok(Vec::new())
        }

        fn papers(&self, _printer: &str) -> Result<Vec<PaperInfo>, PlatformError> {
            Ok(Vec::new())
        }

        fn submit(&self, job: &PrintJob) -> Result<SubmittedJob, PlatformError> {
            if !self.prints_pdf {
                return Err(PlatformError::Unsupported(
                    "this test prints HTML only".to_owned(),
                ));
            }
            self.pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(job.clone());
            Ok(SubmittedJob {
                job_id: self.pdf_job_id,
                printer: "pdf-printer".to_owned(),
            })
        }

        fn prints_pdf(&self) -> bool {
            self.prints_pdf
        }

        fn submit_raw(&self, _job: &RawJob) -> Result<SubmittedJob, PlatformError> {
            Err(PlatformError::RawUnsupported(
                "this test prints HTML".to_owned(),
            ))
        }

        fn cancel_submitted(&self, _job: &SubmittedJob) -> Result<(), PlatformError> {
            Err(PlatformError::CancelUnsupported)
        }

        fn cancel_submitted_supported(&self) -> bool {
            false
        }

        fn prints_html(&self) -> bool {
            true
        }

        fn submit_html(&self, job: &HtmlPrintJob) -> Result<SubmittedJob, PlatformError> {
            self.jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(job.clone());
            Ok(SubmittedJob {
                job_id: None,
                printer: "html-printer".to_owned(),
            })
        }
    }

    #[test]
    fn the_pool_skips_pdf_rendering_for_an_html_backend() {
        let host = FakeRenderHost::new();
        let factory_host = host.clone();
        let pool = RenderPool::new(
            PoolConfig::new(1, 1, Duration::from_secs(5)),
            Arc::new(move || Arc::new(factory_host.clone()) as Arc<dyn RenderHost>),
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let reservation = pool
            .reserve(PreparedRender {
                priority: JobPriority::Print,
                html: "<html></html>".to_owned(),
                spec: RenderSpec::new(210.0, 297.0),
                render_pdf: false,
                pdf_only: false,
                hooks: JobHooks {
                    update: Box::new(|_, _| {}),
                    finished: Box::new(move |report| {
                        let _ = sender.send((report.result.is_ok(), report.prepared.is_some()));
                        JobOutcome::Done {
                            warnings: Vec::new(),
                        }
                    }),
                },
            })
            .unwrap();
        reservation.activate();
        let (result_ok, has_prepared) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the pool finished the job");
        assert!(result_ok);
        assert!(has_prepared);
        assert_eq!(host.calls(), 0, "render_to_pdf ran for an HTML-only job");
    }

    #[test]
    fn mixed_page_sizes_are_not_uniform() {
        let uniform = RenderSpec::new(210.0, 297.0);
        assert!(uniform_paper(&uniform));
        let mixed =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (148.0, 210.0)]);
        assert!(!uniform_paper(&mixed));
        let same_two_pages =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (210.0, 297.0)]);
        assert!(uniform_paper(&same_two_pages));
    }

    fn fake_pool(host: &FakeRenderHost) -> Arc<RenderPool> {
        let host = host.clone();
        RenderPool::new(
            PoolConfig::new(1, 1, Duration::from_secs(5)),
            Arc::new(move || Arc::new(host.clone()) as Arc<dyn RenderHost>),
        )
    }

    #[test]
    fn mixed_paper_uses_pdf_submit_when_the_backend_can_print_pdf() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::pdf_capable();
        let done = print_document_now(
            MIXED,
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(done.submitted.printer, "pdf-printer");
        assert_eq!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        assert_eq!(host.calls(), 1, "mixed paper must render a PDF");
        assert_eq!(
            backend.pdf_jobs.lock().unwrap()[0].paper_mm,
            None,
            "mixed paper must not send a uniform driver media hint"
        );
    }

    #[test]
    fn uniform_paper_still_uses_html_when_the_backend_supports_both() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::pdf_capable();
        print_document_now(
            MINIMAL,
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        assert_eq!(host.calls(), 0, "uniform paper must stay on the HTML path");
    }

    #[test]
    fn a_pdf_only_document_skips_the_webview() {
        use base64::Engine as _;

        let source_host = FakeRenderHost::new();
        let source_pdf = futures::executor::block_on(
            source_host.render_to_pdf("<html></html>", &RenderSpec::new(210.0, 297.0)),
        )
        .unwrap();
        let src = format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&source_pdf)
        );
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[
                {{"id":"page","type":"pdf","frame":{{"x":10.0,"y":10.0,"width":100.0,"height":140.0}},
                  "data":{{"src":"{src}","page":1}}}}]}}]}}"#
        );
        let host = FakeRenderHost::new();
        let backend = printkit_platform::test_support::FakePrinterBackend::new();
        let done = print_document_now(
            document.as_bytes(),
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(host.calls(), 0, "a pdf-only document opened a WebView");
        assert_eq!(done.pages, 1);
        assert_eq!(backend.submitted().len(), 1);
    }

    #[test]
    fn embedded_pdf_uses_pdf_submit_on_a_windows_like_backend() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::windows_like();
        let sink = MemorySink::default();
        let done = print_document_now(
            EMBEDDED,
            &host,
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(done.submitted.job_id, None);
        assert_eq!(done.submitted.printer, "pdf-printer");
        assert_eq!(host.calls(), 1, "embedded PDF must render a PDF");
        let render = sink
            .events()
            .into_iter()
            .find(|event| event.stage == Stage::Render)
            .expect("render stage");
        assert!(render.fields.contains_key(&FieldKey::HtmlMs), "{render:?}");
        assert!(render.fields.contains_key(&FieldKey::PdfMs), "{render:?}");
        assert_eq!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "the embedded PDF must not go through submit_html"
        );
    }

    #[test]
    fn embedded_pdf_uses_the_pool_pdf_path_on_a_windows_like_backend() {
        let host = FakeRenderHost::new();
        let pool = fake_pool(&host);
        let backend = Arc::new(HtmlBackend::windows_like());
        let backend_dyn: Arc<dyn PrinterBackend> = backend.clone();
        let jobs = PrintJobs::new();
        let reserved = reserve_print(
            EMBEDDED,
            &pool,
            &host,
            backend_dyn,
            &jobs,
            Arc::new(MemorySink::default()),
            &FetchPolicy::default(),
            Box::new(|_, _| {}),
            None,
        )
        .unwrap();
        let job_id = reserved.job_id.clone();
        reserved.reservation.activate();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if pool
                .status(&job_id)
                .is_some_and(|status| status.phase == JobPhase::Done)
            {
                break;
            }
            assert!(Instant::now() < deadline, "the pooled print never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(host.calls(), 1, "pooled embedded PDF must render a PDF");
        assert_eq!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "pooled embedded PDF must not go through submit_html"
        );
    }

    #[test]
    fn a_table_on_a_windows_like_backend_stays_on_the_html_path() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::windows_like();
        print_document_now(
            TABLE_DOC,
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        assert_eq!(host.calls(), 0, "a table must stay on the HTML path");
    }

    #[test]
    fn external_jobs_wait_for_a_pool_slot_and_report_document_progress() {
        let host = FakeRenderHost::new();
        let factory_host = host.clone();
        let pool = RenderPool::new(
            PoolConfig::new(1, 4, Duration::from_secs(5)),
            Arc::new(move || Arc::new(factory_host.clone()) as Arc<dyn RenderHost>),
        );
        let first = pool.reserve_external(Box::new(|_, _| {})).unwrap();
        let mut first_handle = first.activate();
        first_handle.start();

        let second = pool.reserve_external(Box::new(|_, _| {})).unwrap();
        second.set_docs_total(3);
        let second_id = second.job_id().clone();
        let mut second_handle = second.activate();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            second_handle.start();
            second_handle.progress(1, 3);
            let _ = sender.send(());
            second_handle.finish(JobOutcome::Done {
                warnings: Vec::new(),
            });
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            receiver.try_recv().is_err(),
            "the second external job did not wait for the K slot"
        );
        first_handle.finish(JobOutcome::Done {
            warnings: Vec::new(),
        });
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("the waiting external job acquired the slot");
        assert_eq!(pool.status(&second_id).unwrap().docs_done, Some(1));
        assert_eq!(pool.status(&second_id).unwrap().docs_total, Some(3));
    }

    #[test]
    fn cancelling_a_queued_external_job_keeps_its_slot_until_the_runner_stops() {
        let host = FakeRenderHost::new();
        let factory_host = host.clone();
        let pool = RenderPool::new(
            PoolConfig::new(1, 2, Duration::from_secs(5)),
            Arc::new(move || Arc::new(factory_host.clone()) as Arc<dyn RenderHost>),
        );
        let first = pool.reserve_external(Box::new(|_, _| {})).unwrap();
        let mut first_handle = first.activate();
        first_handle.start();

        let waiting = pool.reserve_external(Box::new(|_, _| {})).unwrap();
        let waiting_id = waiting.job_id().clone();
        let mut waiting_handle = waiting.activate();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = entered_tx.send(());
            // This blocks until cancellation wakes it, then returns without a
            // permit. The inflight slot is released only by `finish`.
            waiting_handle.start();
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            waiting_handle.finish(JobOutcome::Cancelled);
            let _ = done_tx.send(());
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the runner reached the slot wait");
        assert!(matches!(
            pool.cancel_job(&waiting_id),
            Some(crate::CancelDisposition::ExternalQueued)
        ));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the cancelled runner left the slot wait");
        assert!(
            pool.reserve_external(Box::new(|_, _| {})).is_err(),
            "the cancelled runner still held its inflight slot"
        );
        let _ = release_tx.send(());
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the cancelled runner stopped");
        assert!(pool.reserve_external(Box::new(|_, _| {})).is_ok());
        first_handle.finish(JobOutcome::Done {
            warnings: Vec::new(),
        });
    }

    #[test]
    fn a_batch_with_a_pdf_part_has_no_uniform_paper_hint() {
        let host = FakeRenderHost::new();
        let pdf = futures::executor::block_on(
            host.render_to_pdf("<html></html>", &RenderSpec::new(210.0, 297.0)),
        )
        .unwrap();
        let backend = HtmlBackend::pdf_capable();
        let done = print_batch_now(
            vec![BatchPart::Document(MINIMAL.to_vec()), BatchPart::Pdf(pdf)],
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap();
        assert_eq!(done.submitted.printer, "pdf-printer");
        assert_eq!(
            backend.pdf_jobs.lock().unwrap()[0].paper_mm,
            None,
            "a PDF part makes the batch paper unknown, so no media hint"
        );
    }

    #[test]
    fn embedded_pdf_is_refused_without_a_pdf_backend() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::default();
        let error = print_document_now(
            EMBEDDED,
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
        assert_eq!(
            host.calls(),
            0,
            "a refused embedded request must not render"
        );
    }

    #[test]
    fn mixed_paper_is_refused_without_a_pdf_backend() {
        let host = FakeRenderHost::new();
        let backend = HtmlBackend::default();
        let error = print_document_now(
            MIXED,
            &host,
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
        assert_eq!(host.calls(), 0, "a refused mixed request must not render");
    }

    #[test]
    fn batch_uses_pdf_submit_when_the_backend_prints_pdf() {
        let sink = MemorySink::default();
        let backend = HtmlBackend::pdf_capable();
        let done = print_batch_now(
            vec![BatchPart::Document(MINIMAL.to_vec())],
            &FakeRenderHost::new(),
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap();
        assert_eq!(done.submitted.printer, "pdf-printer");
        assert_eq!(done.submitted.job_id, Some(1));
        assert_eq!(
            backend
                .pdf_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(
            backend
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
    }

    #[test]
    fn print_pdf_needs_a_pdf_capable_backend() {
        let host = FakeRenderHost::new();
        let pdf = futures::executor::block_on(
            host.render_to_pdf("<html></html>", &RenderSpec::new(210.0, 297.0)),
        )
        .unwrap();
        let pool = fake_pool(&host);
        let jobs = PrintJobs::new();
        let sink: Arc<dyn Sink> = Arc::new(MemorySink::default());
        let request = PrintPdf {
            pdf: pdf.clone(),
            printer: None,
            copies: 1,
            collate: true,
            title: "print-pdf".to_owned(),
        };
        let accepted = begin_print_pdf(
            request.clone(),
            &pool,
            Arc::new(HtmlBackend::pdf_capable()),
            &jobs,
            Arc::clone(&sink),
            Box::new(|_, _| {}),
            None,
            None,
        );
        assert!(accepted.is_ok());

        let Err(rejected) = begin_print_pdf(
            request,
            &pool,
            Arc::new(HtmlBackend::default()),
            &PrintJobs::new(),
            sink,
            Box::new(|_, _| {}),
            None,
            None,
        ) else {
            panic!("an HTML-only backend must refuse doc.printPdf");
        };
        assert_eq!(rejected.code(), "E_UNSUPPORTED");
    }

    #[test]
    fn print_pdf_rejects_a_zero_page_pdf() {
        let mut document = lopdf::Document::with_version("1.4");
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", lopdf::Object::Array(Vec::new()));
        pages.set("Count", 0i64);
        let pages_id = document.add_object(lopdf::Object::Dictionary(pages));
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference(pages_id));
        let catalog_id = document.add_object(lopdf::Object::Dictionary(catalog));
        document
            .trailer
            .set("Root", lopdf::Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        let pdf = bytes::Bytes::from(bytes);

        let host = FakeRenderHost::new();
        let pool = fake_pool(&host);
        let sink: Arc<dyn Sink> = Arc::new(MemorySink::default());
        let request = PrintPdf {
            pdf,
            printer: None,
            copies: 1,
            collate: true,
            title: "empty-pdf".to_owned(),
        };
        let Err(rejected) = begin_print_pdf(
            request,
            &pool,
            Arc::new(HtmlBackend::pdf_capable()),
            &PrintJobs::new(),
            sink,
            Box::new(|_, _| {}),
            None,
            None,
        ) else {
            panic!("a zero-page PDF must be rejected");
        };
        assert_eq!(rejected.code(), "E_SCHEMA");
        assert!(rejected.to_string().contains("no pages"));
        assert_eq!(rejected.pointer(), Some("/pdf/pages"));
        assert_eq!(
            rejected
                .detail()
                .and_then(|detail| detail["pointer"].as_str().map(str::to_owned)),
            Some("/pdf/pages".to_owned())
        );
    }

    #[test]
    fn begin_print_pdf_refuses_more_than_the_batch_page_ceiling() {
        let host = FakeRenderHost::new();
        let pool = fake_pool(&host);
        let request = PrintPdf {
            pdf: shaped_pdf(2001, false),
            printer: None,
            copies: 1,
            collate: true,
            title: "too-long".to_owned(),
        };
        let Err(rejected) = begin_print_pdf(
            request,
            &pool,
            Arc::new(HtmlBackend::pdf_capable()),
            &PrintJobs::new(),
            Arc::new(MemorySink::default()),
            Box::new(|_, _| {}),
            None,
            None,
        ) else {
            panic!("2001 pages must be rejected before reserving");
        };
        assert_eq!(rejected.code(), "E_TOO_LARGE");
        assert!(rejected.to_string().contains("2001 pages"));
        assert_eq!(host.calls(), 0, "nothing may render for a refused PDF");
    }

    fn shaped_pdf(pages: usize, acro_form: bool) -> bytes::Bytes {
        let mut document = lopdf::Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let mut kids = Vec::with_capacity(pages);
        for index in 0..pages {
            let content = document.add_object(lopdf::Stream::new(
                lopdf::Dictionary::new(),
                format!("BT /F1 12 Tf 72 720 Td (page {index}) Tj ET").into_bytes(),
            ));
            let mut page = lopdf::Dictionary::new();
            page.set("Type", "Page");
            page.set("Parent", pages_id);
            page.set(
                "MediaBox",
                lopdf::Object::Array(vec![0.into(), 0.into(), 595.into(), 842.into()]),
            );
            page.set("Contents", content);
            kids.push(document.add_object(page).into());
        }
        let mut pages_node = lopdf::Dictionary::new();
        pages_node.set("Type", "Pages");
        pages_node.set("Kids", lopdf::Object::Array(kids));
        pages_node.set("Count", i64::try_from(pages).unwrap());
        document
            .objects
            .insert(pages_id, lopdf::Object::Dictionary(pages_node));
        let catalog_id = document.add_object(lopdf::Object::Dictionary({
            let mut catalog = lopdf::Dictionary::new();
            catalog.set("Type", "Catalog");
            catalog.set("Pages", pages_id);
            if acro_form {
                let mut form = lopdf::Dictionary::new();
                form.set("Fields", lopdf::Object::Array(Vec::new()));
                catalog.set("AcroForm", form);
            }
            catalog
        }));
        document
            .trailer
            .set("Root", lopdf::Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes::Bytes::from(bytes)
    }

    #[test]
    fn a_single_pdf_is_submitted_byte_for_byte() {
        let pdf = shaped_pdf(3, true);
        let backend = FakePrinterBackend::new();
        let sink = NullSink;
        let done = print_pdf_now(
            pdf.clone(),
            &backend,
            &sink,
            None,
            2,
            "form".to_owned(),
            None,
        )
        .expect("a valid PDF prints");
        assert_eq!(done.pages, 3);
        assert_eq!(done.copies, 2);
        let submitted = backend.submitted();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].pdf, pdf, "the PDF must not be rebuilt");
        assert_eq!(submitted[0].copies, 2);
    }

    #[test]
    fn the_passthrough_limits_match_the_batch_ceiling() {
        assert!(check_pdf_passthrough_limits(MAX_BATCH_PAGES as u32, MAX_BATCH_PDF_BYTES).is_ok());
        let pages = check_pdf_passthrough_limits(MAX_BATCH_PAGES as u32 + 1, 1)
            .expect_err("one page past the batch ceiling");
        assert_eq!(pages.code(), "E_TOO_LARGE");
        assert_eq!(pages.pointer(), None);
        let bytes = check_pdf_passthrough_limits(1, MAX_BATCH_PDF_BYTES + 1)
            .expect_err("one byte past the batch ceiling");
        assert_eq!(bytes.code(), "E_TOO_LARGE");
        assert_eq!(bytes.pointer(), None);
    }

    /// A valid one-page PDF padded with an unreferenced stream so the file
    /// crosses `MAX_BATCH_PDF_BYTES`. The padded object is not referenced by
    /// the page tree, but its bytes still count toward the file ceiling.
    fn oversized_pdf(min_total_bytes: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(min_total_bytes + 4096);
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = [0usize; 5];
        offsets[1] = out.len();
        out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        offsets[2] = out.len();
        out.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        offsets[3] = out.len();
        out.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
              /Resources << >> >>\nendobj\n",
        );
        // Leave room for the xref table and trailer; the padding stream takes
        // the rest of the requested size.
        let tail_reserve = 1024;
        let target = min_total_bytes + tail_reserve;
        let body_len = target.saturating_sub(out.len() + 64);
        offsets[4] = out.len();
        out.extend_from_slice(b"4 0 obj\n<< /Length ");
        out.extend_from_slice(body_len.to_string().as_bytes());
        out.extend_from_slice(b" >>\nstream\n");
        out.resize(out.len() + body_len, b'A');
        out.extend_from_slice(b"\nendstream\nendobj\n");
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        out
    }

    /// Slow, memory-heavy counterpart to the arithmetic ceiling test: builds a
    /// real 131 MiB PDF and walks it through the passthrough path. Ignored by
    /// default; run with:
    /// `cargo test -p printkit-engine --lib -- --ignored a_real_131_mib_pdf_is_refused`
    /// Peak RSS is roughly two to three times the 131 MiB buffer (the input
    /// plus the PDF parser); measure with `/usr/bin/time -l` on macOS.
    #[test]
    #[ignore = "allocates and parses a 131 MiB PDF; run explicitly with --ignored"]
    fn a_real_131_mib_pdf_is_refused() {
        let started = std::time::Instant::now();
        let pdf = oversized_pdf(131 * 1024 * 1024);
        let bytes = pdf.len();
        assert!(
            bytes > MAX_BATCH_PDF_BYTES,
            "the generated PDF must exceed the ceiling: {bytes}"
        );
        let backend = FakePrinterBackend::new();
        let error = print_pdf_now(
            pdf.into(),
            &backend,
            &NullSink,
            None,
            1,
            "oversized".to_owned(),
            None,
        )
        .expect_err("a 131 MiB PDF must be refused");
        assert_eq!(error.code(), "E_TOO_LARGE");
        assert!(
            backend.submitted().is_empty(),
            "nothing may reach the spooler"
        );
        let elapsed = started.elapsed();
        eprintln!(
            "131 MiB PDF ({bytes} bytes): refused in {elapsed:?}; \
             peak RSS is roughly 2-3x the buffer"
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "131 MiB ceiling check took {elapsed:?}"
        );
    }

    #[test]
    fn a_2001_page_pdf_is_too_large() {
        let pdf = shaped_pdf(MAX_BATCH_PAGES + 1, false);
        let backend = FakePrinterBackend::new();
        let error = print_pdf_now(
            pdf,
            &backend,
            &NullSink,
            None,
            1,
            "too-long".to_owned(),
            None,
        )
        .expect_err("2001 pages must be refused");
        assert_eq!(error.code(), "E_TOO_LARGE");
        assert!(error.to_string().contains("2001 pages"), "{error}");
        assert!(
            backend.submitted().is_empty(),
            "nothing may reach the spooler"
        );
    }

    #[test]
    fn a_thousand_page_pdf_uses_the_passthrough_fast_path() {
        let pdf = shaped_pdf(1000, false);
        let backend = FakePrinterBackend::new();
        let sink = NullSink;
        let started = std::time::Instant::now();
        let done = print_pdf_now(
            pdf.clone(),
            &backend,
            &sink,
            None,
            1,
            "long".to_owned(),
            None,
        )
        .expect("a long PDF prints");
        let elapsed = started.elapsed();
        assert_eq!(done.pages, 1000);
        assert_eq!(backend.submitted()[0].pdf, pdf);
        assert!(
            elapsed < Duration::from_secs(2),
            "1000-page passthrough took {elapsed:?}"
        );
    }

    #[test]
    fn the_merged_pdf_byte_ceiling_is_rechecked() {
        assert!(check_merged_batch_bytes(MAX_BATCH_PDF_BYTES).is_ok());
        let error = check_merged_batch_bytes(MAX_BATCH_PDF_BYTES + 1)
            .expect_err("the merged output must be bounded");
        assert_eq!(error.code(), "E_TOO_LARGE");
    }

    #[test]
    fn a_multi_pdf_batch_parses_each_input_pdf_once() {
        let backend = HtmlBackend::pdf_capable();
        printkit_pdf::reset_parse_count();
        let done = print_batch_now(
            vec![
                BatchPart::Pdf(shaped_pdf(1, false)),
                BatchPart::Pdf(shaped_pdf(1, false)),
            ],
            &FakeRenderHost::new(),
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap();
        assert_eq!(done.pages, 2);
        assert_eq!(
            printkit_pdf::parse_count(),
            2,
            "each PDF page count must be parsed exactly once"
        );
    }

    #[test]
    fn begin_print_batch_parses_each_pdf_once() {
        let host: Arc<dyn RenderHost> = Arc::new(FakeRenderHost::new());
        let backend: Arc<dyn PrinterBackend> = Arc::new(HtmlBackend::pdf_capable());
        printkit_pdf::reset_parse_count();
        let _reserved = begin_print_batch(
            super::PrintBatch {
                parts: vec![
                    BatchPart::Pdf(shaped_pdf(1, false)),
                    BatchPart::Pdf(shaped_pdf(1, true)),
                ],
                printer: None,
                copies: 1,
                collate: true,
                title: "batch".to_owned(),
            },
            &fake_pool(&FakeRenderHost::new()),
            backend,
            Arc::clone(&host),
            &PrintJobs::new(),
            Arc::new(MemorySink::default()),
            FetchPolicy::default(),
            Box::new(|_, _| {}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            printkit_pdf::parse_count(),
            2,
            "each PDF must be parsed exactly once during validation"
        );
    }

    #[test]
    fn a_multi_pdf_batch_warns_when_metadata_is_dropped() {
        let backend = HtmlBackend::pdf_capable();
        let first = shaped_pdf(1, true);
        let second = shaped_pdf(1, true);
        let done = print_batch_now(
            vec![BatchPart::Pdf(first), BatchPart::Pdf(second)],
            &FakeRenderHost::new(),
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap();
        assert_eq!(done.pages, 2, "page content is preserved");
        assert!(
            done.warnings
                .iter()
                .any(|warning| warning.code == "W_PDF_MERGE_METADATA_LOST"),
            "{:?}",
            done.warnings
        );
        let jobs = backend.pdf_jobs.lock().unwrap();
        assert_eq!(printkit_pdf::page_count(&jobs[0].pdf).unwrap(), 2);
    }

    #[test]
    fn batch_copies_are_spooler_copies_not_per_document_copies() {
        let backend = HtmlBackend::pdf_capable();
        let bytes = MINIMAL.to_vec();
        let done = print_batch_now(
            vec![
                BatchPart::Document(bytes.clone()),
                BatchPart::Document(bytes),
            ],
            &FakeRenderHost::new(),
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            2,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap();
        assert_eq!(done.pages, 2, "each document contributes its pages once");
        assert_eq!(done.copies, 2, "the top-level copies reach the spooler");
        let jobs = backend.pdf_jobs.lock().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].copies, 2);
        assert_eq!(
            printkit_pdf::page_count(&jobs[0].pdf).unwrap(),
            2,
            "the merged PDF carries each page once"
        );
    }

    #[test]
    fn a_multi_pdf_batch_zero_page_points_at_the_document_index() {
        let error = print_batch_now(
            vec![
                BatchPart::Pdf(shaped_pdf(1, false)),
                BatchPart::Pdf(shaped_pdf(0, false)),
            ],
            &FakeRenderHost::new(),
            &HtmlBackend::pdf_capable(),
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .expect_err("the second PDF has no pages");
        assert_eq!(error.code(), "E_SCHEMA");
        assert_eq!(error.pointer(), Some("/documents/1"));
    }

    #[test]
    fn a_batch_part_with_document_copies_is_refused() {
        let backend = HtmlBackend::pdf_capable();
        for copies in [0_u32, 3] {
            let mut document: serde_json::Value = serde_json::from_slice(MINIMAL).unwrap();
            document["output"] = serde_json::json!({ "copies": copies });
            let bytes = serde_json::to_vec(&document).unwrap();
            let error = print_batch_now(
                vec![BatchPart::Document(bytes)],
                &FakeRenderHost::new(),
                &backend,
                &MemorySink::default(),
                &FetchPolicy::default(),
                None,
                1,
                true,
                "batch".to_owned(),
                None,
            )
            .expect_err("per-document copies must be refused");
            assert_eq!(error.code(), "E_SCHEMA");
            assert_eq!(error.pointer(), Some("/documents/0/output/copies"));
            assert!(
                backend.pdf_jobs.lock().unwrap().is_empty(),
                "a refused batch must not submit"
            );
        }
    }

    #[test]
    fn doc_print_still_honours_document_copies() {
        let backend = FakePrinterBackend::new();
        let mut document: serde_json::Value = serde_json::from_slice(MINIMAL).unwrap();
        document["output"] = serde_json::json!({ "copies": 3 });
        let bytes = serde_json::to_vec(&document).unwrap();
        let done = print_document_now(
            &bytes,
            &FakeRenderHost::new(),
            &backend,
            &MemorySink::default(),
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(done.copies, 3);
        assert_eq!(done.sheets(), 3);
        assert_eq!(backend.submitted()[0].copies, 3);
    }

    #[test]
    fn batch_refuses_a_backend_without_a_pdf_spooler_route() {
        let sink = MemorySink::default();
        let backend = HtmlBackend::default();
        let error = print_batch_now(
            vec![BatchPart::Document(MINIMAL.to_vec())],
            &FakeRenderHost::new(),
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            1,
            true,
            "batch".to_owned(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "E_UNSUPPORTED");
    }

    #[test]
    fn html_backend_receives_the_rendered_document_without_a_spooler_id() {
        let sink = MemorySink::default();
        let backend = HtmlBackend::default();
        let host = FakeRenderHost::new();
        let done = print_document_now(
            MINIMAL,
            &host,
            &backend,
            &sink,
            &FetchPolicy::default(),
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(done.submitted.job_id, None);
        assert_eq!(done.submitted.printer, "html-printer");
        // The HTML path must not render a PDF only to throw it away.
        assert_eq!(host.calls(), 0, "render_to_pdf ran on the HTML print path");
        let jobs = backend
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].html.contains("printkit"));
        assert_eq!(jobs[0].spec.width_mm, 210.0);
        assert_eq!(jobs[0].spec.height_mm, 297.0);

        // Render ran, submit ran, and the submit stage carries no fake id.
        let events = sink.events();
        let render = events
            .iter()
            .find(|event| event.stage == Stage::Render)
            .expect("render stage");
        assert!(render.fields.contains_key(&FieldKey::HtmlMs), "{render:?}");
        assert!(render.fields.contains_key(&FieldKey::PdfMs), "{render:?}");
        assert_eq!(render.fields.get(&FieldKey::PdfMs), Some(&Scalar::Int(0)));
        let submit = events
            .iter()
            .find(|event| event.stage == Stage::Submit)
            .expect("submit stage");
        assert!(!submit.fields.contains_key(&FieldKey::SpoolerJob));
    }

    #[cfg(feature = "url-fetch")]
    #[test]
    fn total_fetched_bytes_are_capped_per_document() {
        use std::io::{Read, Write};

        use crate::export_pdf;

        // Nine 4 MiB bodies pass the per-URL ceiling (5 MiB) but exceed the
        // 32 MiB per-document total, so the document must fail with E_TOO_LARGE.
        let body = "x".repeat(4 * 1024 * 1024);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..9 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        let items: Vec<String> = (0..9)
            .map(|index| {
                format!(
                    r#"{{"id":"f{index}","type":"html",
                    "frame":{{"x":20.0,"y":20.0,"width":100.0,"height":30.0}},
                    "data":{{"url":"http://127.0.0.1:{port}/{index}.html"}}}}"#
                )
            })
            .collect();
        let document = format!(
            r#"{{"version":1,"pages":[{{"spec":{{"size":"A4"}},"items":[{}]}}]}}"#,
            items.join(",")
        );
        let policy = FetchPolicy {
            timeout_ms: 5_000,
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let sink = MemorySink::default();
        let host = FakeRenderHost::new();
        let error =
            futures::executor::block_on(export_pdf(document.as_bytes(), &host, &sink, &policy))
                .unwrap_err();
        assert_eq!(error.code(), "E_TOO_LARGE");
        assert_eq!(host.calls(), 0);
        server.join().unwrap();
    }
}
