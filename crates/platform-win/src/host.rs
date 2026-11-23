//! WebView2 rendering on a dedicated STA thread.
//!
//! Three constraints shape this module:
//!
//! 1. WebView2 needs a COM single-threaded apartment, and every controller call
//!    and callback happens on the thread that created it. Each render therefore
//!    gets its own STA thread, message-only window and controller; the render
//!    pool already serialises jobs per host, so threads are not shared.
//! 2. Callbacks only run while that thread pumps messages. Every wait below is
//!    a `PeekMessage`/`DispatchMessage` loop with a deadline, never a bare
//!    `recv`; a stopped pump would hang navigation and printing forever.
//! 3. `PrintToPdfStream` is the only PDF path that does not write a temporary
//!    file. It needs `ICoreWebView2_16` (runtime 1.0.1518.46), from which the
//!    millimetre geometry is derived: `PageWidth`/`PageHeight` are inches and
//!    the HTML already lays pages out at 96 dpi, so margins stay zero and the
//!    backgrounds stay on.

use std::ffi::c_void;
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use printkit_platform::{
    CSS_PX_TO_MM, PlatformError, PreviewRequest, RenderHost, RenderSpec, RowHeights, WebviewInfo,
    WindowId,
};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_PRINT_COLLATION_COLLATED, COREWEBVIEW2_PRINT_COLLATION_UNCOLLATED,
    COREWEBVIEW2_PRINT_DUPLEX, COREWEBVIEW2_PRINT_DUPLEX_ONE_SIDED,
    COREWEBVIEW2_PRINT_DUPLEX_TWO_SIDED_LONG_EDGE, COREWEBVIEW2_PRINT_MEDIA_SIZE_CUSTOM,
    COREWEBVIEW2_PRINT_ORIENTATION_LANDSCAPE, COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT,
    COREWEBVIEW2_PRINT_STATUS, COREWEBVIEW2_PRINT_STATUS_PRINTER_UNAVAILABLE,
    COREWEBVIEW2_PRINT_STATUS_SUCCEEDED, CompareBrowserVersions,
    CreateCoreWebView2EnvironmentWithOptions, GetAvailableCoreWebView2BrowserVersionString,
    ICoreWebView2, ICoreWebView2_16, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2Environment6, ICoreWebView2EnvironmentOptions, ICoreWebView2PrintSettings,
    ICoreWebView2PrintSettings2,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, PrintCompletedHandler,
    PrintToPdfStreamCompletedHandler,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IStream,
    STREAM_SEEK_SET,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW,
    SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, HSTRING, Interface, PCWSTR, PWSTR, w};

/// Lowest WebView2 Runtime that exposes `ICoreWebView2_16`.
pub const MIN_RUNTIME_VERSION: &str = "1.0.1518.46";

/// Budget for one navigation or PDF capture.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for handing a document to a printer driver. A hundred-page job is a
/// spooler upload, not a layout pass, so it gets more room than a PDF capture.
pub(crate) const PRINT_TIMEOUT: Duration = Duration::from_secs(120);

/// Extra wait for the STA thread after its own deadline expired.
///
/// The inner deadline is authoritative. This only catches a thread that stopped
/// pumping, so a job is never abandoned while it is still on schedule.
const THREAD_GRACE: Duration = Duration::from_secs(5);

/// Windows renderer: one WebView2 instance per render, on its own STA thread.
#[derive(Debug, Clone)]
pub struct WinRenderHost {
    timeout: Duration,
}

impl WinRenderHost {
    /// A host with the default ten-second budget for one navigation or one PDF
    /// capture.
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// One WebView2 `PrintToPdfStream` for a single paper size.
    async fn render_uniform(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        let html = html.to_owned();
        let spec = spec.clone();
        let timeout = self.timeout;
        let expected_pages = spec.pages.max(1);
        run_on_sta_thread(timeout, move |deadline| {
            let session = StaRender::open(&html, &spec, deadline, timeout, false)?;
            let pdf = session.print(&spec)?;
            let actual = printkit_pdf::page_count(&pdf).map_err(|error| {
                PlatformError::Render(format!("cannot count printed pages: {error}"))
            })?;
            check_page_count(actual, expected_pages)?;
            Ok(pdf)
        })
    }

    /// Mixed paper: one WebView2 render per page, then concatenate.
    ///
    /// `PrintToPdfStream` accepts one page size per operation. Each page's HTML
    /// is narrowed to its own `.page` box, rendered with that page's paper size,
    /// and the resulting single-page PDFs are merged. A page-count mismatch is
    /// an error, never a silently short document.
    async fn render_mixed(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        let page_count = spec.pages.max(1);
        let html = html.to_owned();
        let spec = spec.clone();
        let timeout = self.timeout;
        run_on_sta_thread(timeout, move |deadline| {
            // Reuse one WebView2 environment/controller for every page of a
            // mixed-paper document. Creating one environment per page was both
            // slow and memory-heavy.
            let first = RenderSpec::new(spec.size_mm(0).0, spec.size_mm(0).1);
            let session = StaRender::open_bare(&first, deadline, timeout, false, true)?;
            let mut pdfs = Vec::with_capacity(page_count);
            for page in 0..page_count {
                let (width_mm, height_mm) = spec.size_mm(page);
                let page_html = page_only_html(&html, page, (width_mm, height_mm));
                navigate(&session.core, &page_html, deadline, timeout)?;
                let page_spec = RenderSpec {
                    width_mm,
                    height_mm,
                    pages: 1,
                    page_sizes: Vec::new(),
                };
                pdfs.push(session.print(&page_spec)?);
            }
            let (merged, pages) = printkit_pdf::concat(&pdfs).map_err(|error| {
                PlatformError::Render(format!("cannot merge mixed-paper PDFs: {error}"))
            })?;
            check_page_count(pages, page_count)?;
            Ok(merged)
        })
    }
}

