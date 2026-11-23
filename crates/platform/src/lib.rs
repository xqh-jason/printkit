#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! Platform abstractions. Traits and plain types only - the implementations
//! live in `platform-mac` and `platform-win`.
//!
//! `RenderHost` is a dyn trait rather than generic: the service stores a
//! renderer in shared state and tests substitute the fake host from the
//! `test-support` feature without a WebView. This is one of the few places
//! where dynamic dispatch is justified; the rest of the workspace stays
//! concrete.
//!
//! This crate depends on no internal crate: paper dimensions arrive as plain
//! millimetres in [`RenderSpec`]. The platform layer never sees a `Document`;
//! it receives HTML plus a paper spec and produces PDF bytes. Printing has two
//! shapes: [`PrintJob`] carries PDF bytes and spooler options, [`HtmlPrintJob`]
//! carries the same HTML and spec for platforms whose renderer can print to a
//! driver directly (Windows WebView2 `Print`), and [`PrinterBackend`] hands
//! either one to the operating system's print system.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

#[cfg(feature = "test-support")]
pub mod test_support;

/// Paper size for one rendered page, in millimetres.
///
/// `width_mm` and `height_mm` describe the paper, not the stacked web content:
/// a renderer lays every page box one below another and captures each page
/// rectangle separately. `pages` is the number of rectangles, and `page_sizes`
/// overrides the uniform pair when one document mixes paper sizes.
///
/// # Examples
///
/// ```
/// use printkit_platform::{MM_TO_PT, RenderSpec};
///
/// let a4 = RenderSpec::new(210.0, 297.0);
/// assert_eq!((a4.width_mm, a4.height_mm), (210.0, 297.0));
/// assert_eq!(a4.pages, 1);
/// assert_eq!(a4.size_mm(0), (210.0, 297.0));
/// assert!((a4.width_pt() - 210.0 * MM_TO_PT).abs() < 1e-9);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    /// Paper width of one page in millimetres; `210.0` is A4 portrait.
    pub width_mm: f64,
    /// Paper height of one page in millimetres; `297.0` is A4 portrait.
    pub height_mm: f64,
    /// Paper pages in the HTML. A renderer that captures the web content by
    /// page rectangle needs this to cover every page, including table
    /// continuations.
    pub pages: usize,
    /// Size of each paper page in millimetres, in layout order. Empty means
    /// every page uses `width_mm` × `height_mm`. Table continuations repeat
    /// their document page's size, so one document may mix paper sizes.
    ///
    /// The invariant: a renderer captures page `i` at `size_mm(i)`, never at
    /// the uniform pair. [`RenderSpec::with_page_sizes`] keeps `pages` equal to
    /// the list length; a shorter list falls back to the uniform pair.
    pub page_sizes: Vec<(f64, f64)>,
}

/// 1 mm in PostScript points (1 pt = 1/72 inch), the unit
/// `WKPDFConfiguration` expects.
pub const MM_TO_PT: f64 = 72.0 / 25.4;

/// CSS lays content out at 96 dpi while PDF uses 72 dpi points; a host that
/// feeds CSS pixels to `createPDF` must zoom by this factor to keep geometry.
pub const CSS_PX_TO_PT: f64 = 72.0 / 96.0;

/// CSS lays content out at 96 dpi; one CSS pixel is this many millimetres.
pub const CSS_PX_TO_MM: f64 = 25.4 / 96.0;

/// Measured height of each table row in millimetres, keyed by `data-row-key`.
///
/// The renderer writes the key on every `<tr>`; the host fills this map from a
/// layout pass, and the engine re-renders with real heights so page breaks
/// never rely on an estimate.
pub type RowHeights = BTreeMap<String, f64>;

impl RenderSpec {
    /// A spec for one page of `width_mm` × `height_mm` with no per-page sizes.
    pub fn new(width_mm: f64, height_mm: f64) -> Self {
        Self {
            width_mm,
            height_mm,
            pages: 1,
            page_sizes: Vec::new(),
        }
    }

    /// Set the number of paper pages the HTML covers.
    pub fn with_pages(mut self, pages: usize) -> Self {
        self.pages = pages.max(1);
        self
    }

