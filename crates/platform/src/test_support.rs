//! Test double for [`crate::RenderHost`].
//!
//! Shared by crate tests and the mock print script: returns fixed PDF bytes,
//! optionally panics (to exercise crash recovery) and optionally delays (to
//! exercise queueing and timeouts). It never touches a WebView, so it works on
//! any host and inside CI.
//!
//! End-to-end runs cannot pass builder options through the service, so the
//! double also reacts to markers inside the rendered HTML. The crash marker
//! panics on the first render of the process only: the point is that the pool
//! rebuilds the instance and the retry succeeds. The always marker panics on
//! every render, which exhausts the single retry.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{PlatformError, PreviewActions, PreviewRequest, RenderHost, RenderSpec, WindowId};

/// Panics on the first render whose HTML carries this marker.
pub const CRASH_MARKER: &str = "fake-render-crash";

/// Panics on every render whose HTML carries this marker.
pub const CRASH_ALWAYS_MARKER: &str = "fake-render-crash-always";

/// Blocks for the following decimal milliseconds; `fake-render-delay:250`.
pub const DELAY_MARKER: &str = "fake-render-delay:";

/// Upper bound for a marker delay, so a typo cannot pin a worker thread.
const MAX_MARKER_DELAY_MS: u64 = 60_000;

/// Marker state is process-wide because a rebuilt instance must not replay the
/// crash: the retry has to succeed to prove recovery.
static CRASHED_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A one-page PDF whose font is embedded as a subset, so it passes the
/// self-containment check that the engine applies to renderer output.
fn fake_pdf() -> Vec<u8> {
    let content = b"BT /F1 24 Tf 72 770 Td (printkit fake render host) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            String::from_utf8_lossy(content)
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /AAAAAA+Fake /FontDescriptor 6 0 R >>".to_owned(),
        "<< /Type /FontDescriptor /FontName /AAAAAA+Fake /Flags 4 \
         /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
         /CapHeight 700 /StemV 80 /FontFile 7 0 R >>"
            .to_owned(),
        "<< /Length 4 >>\nstream\nfake\nendstream".to_owned(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{object}\nendobj\n", index + 1);
    }
    let xref_start = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        let _ = writeln!(out, "{offset:010} 00000 n ");
    }
    let _ = write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF\n",
        objects.len() + 1
    );
    out.into_bytes()
}

/// A [`RenderHost`] that returns fixed bytes.
#[derive(Clone)]
pub struct FakeRenderHost {
    pdf: Bytes,
    /// Repeat the fixed page per requested page; a custom PDF already has its
    /// own page count and is returned unchanged.
    paginate: bool,
    inject_panic: bool,
    delay: Duration,
    preview: bool,
    calls: Arc<AtomicUsize>,
    windows: Arc<Mutex<HashMap<WindowId, FakeWindow>>>,
    next_window: Arc<AtomicUsize>,
}

struct FakeWindow {
    spec: RenderSpec,
    actions: Arc<dyn PreviewActions>,
}

impl std::fmt::Debug for FakeWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeWindow")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl FakeRenderHost {
    /// A host with the built-in one-page A4 PDF, repeated once per requested
    /// page; no delay, no injected panic, and preview windows available.
    ///
    /// The HTML markers (`fake-render-crash`, `fake-render-crash-always`,
    /// `fake-render-delay:<ms>`) still override the defaults per call.
    pub fn new() -> Self {
        Self {
            pdf: Bytes::from(fake_pdf()),
            paginate: true,
            inject_panic: false,
            delay: Duration::ZERO,
            preview: true,
            calls: Arc::new(AtomicUsize::new(0)),
            windows: Arc::new(Mutex::new(HashMap::new())),
            next_window: Arc::new(AtomicUsize::new(1)),
        }
    }
    /// Report no window server, as a remote or headless session would.
    pub fn without_preview(mut self) -> Self {
        self.preview = false;
        self
    }
    /// Replace the returned PDF bytes.
    pub fn with_pdf(mut self, pdf: impl Into<Bytes>) -> Self {
        self.pdf = pdf.into();
        self.paginate = false;
        self
    }

    /// Panic inside the render call, to test crash recovery.
    pub fn with_panic(mut self) -> Self {
        self.inject_panic = true;
        self
    }

    /// Block the calling thread before returning, to test queue timing.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// How many times this host was asked to render.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// Fire the window's Print control, as a click would.
    pub fn press_preview_print(&self, window: WindowId) -> bool {
        let actions = self
            .windows
            .lock()
            .ok()
            .and_then(|windows| windows.get(&window).map(|entry| Arc::clone(&entry.actions)));
        match actions {
            Some(actions) => {
                actions.print(window);
                true
            }
            None => false,
        }
    }

    /// Number of live preview windows, for tests that exercise the cap.
    pub fn preview_windows(&self) -> usize {
        self.windows
            .lock()
            .map(|windows| windows.len())
            .unwrap_or(0)
    }
}

impl Default for FakeRenderHost {
    fn default() -> Self {
        Self::new()
    }
}