impl Default for WinRenderHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RenderHost for WinRenderHost {
    async fn render_to_pdf(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        if has_mixed_paper(spec) {
            return self.render_mixed(html, spec).await;
        }
        self.render_uniform(html, spec).await
    }

    async fn measure_rows(
        &self,
        html: &str,
        spec: &RenderSpec,
    ) -> Result<RowHeights, PlatformError> {
        let html = html.to_owned();
        let spec = spec.clone();
        let timeout = self.timeout;
        run_on_sta_thread(timeout, move |deadline| {
            // Measurement needs the host script even though page scripts stay
            // off for rendering; the sanitised HTML carries no scripts either.
            let session = StaRender::open(&html, &spec, deadline, timeout, true)?;
            session.measure()
        })
    }

    fn preview_available(&self) -> bool {
        crate::preview::available()
    }

    async fn show_window(&self, request: PreviewRequest) -> Result<WindowId, PlatformError> {
        crate::preview::show_window(request, self.timeout)
    }

    async fn window_png(
        &self,
        window: WindowId,
        page: u32,
        dpi: u32,
    ) -> Result<Bytes, PlatformError> {
        crate::preview::window_png(window, page, dpi)
    }

    async fn close_window(&self, window: WindowId) -> Result<(), PlatformError> {
        crate::preview::close_window(window)
    }
}

/// Run `work` on a fresh STA thread and wait for it.
///
/// The thread owns every COM object; the caller only receives the result. A
/// thread that ignores its deadline cannot be killed, so it is detached and the
/// caller gets [`PlatformError::Timeout`], never a blank PDF.
fn run_on_sta_thread<T, F>(timeout: Duration, work: F) -> Result<T, PlatformError>
where
    T: Send + 'static,
    F: FnOnce(Instant) -> Result<T, PlatformError> + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("printkit-webview2".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + timeout;
            // A panic in WebView2 code must not look like a timeout: the
            // caller would retry a job that actually crashed the host.
            let result = catch_host_panic(|| {
                // The apartment must outlive every COM object the work creates.
                ComApartment::init().and_then(|_apartment| work(deadline))
            });
            let _ = sender.send(result);
        })
        .map_err(|error| {
            PlatformError::Unavailable(format!("cannot start the WebView2 thread: {error}"))
        })?;

    match receiver.recv_timeout(timeout + THREAD_GRACE) {
        Ok(result) => result,
        Err(_) => Err(PlatformError::Timeout(timeout.as_millis() as u64)),
    }
}