    /// Set one size per paper page (millimetres). The count becomes `pages`.
    pub fn with_page_sizes(mut self, page_sizes: Vec<(f64, f64)>) -> Self {
        self.pages = page_sizes.len().max(1);
        self.page_sizes = page_sizes;
        self
    }

    /// Size of paper page `page` (zero-based) in millimetres.
    pub fn size_mm(&self, page: usize) -> (f64, f64) {
        self.page_sizes
            .get(page)
            .copied()
            .unwrap_or((self.width_mm, self.height_mm))
    }

    /// Vertical offset of paper page `page` in the stacked layout, millimetres.
    pub fn offset_mm(&self, page: usize) -> f64 {
        (0..page.min(self.pages))
            .map(|index| self.size_mm(index).1)
            .sum()
    }

    /// Width the stacked layout needs: the widest page.
    pub fn content_width_mm(&self) -> f64 {
        (0..self.pages)
            .map(|index| self.size_mm(index).0)
            .fold(self.width_mm, f64::max)
    }

    /// Height the stacked layout needs: every page, one after another.
    pub fn content_height_mm(&self) -> f64 {
        (0..self.pages).map(|index| self.size_mm(index).1).sum()
    }

    /// Width in PostScript points, the unit `WKPDFConfiguration` expects.
    pub fn width_pt(&self) -> f64 {
        self.width_mm * MM_TO_PT
    }
    /// Height in PostScript points.
    pub fn height_pt(&self) -> f64 {
        self.height_mm * MM_TO_PT
    }
}

/// What the platform layer knows about its renderer, for `printkit doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebviewInfo {
    /// Renderer family, for example `WKWebView`.
    pub engine: String,
    /// Whether the renderer can be instantiated here: `false` when the runtime
    /// is missing, the reported version is below the minimum, or this session
    /// has no window server.
    pub available: bool,
    /// Operating-system version that supplies the renderer, when known.
    pub version: Option<String>,
}

/// Every way a platform renderer or print backend can fail.
///
/// A call that cannot finish reports one of these variants; no implementation
/// returns placeholder output or a fake success.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The platform renderer is not usable here (for example no GUI session).
    #[error("renderer unavailable: {0}")]
    Unavailable(String),
    /// The renderer did not finish before the deadline. Never return blank
    /// pages instead of this error.
    #[error("render timed out after {0} ms")]
    Timeout(u64),
    /// WebView reported a failure.
    #[error("render failed: {0}")]
    Render(String),
    /// The capability does not exist on this platform.
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    /// The operating system has no printer with that name.
    #[error("printer not found: {0}")]
    PrinterNotFound(String),
    /// The operating system has no default printer.
    #[error("no default printer is configured")]
    NoDefaultPrinter,
    /// The spooler refused the job.
    #[error("print failed: {0}")]
    Print(String),
    /// The job already reached the spooler and this platform cannot retract
    /// it. Windows graphic printing has no spooler job id.
    #[error("this platform cannot cancel a submitted job")]
    CancelUnsupported,
    /// The destination does not accept raw device bytes.
    #[error("the printer does not accept raw bytes: {0}")]
    RawUnsupported(String),
    /// The spooler no longer has the job: it finished, was cancelled elsewhere,
    /// or the queue was removed. Retrying cannot succeed, but the platform can
    /// cancel other jobs, so this is not [`Self::CancelUnsupported`].
    #[error("spooler job {job} on {printer} is gone: {detail}")]
    JobNotFound {
        /// Name the spooler filed the job under.
        printer: String,
        /// Spooler job number that the queue no longer holds.
        job: u32,
        /// Spooler text that separates "finished", "not found" and "removed".
        detail: String,
    },
    /// No preview window matches the handle: it was closed or never existed.
    #[error("preview window not found: {0}")]
    WindowNotFound(String),
}

/// Identity of one preview window inside a service instance.
///
/// The service hands the number to the client as an opaque `windowId`; the
/// platform layer only needs to find the native window again. Numbers are
/// never reused, so a closed handle stays invalid instead of addressing a
/// newer window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(u64);

