#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! Application facade.
//!
//! The CLI calls this crate and nothing else; the facade is the one place that
//! knows which platform implementation exists. Everything below it is
//! platform-neutral.
//!
//! The facade refuses to compile on hosts without a platform implementation
//! instead of failing at runtime: a binary that can only print nothing is worse
//! than no binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use printkit_engine::{
    BatchPart, CancelDisposition, EngineError, ExportPathError, ExportResult, ExportedFile,
    FetchPolicy, HostFactory, JobFailure, JobFinishHook, JobHooks, JobId, JobOutcome, JobPhase,
    JobPriority, JobStatus, JobUpdate, JobUpdateHook, MAX_FETCH_BYTES, PoolConfig, PreparedRender,
    PreviewRender, PrintBatch, PrintControl, PrintDone, PrintJobs, PrintPdf, QueueTiming, RawDone,
    RawSend, RenderFailure, RenderPool, RenderReport, Reservation, ReserveError, ReservedPrint,
    ReservedPrintJob, UsageRecorder, Warning, begin_print_batch, begin_print_pdf, begin_raw_send,
    export_pdf_to_path, limits, prepare_preview, print_batch_now, print_document_now,
    print_pdf_now, raw_send_now, reserve_print, trace_preview_refusal,
};
pub use printkit_platform::{
    HtmlPrintJob, PaperInfo, PlatformError, PreviewActions, PreviewRequest, PrintJob,
    PrinterBackend, PrinterInfo, RawJob, RenderHost, RenderSpec, SubmittedJob, WebviewInfo,
    WindowId,
};
pub use printkit_trace::{
    FieldKey, JsonlSink, LogLevel, MemorySink, NullSink, RotateOnOpen, Scalar, Sink, Stage,
    TraceEvent, TraceId, Tracer, earliest_seq, events_since, max_seq_in_dir, read_events,
};

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("printkit supports macOS and Windows");

#[cfg(feature = "fake-render")]
use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
#[cfg(all(target_os = "macos", not(feature = "fake-render")))]
use printkit_platform_mac::MacPrinterBackend;
#[cfg(target_os = "macos")]
use printkit_platform_mac::MacRenderHost;
#[cfg(all(windows, not(feature = "fake-render")))]
use printkit_platform_win::WinPrinterBackend;
#[cfg(windows)]
use printkit_platform_win::WinRenderHost;

/// Directory holding JSONL log files.
pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Export one document to PDF, tracing each stage.
///
/// Blocking on purpose: the CLI is a one-shot process, and the renderer blocks
/// its thread on the platform loop anyway.
pub fn export_pdf(
    input: &[u8],
    sink: &dyn Sink,
    fetch: &FetchPolicy,
) -> Result<ExportResult, EngineError> {
    #[cfg(target_os = "macos")]
    let host = MacRenderHost::new();
    #[cfg(windows)]
    let host = WinRenderHost::new();
    futures::executor::block_on(printkit_engine::export_pdf(input, &host, sink, fetch))
}

/// Renderer identity for `printkit doctor`.
pub fn webview_info() -> WebviewInfo {
    #[cfg(target_os = "macos")]
    {
        printkit_platform_mac::webview_info()
    }
    #[cfg(windows)]
    {
        printkit_platform_win::webview_info()
    }
}

/// Keep the process main loop alive while `keep_running` returns true.
///
/// macOS renders on the main queue; Windows delivers WebView2 callbacks as
/// posted messages. Both need this loop pumped by the main thread, or every
/// render waits for a queue that never drains.
pub fn run_main_loop_while(keep_running: impl FnMut() -> bool) {
    let mut keep_running = keep_running;
    #[cfg(target_os = "macos")]
    printkit_platform_mac::run_main_loop_while(&mut keep_running);
    #[cfg(windows)]
    printkit_platform_win::run_main_loop_while(&mut keep_running);
}

/// Build the service's render pool.
///
/// `fake-render` replaces the renderer with the test double: mock prints and
/// crash recovery then run without a GUI main loop, which is what the
/// acceptance scripts need.
pub fn new_render_pool(
    workers: u32,
    queue_max: u32,
    queue_timeout: Duration,
    warmup: bool,
) -> Arc<RenderPool> {
    #[cfg(feature = "fake-render")]
    let factory: HostFactory = Arc::new(|| Arc::new(FakeRenderHost::new()) as Arc<dyn RenderHost>);
    #[cfg(all(not(feature = "fake-render"), target_os = "macos"))]
    let factory: HostFactory = Arc::new(|| Arc::new(MacRenderHost::new()) as Arc<dyn RenderHost>);
    #[cfg(all(not(feature = "fake-render"), windows))]
    let factory: HostFactory = Arc::new(|| Arc::new(WinRenderHost::new()) as Arc<dyn RenderHost>);
    RenderPool::new(
        PoolConfig::new(workers, queue_max, queue_timeout).with_warmup(warmup),
        factory,
    )
}

/// Destroy every preview window when the service stops.
#[cfg(all(not(feature = "fake-render"), target_os = "macos"))]
pub fn close_all_previews() {
    printkit_platform_mac::close_all_previews();
}

/// Windows previews land in W1; `platform-win` already has the shutdown hook.
#[cfg(all(not(feature = "fake-render"), windows))]
pub fn close_all_previews() {
    printkit_platform_win::close_all_previews();
}

/// The fake host owns no GUI windows.
#[cfg(feature = "fake-render")]
pub fn close_all_previews() {}

/// One renderer instance for callers that render outside the pool, such as a
/// batch that assembles several documents into one spooler job.
pub fn new_render_host() -> Arc<dyn RenderHost> {
    #[cfg(feature = "fake-render")]
    {
        Arc::new(FakeRenderHost::new())
    }
    #[cfg(all(not(feature = "fake-render"), target_os = "macos"))]
    {
        Arc::new(MacRenderHost::new())
    }
    #[cfg(all(not(feature = "fake-render"), windows))]
    {
        Arc::new(WinRenderHost::new())
    }
}

/// The operating-system print system.
///
/// `fake-render` builds record submissions instead of touching a spooler, so
/// the mock print script still runs without a printer.
pub fn new_printer_backend() -> Arc<dyn PrinterBackend> {
    #[cfg(feature = "fake-render")]
    {
        Arc::new(FakePrinterBackend::new())
    }
    #[cfg(all(not(feature = "fake-render"), target_os = "macos"))]
    {
        Arc::new(MacPrinterBackend::new())
    }
    #[cfg(all(not(feature = "fake-render"), windows))]
    {
        Arc::new(WinPrinterBackend::new())
    }
}