/// Convert a panic in host code into a render error.
fn catch_host_panic<T>(
    work: impl FnOnce() -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(payload) => Err(PlatformError::Render(format!(
            "host panic: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

/// One live WebView2 instance. Everything here must be used and dropped on the
/// STA thread that created it.
struct StaRender {
    controller: ICoreWebView2Controller,
    core: ICoreWebView2,
    environment: ICoreWebView2Environment,
    _window: HostWindow,
    _user_data: UserDataDir,
    deadline: Instant,
    timeout: Duration,
}

impl StaRender {
    /// Create the environment/controller without loading a document. PDF
    /// printing needs its own navigation URL, so loading stays separate.
    fn open_bare(
        spec: &RenderSpec,
        deadline: Instant,
        timeout: Duration,
        script_enabled: bool,
        offscreen: bool,
    ) -> Result<Self, PlatformError> {
        let user_data = UserDataDir::create()?;
        let environment = create_environment(&user_data.path, deadline, timeout)?;

        let (window, controller) = if offscreen {
            // The PDF viewer is a browser extension; the known-working
            // off-screen scenario uses a shown popup, not a message-only HWND.
            let window = HostWindow::offscreen()?;
            let controller = create_controller(&environment, window.0, deadline, timeout)?;
            (window, controller)
        } else {
            match HostWindow::message_only().and_then(|window| {
                create_controller(&environment, window.0, deadline, timeout).map(|c| (window, c))
            }) {
                Ok(pair) => pair,
                // Retrying after the budget is gone only produces a second timeout.
                Err(first_error) if Instant::now() >= deadline => return Err(first_error),
                Err(first_error) => {
                    let window = HostWindow::offscreen().map_err(|error| {
                        PlatformError::Unavailable(format!(
                            "WebView2 controller parent window failed ({first_error}); \
                             offscreen fallback failed too: {error}"
                        ))
                    })?;
                    let controller = create_controller(&environment, window.0, deadline, timeout)
                        .map_err(|error| {
                        PlatformError::Unavailable(format!(
                            "WebView2 controller creation failed (message-only: {first_error}; \
                                 offscreen: {error})"
                        ))
                    })?;
                    (window, controller)
                }
            }
        };

        // SAFETY: the controller was created on this thread and is live.
        let core = unsafe { controller.CoreWebView2() }.map_err(render_error)?;
        // SAFETY: same thread; every setter only stores a value.
        unsafe {
            controller
                .SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: css_px(spec.content_width_mm()),
                    bottom: css_px(spec.content_height_mm()),
                })
                .map_err(render_error)?;
            core.Settings()
                .map_err(render_error)?
                .SetIsScriptEnabled(script_enabled)
                .map_err(render_error)?;
        }

        Ok(Self {
            controller,
            core,
            environment,
            _window: window,
            _user_data: user_data,
            deadline,
            timeout,
        })
    }

    /// Create a session and load HTML (`NavigateToString`).
    fn open(
        html: &str,
        spec: &RenderSpec,
        deadline: Instant,
        timeout: Duration,
        script_enabled: bool,
    ) -> Result<Self, PlatformError> {
        let session = Self::open_bare(spec, deadline, timeout, script_enabled, false)?;
        navigate(&session.core, html, deadline, timeout)?;
        Ok(session)
    }

    fn print(&self, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        let settings = self.create_print_settings()?;
        apply_print_geometry(&settings, spec)?;

        let (sender, receiver) = mpsc::channel();
        let handler = PrintToPdfStreamCompletedHandler::create(Box::new(move |result, stream| {
            let _ = sender.send((result, stream));
            Ok(())
        }));
        // SAFETY: `ICoreWebView2_16` is queried from the live core object and
        // the callback stays alive until the handler fires.
        unsafe {
            self.core
                .cast::<ICoreWebView2_16>()
                .map_err(|error| {
                    PlatformError::Unavailable(format!(
                        "WebView2 runtime has no ICoreWebView2_16: {error}"
                    ))
                })?
                .PrintToPdfStream(&settings, &handler)
                .map_err(render_error)?;
        }

        let (result, stream) = pump_until(receiver, self.deadline, self.timeout)?;
        result.map_err(|error| {
            PlatformError::Render(format!("WebView2 PrintToPdfStream failed: {error}"))
        })?;
        let stream = stream.ok_or_else(|| {
            PlatformError::Render("WebView2 PrintToPdfStream returned no stream".to_owned())
        })?;
        read_pdf_stream(&stream)
    }

    /// Print this document straight to a driver. The web view is the same one
    /// that would produce the PDF, so geometry and pagination match.
    fn print_to_printer(
        &self,
        spec: Option<&RenderSpec>,
        request: &PrintRequest,
    ) -> Result<(), PlatformError> {
        let settings = self.create_print_settings()?;
        if let Some(spec) = spec {
            apply_print_geometry(&settings, spec)?;
        }
        // The cast is the runtime check: a runtime without
        // `ICoreWebView2PrintSettings2` refuses direct printing here.
        let settings2 = settings
            .cast::<ICoreWebView2PrintSettings2>()
            .map_err(|error| {
                PlatformError::Unavailable(format!(
                    "WebView2 runtime is too old for direct printing: {error}"
                ))
            })?;
        let printer = request.printer.clone().unwrap_or_default();
        let printer_name = HSTRING::from(printer.as_str());
        let copies = i32::try_from(request.copies).map_err(|_| {
            PlatformError::Print(format!("copies {} is out of range", request.copies))
        })?;
        // SAFETY: every setter only stores a value on a live settings object;
        // `printer_name` outlives the call.
        unsafe {
            settings2
                .SetPrinterName(&printer_name)
                .map_err(render_error)?;
            settings2.SetCopies(copies).map_err(render_error)?;
            settings2
                .SetCollation(if request.collate {
                    COREWEBVIEW2_PRINT_COLLATION_COLLATED
                } else {
                    COREWEBVIEW2_PRINT_COLLATION_UNCOLLATED
                })
                .map_err(render_error)?;
            if spec.is_some() {
                // Only an explicit paper size becomes a custom media request;
                // a PDF with no paper hint keeps the viewer/driver default.
                settings2
                    .SetMediaSize(COREWEBVIEW2_PRINT_MEDIA_SIZE_CUSTOM)
                    .map_err(render_error)?;
            }
            settings2
                .SetDuplex(duplex_setting(request.duplex))
                .map_err(render_error)?;
        }

        let (sender, receiver) = mpsc::channel();
        let handler = PrintCompletedHandler::create(Box::new(move |result, status| {
            let _ = sender.send((result.map_err(|error| error.to_string()), status));
            Ok(())
        }));
        // SAFETY: `ICoreWebView2_16` is queried from the live core object and
        // the callback stays alive until the handler fires.
        unsafe {
            self.core
                .cast::<ICoreWebView2_16>()
                .map_err(|error| {
                    PlatformError::Unavailable(format!(
                        "WebView2 runtime has no ICoreWebView2_16: {error}"
                    ))
                })?
                .Print(&settings, &handler)
                .map_err(render_error)?;
        }
        let (result, status) = pump_until(receiver, self.deadline, self.timeout)?;
        print_outcome(&printer, result, status)
    }

    fn create_print_settings(&self) -> Result<ICoreWebView2PrintSettings, PlatformError> {
        // SAFETY: the environment lives on this thread.
        unsafe {
            self.environment
                .cast::<ICoreWebView2Environment6>()
                .map_err(|error| {
                    PlatformError::Unavailable(format!(
                        "WebView2 runtime is too old for print settings: {error}"
                    ))
                })?
                .CreatePrintSettings()
                .map_err(render_error)
        }
    }

    fn measure(&self) -> Result<RowHeights, PlatformError> {
        // `ExecuteScript` returns the value as JSON. `tr` elements carry the
        // `data-row-key` the renderer wrote, and `getBoundingClientRect`
        // flushes layout, so the height is the one printing will use.
        const SCRIPT: &str = "Array.from(document.querySelectorAll('tr')).map(function (row) {\
             var key = row.getAttribute('data-row-key') || '';\
             var box = row.getBoundingClientRect();\
             return [key, box.height];\
         })";

        let (sender, receiver) = mpsc::channel();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, json| {
            let _ = sender.send((result, json));
            Ok(())
        }));
        let script = HSTRING::from(SCRIPT);
        // SAFETY: the core object and the callback are live on this thread.
        unsafe { self.core.ExecuteScript(&script, &handler) }.map_err(render_error)?;

        let (result, json) = pump_until(receiver, self.deadline, self.timeout)?;
        result.map_err(|error| {
            PlatformError::Render(format!("WebView2 ExecuteScript failed: {error}"))
        })?;
        let rows: Vec<(String, f64)> = serde_json::from_str(&json).map_err(|error| {
            PlatformError::Render(format!("WebView2 returned unreadable row heights: {error}"))
        })?;

        let mut heights = RowHeights::new();
        for (key, css_px_height) in rows {
            if !key.is_empty() && css_px_height > 0.0 {
                heights.insert(key, css_px_height * CSS_PX_TO_MM);
            }
        }
        Ok(heights)
    }
}