impl WindowId {
    /// Wrap a raw handle value, as a host does when it assigns a new window.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value, the same number the client receives as `windowId`.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One window to show or refresh.
#[derive(Clone)]
pub struct PreviewRequest {
    /// Refresh this window instead of opening another one.
    pub window: Option<WindowId>,
    /// The exact HTML the print path renders. Preview must not build its own.
    pub html: String,
    /// Paper geometry the preview lays out; the same spec the print path uses.
    pub spec: RenderSpec,
    /// Window title, already resolved from the document metadata.
    pub title: String,
    /// Callbacks the window invokes when the user prints or closes it.
    pub actions: Arc<dyn PreviewActions>,
}

/// Controls a preview window offers back to the service.
///
/// Called on the GUI thread. Implementations must return promptly and move the
/// work to their own thread: a blocked GUI thread freezes every window.
pub trait PreviewActions: Send + Sync {
    /// The window's Print control was used. The service must route this through
    /// the same job path as `doc.print`.
    fn print(&self, window: WindowId);
    /// The user closed the window with the title-bar button.
    fn closed(&self, window: WindowId);
}

/// One submission to the operating-system print system.
///
/// # Examples
///
/// ```
/// use bytes::Bytes;
/// use printkit_platform::PrintJob;
///
/// let job = PrintJob {
///     printer: None,
///     pdf: Bytes::from_static(b"%PDF-1.4"),
///     copies: 1,
///     collate: true,
///     title: "invoice".to_owned(),
///     paper_mm: Some((210.0, 297.0)),
///     duplex: false,
/// };
/// assert_eq!(job.copies, 1);
/// assert!(job.printer.is_none());
/// ```
#[derive(Debug, Clone)]
pub struct PrintJob {
    /// Printer name; `None` means the system default printer.
    pub printer: Option<String>,
    /// A complete PDF, already merged and self-contained.
    pub pdf: Bytes,
    /// Copies requested for the whole job. The spooler repeats the document,
    /// so one submission is one spooler job regardless of this number.
    pub copies: u32,
    /// Whether copies are collated as complete sets.
    pub collate: bool,
    /// Title shown in the spooler queue.
    pub title: String,
    /// Paper size in millimetres when every page shares it. `None` lets the
    /// driver pick its default.
    pub paper_mm: Option<(f64, f64)>,
    /// Two-sided printing. The engine rejects a `true` value unless
    /// [`PrinterBackend::duplex_supported`] is true; both current backends
    /// report false until a physical duplex device is verified.
    pub duplex: bool,
}

/// One HTML submission for a platform whose renderer prints to a driver.
///
/// Windows WebView2 `Print` sends the page to the driver without a spooler job
/// number, so [`SubmittedJob::job_id`] is `None` on that path. The HTML and
/// [`RenderSpec`] are the exact pair the renderer already accepted; a backend
/// must load this HTML itself and must not re-serialise the document.
#[derive(Debug, Clone)]
pub struct HtmlPrintJob {
    /// Printer name; `None` means the system default printer.
    pub printer: Option<String>,
    /// Copies requested for the whole job; forwarded to the renderer's print
    /// settings.
    pub copies: u32,
    /// Whether copies are collated as complete sets.
    pub collate: bool,
    /// Title shown in the print queue and the print dialog.
    pub title: String,
    /// Paper size in millimetres when every page shares it.
    pub paper_mm: Option<(f64, f64)>,
    /// Two-sided printing; same gate as [`PrintJob::duplex`].
    pub duplex: bool,
    /// The HTML the render path produced.
    pub html: String,
    /// The same paper geometry the render path used.
    pub spec: RenderSpec,
}

/// One raw submission: device bytes that must reach the printer unchanged.
/// Raw bytes carry a device's own command language (ESC/POS, TSPL, ZPL, CPCL).
/// The platform layer must hand them to the spooler unchanged; rendering them
/// as a document would turn a label into pages of control codes.
#[derive(Debug, Clone)]
pub struct RawJob {
    /// Printer name; `None` means the system default printer.
    pub printer: Option<String>,
    /// Device command bytes, handed to the spooler unchanged.
    pub bytes: Bytes,
    /// Title shown in the spooler queue.
    pub title: String,
}

/// The spooler's receipt for one submitted job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedJob {
    /// Spooler job number, valid until the job leaves the queue. `None` when
    /// the platform handed the document to a renderer that produces no job id
    /// (Windows WebView2 `Print`); cancellation is then unsupported.
    pub job_id: Option<u32>,
    /// Name the spooler filed the job under. Resolved from the default when
    /// [`PrintJob::printer`] was `None`.
    pub printer: String,
}