/// A 1x1 white PNG: enough for the service tests that only check the envelope
/// and the ID lifecycle. Real image content is covered by the macOS host.
const FAKE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xff, 0xff, 0x3f,
    0x00, 0x05, 0xfe, 0x02, 0xfe, 0x0d, 0xef, 0x46, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];
/// Milliseconds requested by a [`DELAY_MARKER`] inside the HTML, if any.
fn marker_delay(html: &str) -> Option<Duration> {
    let start = html.find(DELAY_MARKER)? + DELAY_MARKER.len();
    let digits: String = html[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let millis = digits.parse::<u64>().ok()?.min(MAX_MARKER_DELAY_MS);

    Some(Duration::from_millis(millis))
}

#[async_trait]
#[allow(clippy::panic)] // panic injection is the point of this test double
impl RenderHost for FakeRenderHost {
    async fn render_to_pdf(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.inject_panic || html.contains(CRASH_ALWAYS_MARKER) {
            panic!("FakeRenderHost: injected render failure");
        }
        if html.contains(CRASH_MARKER) && !CRASHED_ONCE.swap(true, Ordering::SeqCst) {
            panic!("FakeRenderHost: injected render failure");
        }
        // A real host blocks its thread until the WebView answers; sleeping here
        // reproduces that from the caller's point of view without a WebView.
        let delay = if self.delay.is_zero() {
            marker_delay(html).unwrap_or(Duration::ZERO)
        } else {
            self.delay
        };
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let pages = spec.pages.max(1);
        if !self.paginate || pages == 1 {
            return Ok(self.pdf.clone());
        }
        // A real renderer produces one page per rectangle; concatenating the
        // fixed page keeps page-count assertions honest.
        let parts = vec![self.pdf.clone(); pages];
        printkit_pdf::concat(&parts)
            .map(|(pdf, _)| pdf)
            .map_err(|error| PlatformError::Render(error.to_string()))
    }

    fn preview_available(&self) -> bool {
        self.preview
    }

    async fn show_window(&self, request: PreviewRequest) -> Result<WindowId, PlatformError> {
        if !self.preview {
            return Err(PlatformError::Unavailable(
                "no preview window server".to_owned(),
            ));
        }
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(window) = request.window {
            let entry = windows
                .get_mut(&window)
                .ok_or_else(|| PlatformError::WindowNotFound(window.to_string()))?;
            entry.spec = request.spec;
            entry.actions = request.actions;
            return Ok(window);
        }
        let window = WindowId::new(self.next_window.fetch_add(1, Ordering::Relaxed) as u64);
        windows.insert(
            window,
            FakeWindow {
                spec: request.spec,
                actions: request.actions,
            },
        );
        Ok(window)
    }

    async fn window_png(
        &self,
        window: WindowId,
        page: u32,
        _dpi: u32,
    ) -> Result<Bytes, PlatformError> {
        let windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = windows
            .get(&window)
            .ok_or_else(|| PlatformError::WindowNotFound(window.to_string()))?;
        if usize::try_from(page).map_or(true, |page| page >= entry.spec.pages) {
            return Err(PlatformError::Render(format!(
                "page {page} is outside {} pages",
                entry.spec.pages
            )));
        }
        Ok(Bytes::from_static(FAKE_PNG))
    }

    async fn close_window(&self, window: WindowId) -> Result<(), PlatformError> {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        windows
            .remove(&window)
            .map(|_| ())
            .ok_or_else(|| PlatformError::WindowNotFound(window.to_string()))
    }
}

/// A [`crate::PrinterBackend`] that records submissions instead of touching a
/// spooler.
///
/// The mock print script runs with this backend: it proves the job pipeline and
/// usage rows without a printer, and it keeps `doc.print` acceptance
/// independent of the host's configured printers.
#[derive(Debug, Clone)]
pub struct FakePrinterBackend {
    submitted: Arc<std::sync::Mutex<Vec<crate::PrintJob>>>,
    raw: Arc<std::sync::Mutex<Vec<crate::RawJob>>>,
    cancelled: Arc<std::sync::Mutex<Vec<crate::SubmittedJob>>>,
    printers: Vec<crate::PrinterInfo>,
    next_id: Arc<AtomicUsize>,
}

impl FakePrinterBackend {
    /// A backend whose only printer, `fake-printer`, is the system default,
    /// advertises `application/pdf`, and rejects raw bytes and duplex; every
    /// submission is recorded instead of reaching a spooler.
    pub fn new() -> Self {
        Self::with_raw(false)
    }

    /// A backend whose only printer accepts raw bytes, for raw-channel tests.
    pub fn raw_capable() -> Self {
        Self::with_raw(true)
    }

    fn with_raw(raw: bool) -> Self {
        Self {
            submitted: Arc::new(std::sync::Mutex::new(Vec::new())),
            raw: Arc::new(std::sync::Mutex::new(Vec::new())),
            cancelled: Arc::new(std::sync::Mutex::new(Vec::new())),
            printers: vec![crate::PrinterInfo {
                name: "fake-printer".to_owned(),
                is_default: true,
                mime_types: vec!["application/pdf".to_owned()],
                duplex: false,
                raw,
                paper_source: false,
            }],
            next_id: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// Jobs handed to [`crate::PrinterBackend::submit`], oldest first.
    pub fn submitted(&self) -> Vec<crate::PrintJob> {
        self.submitted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Jobs handed to [`crate::PrinterBackend::submit_raw`], oldest first.
    pub fn raw_submissions(&self) -> Vec<crate::RawJob> {
        self.raw
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Jobs handed to [`crate::PrinterBackend::cancel_submitted`], oldest first.
    pub fn cancelled(&self) -> Vec<crate::SubmittedJob> {
        self.cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Default for FakePrinterBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::PrinterBackend for FakePrinterBackend {
    fn printers(&self) -> Result<Vec<crate::PrinterInfo>, PlatformError> {
        Ok(self.printers.clone())
    }

    fn papers(&self, _printer: &str) -> Result<Vec<crate::PaperInfo>, PlatformError> {
        Ok(vec![
            crate::PaperInfo {
                name: "A4".to_owned(),
                width_mm: 210.0,
                height_mm: 297.0,
                named: true,
            },
            crate::PaperInfo {
                name: "Letter".to_owned(),
                width_mm: 215.9,
                height_mm: 279.4,
                named: true,
            },
        ])
    }

    fn submit(&self, job: &crate::PrintJob) -> Result<crate::SubmittedJob, PlatformError> {
        let printer = job
            .printer
            .clone()
            .unwrap_or_else(|| "fake-printer".to_owned());
        self.submitted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(job.clone());
        let job_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(crate::SubmittedJob {
            job_id: Some(u32::try_from(job_id).unwrap_or(u32::MAX)),
            printer,
        })
    }

    fn prints_pdf(&self) -> bool {
        true
    }

    fn submit_raw(&self, job: &crate::RawJob) -> Result<crate::SubmittedJob, PlatformError> {
        let printer = job
            .printer
            .clone()
            .unwrap_or_else(|| "fake-printer".to_owned());
        let Some(info) = self.printers.iter().find(|info| info.name == printer) else {
            return Err(PlatformError::PrinterNotFound(printer));
        };
        if !info.raw {
            return Err(PlatformError::RawUnsupported(printer));
        }
        self.raw
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(job.clone());
        let job_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(crate::SubmittedJob {
            job_id: Some(u32::try_from(job_id).unwrap_or(u32::MAX)),
            printer,
        })
    }

    fn cancel_submitted(&self, job: &crate::SubmittedJob) -> Result<(), PlatformError> {
        if job.job_id.is_none() {
            return Err(PlatformError::CancelUnsupported);
        }
        self.cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(job.clone());
        Ok(())
    }

    fn cancel_submitted_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_job_without_a_spooler_id_cannot_be_cancelled() {
        use crate::PrinterBackend as _;

        let backend = FakePrinterBackend::new();
        let job = crate::SubmittedJob {
            job_id: None,
            printer: "fake-printer".to_owned(),
        };
        assert!(matches!(
            backend.cancel_submitted(&job),
            Err(PlatformError::CancelUnsupported)
        ));
        assert!(backend.cancelled().is_empty());
    }

    #[test]
    fn returns_fixed_pdf_bytes_and_counts_calls() {
        let host = FakeRenderHost::new();
        let spec = RenderSpec::new(210.0, 297.0);
        let pdf = futures::executor::block_on(host.render_to_pdf("<html></html>", &spec)).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        assert_eq!(host.calls(), 1);
    }

    #[test]
    fn delay_is_applied() {
        let host = FakeRenderHost::new().with_delay(Duration::from_millis(30));
        let spec = RenderSpec::new(210.0, 297.0);
        let started = Instant::now();
        futures::executor::block_on(host.render_to_pdf("<html></html>", &spec)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    #[should_panic(expected = "injected render failure")]
    fn injected_panic_propagates() {
        let host = FakeRenderHost::new().with_panic();
        let spec = RenderSpec::new(210.0, 297.0);
        let _ = futures::executor::block_on(host.render_to_pdf("<html></html>", &spec));
    }

    #[test]
    fn marker_delay_is_parsed_and_bounded() {
        assert_eq!(
            marker_delay("x fake-render-delay:25 y"),
            Some(Duration::from_millis(25))
        );
        assert_eq!(marker_delay("fake-render-delay:"), None);
        assert_eq!(marker_delay("no marker"), None);
        assert_eq!(
            marker_delay("fake-render-delay:999999999"),
            Some(Duration::from_millis(MAX_MARKER_DELAY_MS))
        );
    }

    #[test]
    #[should_panic(expected = "injected render failure")]
    fn always_crash_marker_panics() {
        let host = FakeRenderHost::new();
        let spec = RenderSpec::new(210.0, 297.0);
        let _ = futures::executor::block_on(host.render_to_pdf(CRASH_ALWAYS_MARKER, &spec));
    }
}