impl Drop for StaRender {
    fn drop(&mut self) {
        // Closing the controller releases the web process before the window and
        // the user-data directory go away.
        // SAFETY: the controller is live and belongs to this thread.
        unsafe {
            let _ = self.controller.Close();
        }
    }
}
/// Map the protocol's explicit `duplex` bool to the WebView2 driver setting.
/// `false` is one-sided, not the driver default; v1 never offers follow-driver.
fn duplex_setting(duplex: bool) -> COREWEBVIEW2_PRINT_DUPLEX {
    if duplex {
        COREWEBVIEW2_PRINT_DUPLEX_TWO_SIDED_LONG_EDGE
    } else {
        COREWEBVIEW2_PRINT_DUPLEX_ONE_SIDED
    }
}

/// Options for one direct-to-driver print, independent of COM.
pub(crate) struct PrintRequest {
    pub printer: Option<String>,
    pub copies: u32,
    pub collate: bool,
    pub duplex: bool,
}

/// Print `html` through WebView2 `Print` on its own STA thread.
///
/// The backend calls this after the engine rendered the document once; the
/// HTML and spec are copied because the STA thread outlives the borrow. There
/// is no spooler job id on this path.
pub(crate) fn print_html(
    request: PrintRequest,
    html: String,
    spec: RenderSpec,
    timeout: Duration,
) -> Result<(), PlatformError> {
    run_on_sta_thread(timeout, move |deadline| {
        let session = StaRender::open(&html, &spec, deadline, timeout, false)?;
        session.print_to_printer(Some(&spec), &request)
    })
}

/// Print a PDF through WebView2's built-in PDF viewer.
///
/// The file is written to a unique temporary path and removed when the print
/// returns. WebView2 has no "PDF viewer ready" event, so the thread pumps for
/// [`PDF_VIEWER_SETTLE`] after `NavigationCompleted`; printing immediately has
/// produced blank pages (WebView2Feedback #3779). No fallback to HTML printing:
/// that would print a different document.
pub(crate) fn print_pdf(
    pdf: Bytes,
    request: PrintRequest,
    paper_mm: Option<(f64, f64)>,
    title: &str,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let prefix = format!("printkit-{}-", safe_title(title));
    run_on_sta_thread(timeout, move |deadline| {
        let mut file = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(".pdf")
            .tempfile()
            .map_err(|error| {
                PlatformError::Unavailable(format!("cannot create the PDF spool file: {error}"))
            })?;
        file.write_all(&pdf)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|error| {
                PlatformError::Print(format!("cannot write the PDF spool file: {error}"))
            })?;
        let url = file_url(file.path());
        let paper = paper_mm.map(|(width, height)| RenderSpec::new(width, height));
        // The window needs some bounds; a PDF with no paper hint still gets a
        // neutral A4 window while the viewer/driver choose the real media.
        let bounds = paper
            .clone()
            .unwrap_or_else(|| RenderSpec::new(210.0, 297.0));
        let session = StaRender::open_bare(&bounds, deadline, timeout, false, true)?;
        navigate_url(&session.core, &url, deadline, timeout)?;
        pump_messages_for(PDF_VIEWER_SETTLE);
        session.print_to_printer(paper.as_ref(), &request)
    })
}

/// Time for the PDF viewer to finish loading after navigation. This is a
/// race workaround, not a guarantee; verify on the target runtime.
const PDF_VIEWER_SETTLE: Duration = Duration::from_millis(500);

/// A spool filename prefix derived from the document title.
///
/// Keep it ASCII and short: the PDF viewer shows the file name in the queue.
fn safe_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.')
        })
        .take(48)
        .collect();
    let trimmed = cleaned.trim().replace(' ', "_");
    if trimmed.is_empty() {
        "printkit".to_owned()
    } else {
        trimmed
    }
}

/// `file:///C:/...` URL with every non-unreserved byte percent-encoded.
fn file_url(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let text = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from("file:///");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                url.push(char::from(byte))
            }
            _ => {
                url.push('%');
                url.push(char::from(HEX[usize::from(byte >> 4)]));
                url.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    url
}