/// One printer as the operating system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterInfo {
    /// Printer name as the operating system reports it; this is the name
    /// [`PrinterBackend::submit`] resolves.
    pub name: String,
    /// Whether the system default printer points at this entry.
    pub is_default: bool,
    /// MIME types the driver advertises.
    pub mime_types: Vec<String>,
    /// The driver accepts two-sided jobs.
    pub duplex: bool,
    /// The driver accepts raw bytes, not just rendered PDF.
    pub raw: bool,
    /// The driver exposes a paper-source selection.
    pub paper_source: bool,
}

/// One paper size a printer accepts.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperInfo {
    /// Driver-reported size name, for example `iso_a4_210x297mm` or a
    /// user-defined name.
    pub name: String,
    /// Paper width in millimetres.
    pub width_mm: f64,
    /// Paper height in millimetres.
    pub height_mm: f64,
    /// `true` for A3/A4/A5/Letter and other named sizes, `false` for
    /// driver-defined custom dimensions.
    pub named: bool,
}

/// Submit PDF bytes to the operating-system print system.
///
/// The trait is synchronous: every implementation blocks in FFI, and callers
/// run it on a render worker or a blocking task. Listing and submission are
/// separate calls so a caller can validate the printer before rendering.
pub trait PrinterBackend: Send + Sync {
    /// Every configured printer, default first when the system reports one.
    fn printers(&self) -> Result<Vec<PrinterInfo>, PlatformError>;
    /// Paper sizes the named printer accepts.
    fn papers(&self, printer: &str) -> Result<Vec<PaperInfo>, PlatformError>;
    /// Hand one PDF job to the spooler. Returns the spooler's job number; an
    /// implementation that cannot produce one must return `job_id: None`
    /// instead of a synthetic id, because cancellation would then be a lie.
    fn submit(&self, job: &PrintJob) -> Result<SubmittedJob, PlatformError>;
    /// Whether [`Self::submit`] can hand PDF bytes to the OS print system.
    ///
    /// `false` makes `doc.printPdf` and `doc.printBatch` refuse: there is no
    /// honest way to spool a PDF the platform cannot print. macOS and Windows
    /// both implement this (Windows through the WebView2 PDF viewer).
    fn prints_pdf(&self) -> bool {
        false
    }

    /// Whether [`Self::submit_html`] can print on this platform.
    ///
    /// `false` keeps the engine on the PDF path, so a host that only knows
    /// `createPDF` never sees [`HtmlPrintJob`].
    fn prints_html(&self) -> bool {
        false
    }
    /// Print HTML directly through the platform renderer.
    ///
    /// Used only when [`Self::prints_html`] is `true`. The implementation must
    /// load exactly this HTML with this [`RenderSpec`]; the returned job has
    /// no spooler id on platforms that print without one.
    fn submit_html(&self, job: &HtmlPrintJob) -> Result<SubmittedJob, PlatformError> {
        let _ = job;
        Err(PlatformError::Unsupported(
            "this platform cannot print HTML directly".to_owned(),
        ))
    }
    /// Hand raw device bytes to the spooler without rendering.
    ///
    /// Implementations must refuse with [`PlatformError::RawUnsupported`] when
    /// the destination cannot take raw bytes. Falling back to graphic printing
    /// is forbidden: it would submit the device commands as a document.
    fn submit_raw(&self, job: &RawJob) -> Result<SubmittedJob, PlatformError>;
    /// Retract a job that already reached the spooler.
    ///
    /// A job with `job_id: None` never had a spooler id, so implementations
    /// return [`PlatformError::CancelUnsupported`] before touching the spooler.
    /// Raw jobs always carry an id and can be retracted through the same call.
    fn cancel_submitted(&self, job: &SubmittedJob) -> Result<(), PlatformError>;
    /// Whether [`Self::cancel_submitted`] can retract a **graphic/PDF** job.
    ///
    /// Windows WebView2 `Print` has no spooler id, so this is `false` there
    /// even though raw jobs (which do have ids) can still be cancelled. The
    /// service reports the two cases as `cancelSubmitted` and `cancelRaw`.
    fn cancel_submitted_supported(&self) -> bool;
    /// Whether [`Self::cancel_submitted`] can retract a **raw** job.
    ///
    /// Raw jobs carry a real spooler id on both current backends, so the
    /// default is `true`; an implementation without a raw cancel must override.
    fn cancel_raw_supported(&self) -> bool {
        true
    }
    /// Whether this backend can honour `PrintJob.duplex` / `HtmlPrintJob.duplex`.
    ///
    /// `false` while no physical duplex device has been verified: the engine
    /// rejects a duplex request instead of silently printing simplex.
    fn duplex_supported(&self) -> bool {
        false
    }
}