/// Pump messages for a fixed time; a sleeping thread would not deliver PDF
/// viewer callbacks.
fn pump_messages_for(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut message = MSG::default();
    while Instant::now() < deadline {
        // SAFETY: `message` is a valid MSG and this thread owns the queue.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Whether any page differs from the document's nominal paper size.
fn has_mixed_paper(spec: &RenderSpec) -> bool {
    spec.page_sizes.iter().any(|(width, height)| {
        (width - spec.width_mm).abs() > 0.01 || (height - spec.height_mm).abs() > 0.01
    })
}

/// Keep only the `page`-th direct `<body>.page` box visible for a single-page
/// render. `write_page_open` makes every page a direct body child, so
/// `nth-of-type` cannot select an item nested inside a page.
fn page_only_html(html: &str, page: usize, size_mm: (f64, f64)) -> String {
    let (width_mm, height_mm) = size_mm;
    // Repeat the page size as the last @page rule: the document's first page
    // size would otherwise describe this single-page render.
    let style = format!(
        "<style>@page{{size:{width_mm}mm {height_mm}mm;margin:0}}\
         body>.page{{display:none!important;break-after:auto!important}}\
         body>.page:nth-of-type({}){{display:block!important}}</style>",
        page + 1
    );
    match html.find("</head>") {
        Some(index) => format!("{}{}{}", &html[..index], style, &html[index..]),
        None => format!("{style}{html}"),
    }
}

fn check_page_count(actual: u32, expected: usize) -> Result<(), PlatformError> {
    if usize::try_from(actual).ok() == Some(expected) {
        Ok(())
    } else {
        Err(PlatformError::Render(format!(
            "merged mixed-paper PDF has {actual} pages, expected {expected}"
        )))
    }
}

/// Portrait paper size in inches plus whether the page is landscape.
fn paper_setup(spec: &RenderSpec) -> (f64, f64, bool) {
    let (width_mm, height_mm) = (spec.width_mm, spec.height_mm);
    let landscape = width_mm > height_mm;
    // `PageWidth`/`PageHeight` describe portrait paper; `Orientation` rotates
    // the content. Feeding the landscape dimensions here and also asking for
    // landscape would rotate the page twice.
    let (paper_width_mm, paper_height_mm) = if landscape {
        (height_mm, width_mm)
    } else {
        (width_mm, height_mm)
    };
    (paper_width_mm / 25.4, paper_height_mm / 25.4, landscape)
}

/// Geometry shared by PDF export and direct printing.
fn apply_print_geometry(
    settings: &ICoreWebView2PrintSettings,
    spec: &RenderSpec,
) -> Result<(), PlatformError> {
    let (paper_width_in, paper_height_in, landscape) = paper_setup(spec);
    // SAFETY: every setter only stores a value on a live settings object.
    unsafe {
        settings
            .SetPageWidth(paper_width_in)
            .map_err(render_error)?;
        settings
            .SetPageHeight(paper_height_in)
            .map_err(render_error)?;
        // The HTML draws its own margins; the printer must not add more.
        settings.SetMarginTop(0.0).map_err(render_error)?;
        settings.SetMarginBottom(0.0).map_err(render_error)?;
        settings.SetMarginLeft(0.0).map_err(render_error)?;
        settings.SetMarginRight(0.0).map_err(render_error)?;
        settings
            .SetShouldPrintBackgrounds(true)
            .map_err(render_error)?;
        settings
            .SetShouldPrintHeaderAndFooter(false)
            .map_err(render_error)?;
        settings
            .SetOrientation(if landscape {
                COREWEBVIEW2_PRINT_ORIENTATION_LANDSCAPE
            } else {
                COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT
            })
            .map_err(render_error)?;
    }
    Ok(())
}

/// Classify a `Print` completion. Both the HRESULT and the print status decide
/// the outcome: `S_OK` with `PRINTER_UNAVAILABLE` is not a success.
fn print_outcome(
    printer: &str,
    result: Result<(), String>,
    status: COREWEBVIEW2_PRINT_STATUS,
) -> Result<(), PlatformError> {
    if let Err(message) = result {
        return Err(PlatformError::Print(format!(
            "WebView2 Print failed{printer_suffix}: {message}",
            printer_suffix = printer_suffix(printer)
        )));
    }
    if status == COREWEBVIEW2_PRINT_STATUS_SUCCEEDED {
        Ok(())
    } else if status == COREWEBVIEW2_PRINT_STATUS_PRINTER_UNAVAILABLE {
        Err(PlatformError::PrinterNotFound(printer.to_owned()))
    } else {
        Err(PlatformError::Print(format!(
            "WebView2 Print reported status {}{}",
            status.0,
            printer_suffix(printer)
        )))
    }
}

fn printer_suffix(printer: &str) -> String {
    if printer.is_empty() {
        " on the default printer".to_owned()
    } else {
        format!(" on {printer}")
    }
}

pub(crate) fn create_environment(
    user_data: &std::path::Path,
    deadline: Instant,
    timeout: Duration,
) -> Result<ICoreWebView2Environment, PlatformError> {
    let (sender, receiver) = mpsc::channel();
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, environment| {
            let _ = sender.send((result, environment));
            Ok(())
        },
    ));
    let user_data = HSTRING::from_wide(&user_data.as_os_str().encode_wide().collect::<Vec<u16>>());
    // SAFETY: a null browser-executable folder selects the installed runtime;
    // both strings and the handler outlive the call.
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &user_data,
            None::<&ICoreWebView2EnvironmentOptions>,
            &handler,
        )
        .map_err(render_error)?;
    }
    let (result, environment) = pump_until(receiver, deadline, timeout)?;
    result.map_err(|error| {
        PlatformError::Unavailable(format!("cannot create the WebView2 environment: {error}"))
    })?;
    environment
        .ok_or_else(|| PlatformError::Unavailable("WebView2 returned no environment".to_owned()))
}

pub(crate) fn create_controller(
    environment: &ICoreWebView2Environment,
    parent: HWND,
    deadline: Instant,
    timeout: Duration,
) -> Result<ICoreWebView2Controller, PlatformError> {
    let (sender, receiver) = mpsc::channel();
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result, controller| {
            let _ = sender.send((result, controller));
            Ok(())
        },
    ));
    // SAFETY: the environment and handler are live; the window is a valid
    // HWND owned by this thread.
    unsafe { environment.CreateCoreWebView2Controller(parent, &handler) }.map_err(render_error)?;
    let (result, controller) = pump_until(receiver, deadline, timeout)?;
    result.map_err(render_error)?;
    controller.ok_or_else(|| PlatformError::Render("WebView2 returned no controller".to_owned()))
}

pub(crate) fn navigate(
    core: &ICoreWebView2,
    html: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let html = HSTRING::from(html);
    // SAFETY: the HTML string outlives the navigation call.
    navigate_with(core, deadline, timeout, |core| unsafe {
        core.NavigateToString(&html)
    })
}

/// Load a URL (for example the temporary PDF file) instead of inline HTML.
pub(crate) fn navigate_url(
    core: &ICoreWebView2,
    url: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let url = HSTRING::from(url);
    // SAFETY: the URL string outlives the navigation call.
    navigate_with(core, deadline, timeout, |core| unsafe {
        core.Navigate(&url)
    })
}

fn navigate_with(
    core: &ICoreWebView2,
    deadline: Instant,
    timeout: Duration,
    start: impl FnOnce(&ICoreWebView2) -> windows::core::Result<()>,
) -> Result<(), PlatformError> {
    let (sender, receiver) = mpsc::channel();
    let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
        let success = match args {
            Some(args) => {
                let mut flag = BOOL::default();
                // SAFETY: the arguments object is live for the callback.
                unsafe { args.IsSuccess(&mut flag) }.is_ok() && flag.as_bool()
            }
            None => false,
        };
        let _ = sender.send(success);
        Ok(())
    }));
    let mut token = 0i64;
    // SAFETY: the core object, handler and token are live on this thread.
    unsafe { core.add_NavigationCompleted(&handler, &mut token) }.map_err(render_error)?;
    start(core).map_err(render_error)?;
    let success = pump_until(receiver, deadline, timeout)?;
    // SAFETY: `token` was returned by the matching add call.
    unsafe {
        let _ = core.remove_NavigationCompleted(token);
    }
    if success {
        Ok(())
    } else {
        Err(PlatformError::Render(
            "WebView2 navigation to the document failed".to_owned(),
        ))
    }
}

/// Copy `IStream` to bytes. `Read` reports the end of the stream as zero bytes.
fn read_pdf_stream(stream: &IStream) -> Result<Bytes, PlatformError> {
    // The print callback may leave the stream at the end; rewind first.
    // SAFETY: a plain stream seek.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }
        .map_err(|error| PlatformError::Render(format!("cannot rewind the PDF stream: {error}")))?;
    let mut pdf = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let mut read = 0u32;
        // SAFETY: `buffer` is writable for its full length and `read` is a
        // writable out-parameter.
        let status = unsafe {
            stream.Read(
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as u32,
                Some(&mut read),
            )
        };
        status.ok().map_err(|error| {
            PlatformError::Render(format!("cannot read the PDF stream: {error}"))
        })?;
        if read == 0 {
            break;
        }
        pdf.extend_from_slice(&buffer[..read as usize]);
    }
    if pdf.is_empty() {
        return Err(PlatformError::Render(
            "WebView2 PrintToPdfStream returned an empty PDF".to_owned(),
        ));
    }
    Ok(Bytes::from(pdf))
}