/// "HTML to PDF bytes" for one renderer instance.
///
/// An instance renders one job at a time; concurrency across instances belongs
/// to the engine's render pool, not this trait.
#[async_trait]
pub trait RenderHost: Send + Sync {
    /// Render `html` into one self-contained PDF covering every page of `spec`.
    ///
    /// `html` is the exact string the render crate produced for this spec; the
    /// host loads it as-is and must not re-serialise the document. The returned
    /// [`Bytes`] belong to the caller, which spools them or merges them into a
    /// batch; the host keeps no reference and may not reuse a cached buffer.
    ///
    /// Errors: [`PlatformError::Unavailable`] when the renderer cannot run in
    /// this session, [`PlatformError::Timeout`] when navigation or a page
    /// capture exceeds the host's budget (blank pages are never a substitute),
    /// and [`PlatformError::Render`] when the renderer reports a failure such
    /// as a failed navigation or an unmergeable page PDF.
    async fn render_to_pdf(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError>;

    /// Measure every table row in `html` after one layout pass.
    ///
    /// A host that cannot measure returns [`PlatformError::Unsupported`] and the
    /// engine falls back to estimated heights with a warning; silence would
    /// hide the risk of a page break landing on the wrong row.
    async fn measure_rows(
        &self,
        _html: &str,
        _spec: &RenderSpec,
    ) -> Result<RowHeights, PlatformError> {
        Err(PlatformError::Unsupported(
            "row measurement is not available for this renderer".to_owned(),
        ))
    }

    /// Whether this process can open a visible window at all.
    ///
    /// A server session without a window server reports `false`; the service
    /// then advertises `capabilities.preview = false` and answers
    /// `E_UNSUPPORTED` instead of failing halfway through window creation.
    fn preview_available(&self) -> bool;

    /// Open or refresh one preview window and wait until its document loaded.
    ///
    /// [`PreviewRequest::window`] names a live window to refresh; every other
    /// call opens a new one. The platform assigns the id of a new window.
    async fn show_window(&self, request: PreviewRequest) -> Result<WindowId, PlatformError>;

    /// Capture one paper page of a live window as PNG.
    ///
    /// The image comes from the window's own web view, so the export cannot
    /// drift from what the user sees. `dpi` is the requested pixel density of
    /// the paper page.
    async fn window_png(
        &self,
        window: WindowId,
        page: u32,
        dpi: u32,
    ) -> Result<Bytes, PlatformError>;

    /// Destroy one preview window. Closing an unknown handle is
    /// [`PlatformError::WindowNotFound`], never a silent success.
    async fn close_window(&self, window: WindowId) -> Result<(), PlatformError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_page_sizes_stack_without_a_uniform_stride() {
        let spec =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (148.0, 210.0)]);
        assert_eq!(spec.pages, 2);
        assert_eq!(spec.size_mm(1), (148.0, 210.0));
        assert!((spec.offset_mm(1) - 297.0).abs() < 1e-9);
        assert!((spec.content_height_mm() - 507.0).abs() < 1e-9);
        assert!((spec.content_width_mm() - 210.0).abs() < 1e-9);
    }

    #[test]
    fn millimetres_convert_to_points() {
        let a4 = RenderSpec::new(210.0, 297.0);
        assert!((a4.width_pt() - 595.275_590_551_181_2).abs() < 1e-9);
        assert!((a4.height_pt() - 841.889_763_779_527_7).abs() < 1e-9);
    }
}