/// Wait for one callback while dispatching its message, with a deadline.
pub(crate) fn pump_until<T>(
    receiver: Receiver<T>,
    deadline: Instant,
    timeout: Duration,
) -> Result<T, PlatformError> {
    let mut message = MSG::default();
    loop {
        if let Ok(value) = receiver.try_recv() {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout(timeout.as_millis() as u64));
        }
        // SAFETY: `message` is a valid MSG and this thread owns the queue.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A message-only window, or an offscreen popup when the runtime refuses the
/// former as a controller parent.
struct HostWindow(HWND);

impl HostWindow {
    fn message_only() -> Result<Self, PlatformError> {
        Self::create(Some(HWND_MESSAGE), 0, 0)
    }

    fn offscreen() -> Result<Self, PlatformError> {
        let window = Self::create(None, -32_000, -32_000)?;
        // Some WebView2 builds only render for a shown window. Off-screen and
        // without activation keeps it invisible in practice while the web
        // process stays alive.
        // SAFETY: the window was created on this thread and is still live.
        unsafe {
            let _ = ShowWindow(window.0, SW_SHOWNOACTIVATE);
        }
        Ok(window)
    }

    fn create(parent: Option<HWND>, x: i32, y: i32) -> Result<Self, PlatformError> {
        // A system class avoids registering our own window procedure; the
        // window never becomes visible, so its class does nothing else.
        // SAFETY: the class and title strings are static and null-terminated.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("printkit"),
                WS_POPUP,
                x,
                y,
                1,
                1,
                parent,
                None,
                None,
                None,
            )
        }
        .map_err(|error| {
            PlatformError::Unavailable(format!("cannot create the WebView2 window: {error}"))
        })?;
        Ok(Self(window))
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread and is destroyed once.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// COM apartment for the render thread; `CoUninitialize` runs on drop.
pub(crate) struct ComApartment;

impl ComApartment {
    pub(crate) fn init() -> Result<Self, PlatformError> {
        // SAFETY: called once per render thread before any WebView2 object.
        let status = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if status.is_err() {
            return Err(PlatformError::Unavailable(format!(
                "cannot initialise COM for WebView2: {}",
                windows::core::Error::from(status)
            )));
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// Per-render user-data folder. A unique directory keeps concurrent render
/// threads from sharing one browser profile.
pub(crate) struct UserDataDir {
    path: PathBuf,
}

impl UserDataDir {
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn create() -> Result<Self, PlatformError> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "printkit-webview2-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).map_err(|error| {
            PlatformError::Unavailable(format!(
                "cannot create the WebView2 user data folder: {error}"
            ))
        })?;
        Ok(Self { path })
    }
}

impl Drop for UserDataDir {
    fn drop(&mut self) {
        // The browser process may still hold files for a moment; a leftover
        // directory in the temp folder is harmless and never blocks a render.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// CSS pixels for a millimetre length at 96 dpi.
pub(crate) fn css_px(mm: f64) -> i32 {
    let pixels = (mm * 96.0 / 25.4).round();
    pixels.clamp(1.0, i32::MAX as f64) as i32
}

pub(crate) fn render_error(error: windows::core::Error) -> PlatformError {
    PlatformError::Render(error.to_string())
}

/// What `printkit doctor` reports about the WebView2 Runtime.
///
/// [`WebviewInfo`] has no free-form message field, so an unusable runtime puts
/// the reason into `engine`: `doctor` and the `pdf`/`print` preflight print that
/// string verbatim instead of a bare "renderer is not available".
pub fn webview_info() -> WebviewInfo {
    match query_runtime() {
        Ok(Some(version)) => match runtime_is_new_enough(&version) {
            Ok(true) => WebviewInfo {
                engine: "WebView2".to_owned(),
                available: true,
                version: Some(version),
            },
            Ok(false) => WebviewInfo {
                engine: format!("WebView2 runtime {version} is older than {MIN_RUNTIME_VERSION}"),
                available: false,
                version: Some(version),
            },
            Err(message) => WebviewInfo {
                engine: format!("cannot compare WebView2 runtime {version}: {message}"),
                available: false,
                version: Some(version),
            },
        },
        Ok(None) => WebviewInfo {
            engine: "WebView2 runtime is not installed".to_owned(),
            available: false,
            version: None,
        },
        Err(message) => WebviewInfo {
            engine: format!("WebView2 runtime probe failed: {message}"),
            available: false,
            version: None,
        },
    }
}

/// Ask the loader for the installed Evergreen runtime version.
///
/// `Ok(None)` is "not installed": the loader returns a null version string and
/// no error in that case. The returned buffer is allocated by COM and must be
/// released with `CoTaskMemFree`.
fn query_runtime() -> Result<Option<String>, String> {
    let mut raw = PWSTR::null();
    // SAFETY: a null browser-executable folder asks for the installed
    // Evergreen runtime; `raw` is a valid out-parameter and the loader returns
    // an HRESULT before writing it.
    unsafe {
        GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut raw)
            .map_err(|error| error.to_string())?;
    }
    if raw.is_null() {
        return Ok(None);
    }
    // SAFETY: the loader wrote a null-terminated UTF-16 string that COM owns;
    // it is copied before the buffer is released, and released exactly once.
    let version = unsafe { raw.to_string() };
    // SAFETY: same allocation as above, not used afterwards.
    unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
    version.map(Some).map_err(|error| error.to_string())
}

/// `CompareBrowserVersions` against the `ICoreWebView2_16` floor.
fn runtime_is_new_enough(version: &str) -> Result<bool, String> {
    let detected = HSTRING::from(version);
    let minimum = HSTRING::from(MIN_RUNTIME_VERSION);
    let mut ordering = 0i32;
    // SAFETY: both HSTRINGs outlive the call and the loader only reads them.
    unsafe { CompareBrowserVersions(&detected, &minimum, &mut ordering) }
        .map_err(|error| error.to_string())?;
    Ok(ordering >= 0)
}

/// Pump this thread's message queue while `keep_running` stays true.
///
/// WebView2 callbacks arrive as posted window messages on the thread that
/// created the controller, so the process main thread must keep dispatching
/// them. The service pumps this on its main thread; individual renders have
/// their own STA thread and pump inside `pump_until`.
pub fn run_main_loop_while(keep_running: &mut dyn FnMut() -> bool) {
    let mut message = MSG::default();
    while keep_running() {
        // SAFETY: `message` is a valid MSG for the duration of the calls, and
        // the thread owns the queue being inspected.
        let had_message = unsafe {
            let had_message = PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool();
            if had_message {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            had_message
        };
        if !had_message {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PRINT_STATUS_OTHER_ERROR;

    use super::*;

    #[test]
    fn landscape_paper_is_normalised_to_portrait_with_orientation() {
        let spec = RenderSpec::new(297.0, 210.0);
        let (width_in, height_in, landscape) = paper_setup(&spec);
        assert!(landscape);
        assert!((width_in - 210.0 / 25.4).abs() < 1e-9);
        assert!((height_in - 297.0 / 25.4).abs() < 1e-9);
    }

    #[test]
    fn spool_filename_prefix_is_ascii_and_short() {
        assert_eq!(safe_title("Invoice 42"), "Invoice_42");
        assert_eq!(safe_title("../../etc/passwd"), "etcpasswd");
        assert_eq!(safe_title(""), "printkit");
    }

    #[test]
    fn portrait_paper_keeps_its_orientation() {
        let spec = RenderSpec::new(210.0, 297.0);
        let (width_in, height_in, landscape) = paper_setup(&spec);
        assert!(!landscape);
        assert!((width_in - 210.0 / 25.4).abs() < 1e-9);
        assert!((height_in - 297.0 / 25.4).abs() < 1e-9);
    }

    #[test]
    fn mixed_paper_is_detected_from_page_sizes() {
        let uniform =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (210.0, 297.0)]);
        assert!(!has_mixed_paper(&uniform));
        let mixed =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (148.0, 210.0)]);
        assert!(has_mixed_paper(&mixed));
    }

    #[test]
    fn page_only_html_injects_before_head_and_selects_the_page() {
        let html = "<html><head><style>x</style></head>\
                    <body><div class=\"page\">one</div><div class=\"page\">two</div></body></html>";
        let injected = page_only_html(html, 1, (148.0, 210.0));
        assert!(injected.contains("@page{size:148mm 210mm;margin:0}"));
        assert!(injected.contains("body>.page:nth-of-type(2)"));
        assert!(injected.contains("break-after:auto!important"));
        assert!(injected.contains("<div class=\"page\">one</div>"));
        let style = injected.find("<style>body>.page").expect("style injected");
        let head_end = injected.find("</head>").expect("head present");
        assert!(style < head_end);
    }

    #[test]
    fn page_only_html_prefixes_when_head_is_missing() {
        let injected = page_only_html("<body><div class=\"page\"></div></body>", 0, (210.0, 297.0));
        assert!(injected.starts_with("<style>body>.page"));
        assert!(injected.contains("nth-of-type(1)"));
    }

    #[test]
    fn merged_page_count_must_match_the_spec() {
        assert!(check_page_count(2, 2).is_ok());
        let error = check_page_count(1, 2).unwrap_err();
        assert!(matches!(error, PlatformError::Render(message) if message.contains("expected 2")));
    }

    #[test]
    fn pdf_paths_become_percent_encoded_file_urls() {
        assert_eq!(
            file_url(Path::new(r"C:\tmp\a b.pdf")),
            "file:///C:/tmp/a%20b.pdf"
        );
        assert_eq!(
            file_url(Path::new(r"C:\café\a.pdf")),
            "file:///C:/caf%C3%A9/a.pdf"
        );
    }

    #[test]
    fn a_sta_thread_panic_is_not_mistaken_for_a_timeout() {
        let result: Result<(), PlatformError> = run_on_sta_thread(
            Duration::from_millis(200),
            |_deadline| -> Result<(), PlatformError> { panic!("sta boom") },
        );
        assert!(matches!(
            result,
            Err(PlatformError::Render(message)) if message == "host panic: sta boom"
        ));
    }

    #[test]
    fn a_panicking_host_is_reported_as_a_render_error() {
        let error = catch_host_panic(|| -> Result<(), PlatformError> { panic!("boom") })
            .expect_err("a panic is an error");
        assert!(matches!(
            error,
            PlatformError::Render(message) if message == "host panic: boom"
        ));
    }

    #[test]
    fn unknown_panic_payloads_are_still_named() {
        let error =
            catch_host_panic(|| -> Result<(), PlatformError> { std::panic::panic_any(42_u32) })
                .expect_err("a panic is an error");
        assert!(matches!(
            error,
            PlatformError::Render(message) if message.contains("unknown panic payload")
        ));
    }

    #[test]
    fn print_status_only_succeeds_on_succeeded() {
        assert!(print_outcome("p", Ok(()), COREWEBVIEW2_PRINT_STATUS_SUCCEEDED).is_ok());
        assert!(matches!(
            print_outcome("p", Ok(()), COREWEBVIEW2_PRINT_STATUS_PRINTER_UNAVAILABLE),
            Err(PlatformError::PrinterNotFound(printer)) if printer == "p"
        ));
        assert!(matches!(
            print_outcome("p", Ok(()), COREWEBVIEW2_PRINT_STATUS_OTHER_ERROR),
            Err(PlatformError::Print(_))
        ));
        // An HRESULT failure is not rescued by a contradictory status.
        assert!(
            print_outcome(
                "p",
                Err("E_ABORT".to_owned()),
                COREWEBVIEW2_PRINT_STATUS_SUCCEEDED
            )
            .is_err()
        );
    }

    #[test]
    fn explicit_duplex_maps_to_one_sided_or_two_sided() {
        assert_eq!(
            duplex_setting(false).0,
            COREWEBVIEW2_PRINT_DUPLEX_ONE_SIDED.0,
            "false must be explicit one-sided, never driver default"
        );
        assert_eq!(
            duplex_setting(true).0,
            COREWEBVIEW2_PRINT_DUPLEX_TWO_SIDED_LONG_EDGE.0
        );
    }
}
