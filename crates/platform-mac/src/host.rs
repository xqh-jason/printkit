//! WKWebView rendering on the main thread.
//!
//! Two constraints shape this module:
//!
//! 1. Every WKWebView call must happen on the main thread. A `RenderHost` can be
//!    called from any thread, so the work is handed to the main loop when
//!    needed.
//! 2. `createPDF` captures whatever the web page currently shows. Calling it
//!    before `didFinishNavigation` yields a blank page, so navigation is awaited
//!    explicitly and a timeout is an error, never a silent empty PDF.
//!
//! A WKWebView does not need a window on macOS, which is what makes headless
//! PDF export possible. Each render creates its own instance.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use block2::RcBlock;
use bytes::Bytes;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask, NSImage};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSData, NSDate, NSDefaultRunLoopMode, NSError, NSNumber, NSProcessInfo, NSRunLoop, NSString,
    NSThread,
};
use objc2_web_kit::{
    WKContentWorld, WKNavigation, WKNavigationAction, WKNavigationActionPolicy,
    WKNavigationDelegate, WKPDFConfiguration, WKSnapshotConfiguration, WKWebView,
    WKWebViewConfiguration,
};
use printkit_platform::{MM_TO_PT, PlatformError, RenderHost, RenderSpec, RowHeights, WebviewInfo};

/// Navigation and PDF capture each get this long before the render fails.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Slack added after a job's own budget before the waiting thread gives up.
///
/// The inner deadline is authoritative; this only catches a main loop that
/// stopped pumping, so a job is never abandoned while it is still running on
/// schedule.
const MAIN_THREAD_GRACE: Duration = Duration::from_secs(5);

/// Main-thread WKWebView renderer.
#[derive(Debug, Clone)]
pub struct MacRenderHost {
    timeout: Duration,
}

impl MacRenderHost {
    /// A host with the default timeout of 10 seconds per stage: one navigation
    /// plus one PDF capture per page.
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// A host with a custom per-stage timeout.
    ///
    /// A render of `pages` pages may occupy the main thread for up to
    /// `(pages + 1) * timeout`; the waiting thread gives up five seconds later
    /// so a stopped main loop is still detected.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for MacRenderHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Wall-clock budget one render may occupy the main thread.
///
/// Navigation gets one `timeout`; every page capture shares the rest. A
/// document with many pages therefore cannot multiply the per-stage timeout
/// without bound: 200 pages at 10 s each stay inside this deadline instead of
/// occupying the main thread for half an hour.
fn render_budget(timeout: Duration, pages: usize) -> Duration {
    let stages = u32::try_from(pages.max(1))
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    timeout.saturating_mul(stages)
}

/// Wait this long for a main-thread job whose inner deadline is `inner`.
fn outer_deadline(inner: Duration) -> Duration {
    inner.saturating_add(MAIN_THREAD_GRACE)
}

#[async_trait]
impl RenderHost for MacRenderHost {
    async fn render_to_pdf(&self, html: &str, spec: &RenderSpec) -> Result<Bytes, PlatformError> {
        let html = html.to_owned();
        let pages = spec.pages;
        let spec = spec.clone();
        let timeout = self.timeout;
        let deadline = outer_deadline(render_budget(timeout, pages));
        let pdf = run_on_main(deadline, move |cancel| {
            render_blocking(&html, spec, timeout, cancel)
        })?;
        Ok(Bytes::from(pdf))
    }

    async fn measure_rows(
        &self,
        html: &str,
        spec: &RenderSpec,
    ) -> Result<RowHeights, PlatformError> {
        let html = html.to_owned();
        let spec = spec.clone();
        let timeout = self.timeout;
        let deadline = outer_deadline(render_budget(timeout, 1));
        run_on_main(deadline, move |cancel| {
            measure_blocking(&html, spec, timeout, cancel)
        })
    }

    fn preview_available(&self) -> bool {
        super::preview::available()
    }

    async fn show_window(
        &self,
        request: printkit_platform::PreviewRequest,
    ) -> Result<printkit_platform::WindowId, PlatformError> {
        let timeout = self.timeout;
        run_on_main(outer_deadline(timeout), move |_| {
            super::preview::show_window(request, timeout)
        })
    }

    async fn window_png(
        &self,
        window: printkit_platform::WindowId,
        page: u32,
        dpi: u32,
    ) -> Result<Bytes, PlatformError> {
        let deadline = outer_deadline(self.timeout);
        run_on_main(deadline, move |_| {
            super::preview::window_png(window, page, dpi)
        })
    }

    async fn close_window(&self, window: printkit_platform::WindowId) -> Result<(), PlatformError> {
        let deadline = outer_deadline(self.timeout);
        run_on_main(deadline, move |_| super::preview::close_window(window))
    }
}

/// Renderer identity for `printkit doctor`.
///
/// WKWebView ships with the operating system, so the OS version is the version
/// that matters.
pub fn webview_info() -> WebviewInfo {
    let version = NSProcessInfo::processInfo()
        .operatingSystemVersionString()
        .to_string();
    WebviewInfo {
        engine: "WKWebView".to_owned(),
        available: true,
        version: Some(version),
    }
}

/// Work waiting for the main loop to pick it up.
///
/// A dispatch to the main queue cannot be used here: WKWebView delivers its
/// delegate callbacks through the serial main queue, and a render running
/// inside a queued block would stall those callbacks until it returned. The
/// job is stored instead, so the main loop can run it at top level between
/// run loop slices.
static PENDING: Mutex<VecDeque<Box<dyn FnOnce() + Send>>> = Mutex::new(VecDeque::new());

fn pending() -> std::sync::MutexGuard<'static, VecDeque<Box<dyn FnOnce() + Send>>> {
    PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cooperative cancellation for work queued on the main thread.
///
/// The waiting thread sets it when its outer deadline expires, which only
/// happens when the main loop stopped pumping. A job that has not started yet
/// is then dropped instead of running for a caller that is no longer listening,
/// and a render checks it between page captures.
pub(crate) struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A token that is never set, for work already running on the main thread.
    fn never() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Run `work` on the main thread, waiting at most `deadline` for it.
///
/// The job claims its result through a channel, so a panic inside it is
/// reported as [`PlatformError::Render`] instead of being mistaken for a
/// timeout. When the wait gives up, the job's [`Cancel`] token is set: queued
/// work that has not started is skipped, and a render stops between pages.
fn run_on_main<T, F>(deadline: Duration, work: F) -> Result<T, PlatformError>
where
    T: Send + 'static,
    F: FnOnce(&Cancel) -> Result<T, PlatformError> + Send + 'static,
{
    if NSThread::isMainThread_class() {
        return work(&Cancel::never());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let job_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::sync_channel(1);
    pending().push_back(Box::new(move || {
        if job_cancelled.load(Ordering::SeqCst) {
            // The caller already gave up, so running this would only keep the
            // main thread busy for a result nobody reads.
            return;
        }
        let cancel = Cancel(job_cancelled);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&cancel)));
        let result = match outcome {
            Ok(result) => result,
            Err(payload) => Err(PlatformError::Render(format!(
                "host panic: {}",
                panic_message(payload.as_ref())
            ))),
        };
        // The receiver can disappear if the caller gave up; dropping the result
        // is fine because nothing else owns it.
        let _ = sender.send(result);
    }));
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => {
            cancelled.store(true, Ordering::SeqCst);
            Err(PlatformError::Timeout(timeout_millis(deadline)))
        }
    }
}

/// Text carried by a panic payload, for a caller that must report it as an
/// error rather than let it escape the host.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

fn run_pending_jobs() {
    loop {
        let job = pending().pop_front();
        match job {
            Some(job) => {
                // The job reports its own panic through its channel; this only
                // keeps a panic before that report from killing the host.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
            None => return,
        }
    }
}

/// Create one WKWebView, configure it and start loading `html`.
///
/// PDF capture and row measurement share this setup so both lay the document
/// out at exactly the same millimetre geometry.
fn load_webview(
    html: &str,
    spec: &RenderSpec,
) -> Result<(Retained<WKWebView>, Retained<NavigationWatcher>), PlatformError> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(PlatformError::Unavailable(
            "WKWebView must be used from the main thread".to_owned(),
        ));
    };

    // Creating the shared application also keeps WebKit's XPC services alive
    // when the process has no NSApplicationMain loop. The activation policy
    // follows whether preview windows are open: a headless render must not put
    // a Dock icon on screen, and a window must not be hidden by it.
    ensure_application(mtm);

    // SAFETY: called on the main thread, as `new` requires.
    let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
    // JavaScript is off before the document is loaded: the sanitiser removes
    // scripts, and the renderer refuses to run any that survive. Window opening
    // is disabled for the same reason. `evaluateJavaScript` from the host still
    // works with this setting: it is app code, not page script.
    // SAFETY: main thread, freshly created configuration.
    unsafe {
        configuration
            .defaultWebpagePreferences()
            .setAllowsContentJavaScript(false);
        configuration
            .preferences()
            .setJavaScriptCanOpenWindowsAutomatically(false);
    }
    // The whole document is laid out inside one tall view: `createPDF` captures
    // a rectangle of the layout, and the rectangle origin selects the page.
    let frame = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(
            spec.content_width_mm() * MM_TO_PT,
            spec.content_height_mm() * MM_TO_PT,
        ),
    );
    // SAFETY: the marker proves we are on the main thread, which is what
    // WKWebView requires; the configuration outlives the initialiser call.
    let webview = unsafe {
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &configuration)
    };
    // CSS lays millimetres out at 96 dpi while `createPDF` maps one CSS pixel
    // to one PDF point (72 dpi). Zooming by CSS_PX_TO_PT keeps millimetre
    // geometry exact: 1 mm = 3.7795 px x 0.75 = 2.8346 pt.
    // SAFETY: main thread; the web view is live.
    unsafe { webview.setPageZoom(printkit_platform::CSS_PX_TO_PT) };

    let watcher = NavigationWatcher::new(mtm);
    // SAFETY: the delegate is a live object and the property is weak, so it must
    // stay alive until the caller is done - it does, in the returned tuple.
    unsafe { webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*watcher))) };

    let ns_html = NSString::from_str(html);
    // SAFETY: main thread, live web view, `baseURL = nil` keeps the page from
    // resolving anything outside the inline document.
    unsafe { webview.loadHTMLString_baseURL(&ns_html, None) };
    Ok((webview, watcher))
}

fn wait_for_paint(
    webview: &WKWebView,
    mtm: MainThreadMarker,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let slot: TextSlot = Rc::new(RefCell::new(None));
    let block_slot = Rc::clone(&slot);
    let completion = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let result = if !error.is_null() {
            // SAFETY: WebKit hands over a valid NSError for the callback's
            // duration; the description is copied out before returning.
            let error = unsafe { &*error };
            Err(error.localizedDescription().to_string())
        } else if value.is_null() {
            Err("the paint barrier returned no value".to_owned())
        } else {
            // SAFETY: the object is alive for the callback's duration; the
            // string is copied out before returning.
            let value = unsafe { &*value };
            match value.downcast_ref::<NSString>() {
                Some(text) => Ok(text.to_string()),
                None => Err("the paint barrier did not return text".to_owned()),
            }
        };
        *block_slot.borrow_mut() = Some(result);
    });
    let script = NSString::from_str(PAINT_SCRIPT);
    let world = unsafe { WKContentWorld::defaultClientWorld(mtm) };
    // SAFETY: main thread, live web view, client content world; the block is
    // copied to the heap and completes only after the promise resolves.
    unsafe {
        webview.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &script,
            None,
            None,
            &world,
            Some(&completion),
        )
    };
    wait_for_text(&slot, timeout)?;
    Ok(())
}

/// Async function body: wait for every inline image to load and decode, then
/// wait two animation frames so the paint reaches the layer `createPDF`
/// captures.
///
/// `Image.complete` says the load finished; `decode()` only starts after that.
/// Calling `decode()` on an image that is still loading rejects immediately,
/// and swallowing that rejection exports a white background. A failed image
/// resolves its `error` event, so this barrier cannot hang forever; a missing
/// image is still visible as a blank image rather than delaying the render.
/// The `setTimeout` fallback keeps an off-screen web view (where
/// requestAnimationFrame can be throttled) from waiting forever.
const PAINT_SCRIPT: &str = r#"
const images = Array.from(document.images);
await Promise.all(images.map(async (image) => {
  if (!image.complete) {
    await new Promise((resolve) => {
      image.addEventListener('load', resolve, { once: true });
      image.addEventListener('error', resolve, { once: true });
    });
  }
  if (image.decode) {
    await image.decode().catch(() => {});
  }
}));
await new Promise((resolve) => {
  let done = false;
  const finish = () => {
    if (!done) { done = true; resolve(); }
  };
  requestAnimationFrame(() => requestAnimationFrame(finish));
  setTimeout(finish, 50);
});
return '1';
"#;

/// JavaScript that returns one `key<TAB>height` line per table row, in CSS
/// pixels. Tab-separated text avoids pulling a JSON parser into the host.
const MEASURE_SCRIPT: &str = r#"
(function () {
  var rows = document.querySelectorAll('tr[data-row-key]');
  var out = '';
  for (var i = 0; i < rows.length; i += 1) {
    var row = rows[i];
    out += row.getAttribute('data-row-key') + '\t' +
      row.getBoundingClientRect().height + '\n';
  }
  return out;
}())
"#;

fn measure_blocking(
    html: &str,
    spec: RenderSpec,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<RowHeights, PlatformError> {
    if cancel.is_cancelled() {
        return Err(PlatformError::Render(
            "render abandoned by the caller".to_owned(),
        ));
    }
    let budget = render_budget(timeout, 1);
    let started = Instant::now();
    let (webview, watcher) = load_webview(html, &spec)?;
    wait_for_navigation(&watcher, timeout)?;

    let slot: TextSlot = Rc::new(RefCell::new(None));
    let block_slot = Rc::clone(&slot);
    let completion = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let result = if !error.is_null() {
            // SAFETY: WebKit hands over a valid NSError for the callback's
            // duration; the description is copied out before returning.
            let error = unsafe { &*error };
            Err(error.localizedDescription().to_string())
        } else if value.is_null() {
            Err("the measurement script returned no value".to_owned())
        } else {
            // SAFETY: the object is alive for the callback's duration; the
            // string is copied out before returning.
            let value = unsafe { &*value };
            match value.downcast_ref::<NSString>() {
                Some(text) => Ok(text.to_string()),
                None => Err("the measurement script did not return text".to_owned()),
            }
        };
        *block_slot.borrow_mut() = Some(result);
    });
    let script = NSString::from_str(MEASURE_SCRIPT);
    // SAFETY: main thread, live web view; the block is copied to the heap by
    // RcBlock and stays alive until this scope ends.
    unsafe { webview.evaluateJavaScript_completionHandler(&script, Some(&completion)) };
    let remaining = budget.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(PlatformError::Timeout(timeout_millis(budget)));
    }
    let text = wait_for_text(&slot, remaining)?;
    parse_row_heights(&text)
}

/// Parse `key<TAB>height` lines from CSS pixels into millimetres.
fn parse_row_heights(text: &str) -> Result<RowHeights, PlatformError> {
    let mut heights = RowHeights::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else {
            continue;
        };
        let Ok(height) = value.trim().parse::<f64>() else {
            continue;
        };
        if key.is_empty() || !height.is_finite() || height <= 0.0 {
            continue;
        }
        heights.insert(key.to_owned(), height * printkit_platform::CSS_PX_TO_MM);
    }
    if heights.is_empty() {
        return Err(PlatformError::Render(
            "the document exposed no measurable table rows".to_owned(),
        ));
    }
    Ok(heights)
}

fn render_blocking(
    html: &str,
    spec: RenderSpec,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<Vec<u8>, PlatformError> {
    let pages = spec.pages.max(1);
    let budget = render_budget(timeout, pages);
    let started = Instant::now();
    if cancel.is_cancelled() {
        return Err(PlatformError::Render(
            "render abandoned by the caller".to_owned(),
        ));
    }
    let (webview, watcher) = load_webview(html, &spec)?;
    wait_for_navigation(&watcher, timeout)?;
    // Navigation finishing does not mean every inline image has painted.
    // Decode the images and wait for a real frame before createPDF; otherwise
    // a loaded runner can capture a white background.
    // SAFETY: called on the main thread, as `new` requires.
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(PlatformError::Unavailable(
            "WKWebView must be used from the main thread".to_owned(),
        ));
    };
    wait_for_paint(&webview, mtm, timeout)?;
    // A completed image decode is still not proof that WebKit's compositor
    // painted the page. Take a real snapshot after screen updates before
    // createPDF; a white or missing snapshot is an explicit render error, not
    // a silently blank PDF.
    let snapshot_configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
    // A tiny snapshot width keeps a 200-page document from allocating a huge
    // image while still forcing the whole laid-out page to paint.
    let snapshot_width = NSNumber::new_f64(64.0);
    // SAFETY: main thread; the configuration is live and copied by WebKit.
    unsafe {
        snapshot_configuration.setAfterScreenUpdates(true);
        snapshot_configuration.setSnapshotWidth(Some(&snapshot_width));
    }
    let remaining = budget.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(PlatformError::Timeout(timeout_millis(budget)));
    }
    wait_for_snapshot(&webview, &snapshot_configuration, remaining)?;
    let configuration = unsafe { WKPDFConfiguration::new(mtm) };
    let mut parts = Vec::with_capacity(pages);
    for page in 0..pages {
        // A caller that gave up must not keep the main thread rendering pages
        // nobody will read; the abandoned job stops at the next page boundary.
        if cancel.is_cancelled() {
            return Err(PlatformError::Render(
                "render abandoned by the caller".to_owned(),
            ));
        }
        // Pages stack in layout order; a mixed-size document advances by the
        // previous page's own height, not by a uniform stride.
        let (width_mm, height_mm) = spec.size_mm(page);
        let rect = CGRect::new(
            CGPoint::new(0.0, spec.offset_mm(page) * MM_TO_PT),
            CGSize::new(width_mm * MM_TO_PT, height_mm * MM_TO_PT),
        );
        // SAFETY: `rect` only stores a value.
        unsafe { configuration.setRect(rect) };

        let slot: PdfSlot = Rc::new(RefCell::new(None));
        let block_slot = Rc::clone(&slot);
        let completion = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: WebKit hands over a valid NSError for the duration of
                // the callback; the string is copied out before returning.
                let error = unsafe { &*error };
                Err(error.localizedDescription().to_string())
            } else if data.is_null() {
                Err("createPDF completed without data".to_owned())
            } else {
                // SAFETY: same as above, but for NSData.
                let data = unsafe { &*data };
                Ok(data.to_vec())
            };
            *block_slot.borrow_mut() = Some(result);
        });
        // SAFETY: main thread, live web view and configuration; the block is
        // copied to the heap by RcBlock and stays alive until this scope ends.
        unsafe {
            webview.createPDFWithConfiguration_completionHandler(Some(&configuration), &completion)
        };
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(PlatformError::Timeout(timeout_millis(budget)));
        }
        parts.push(wait_for_pdf(&slot, remaining)?);
    }

    super::merge::merge_pages(&parts, &spec)
}

/// Filled by the `evaluateJavaScript` completion handler.
type TextSlot = Rc<RefCell<Option<Result<String, String>>>>;

/// Filled by the `createPDF` completion handler, which runs on the main
/// thread; a single-threaded cell is therefore enough.
type PdfSlot = Rc<RefCell<Option<Result<Vec<u8>, String>>>>;

pub(crate) fn wait_for_navigation(
    watcher: &NavigationWatcher,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let deadline = Instant::now() + timeout;
    loop {
        match watcher.phase() {
            Phase::Finished => return Ok(()),
            Phase::Failed => {
                return Err(PlatformError::Render(
                    watcher
                        .error()
                        .unwrap_or_else(|| "navigation failed".to_owned()),
                ));
            }
            Phase::Loading => {}
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout(timeout_millis(timeout)));
        }
        pump_once();
    }
}

fn wait_for_pdf(slot: &PdfSlot, timeout: Duration) -> Result<Vec<u8>, PlatformError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(result) = slot.borrow_mut().take() {
            return result.map_err(PlatformError::Render);
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout(timeout_millis(timeout)));
        }
        pump_once();
    }
}

fn wait_for_text(slot: &TextSlot, timeout: Duration) -> Result<String, PlatformError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(result) = slot.borrow_mut().take() {
            return result.map_err(PlatformError::Render);
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout(timeout_millis(timeout)));
        }
        pump_once();
    }
}

fn wait_for_snapshot(
    webview: &WKWebView,
    configuration: &WKSnapshotConfiguration,
    timeout: Duration,
) -> Result<(), PlatformError> {
    let slot: TextSlot = Rc::new(RefCell::new(None));
    let block_slot = Rc::clone(&slot);
    let completion = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let result = if !error.is_null() {
            // SAFETY: WebKit hands over a valid NSError for the callback's
            // duration; the description is copied out before returning.
            let error = unsafe { &*error };
            Err(error.localizedDescription().to_string())
        } else if image.is_null() {
            Err("takeSnapshot completed without an image".to_owned())
        } else {
            Ok("1".to_owned())
        };
        *block_slot.borrow_mut() = Some(result);
    });
    // SAFETY: main thread, live web view and copied configuration; the block
    // is copied to the heap by RcBlock and completes after the paint barrier.
    unsafe {
        webview.takeSnapshotWithConfiguration_completionHandler(Some(configuration), &completion)
    };
    wait_for_text(&slot, timeout).map(|_| ())
}

/// Let the main run loop process WebKit callbacks for a short slice.
pub(crate) fn pump_once() {
    let run_loop = NSRunLoop::currentRunLoop();
    let limit = NSDate::dateWithTimeIntervalSinceNow(0.01);
    // SAFETY: `NSDefaultRunLoopMode` is an immutable framework constant.
    run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &limit);
}

/// Keep the main run loop alive while `keep_running` returns true.
///
/// WebKit callbacks and anything dispatched to the main queue only run while
/// this loop is pumping. A long-running process whose async runtime lives on
/// worker threads must call this on the main thread, otherwise every render
/// waits forever for a queue that never drains.
pub fn run_main_loop_while(keep_running: &mut dyn FnMut() -> bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        pending().clear();
        return;
    };
    loop {
        // Jobs run at top level, never inside a queued block, so the nested
        // run loop inside a render can still drain WebKit's callbacks.
        run_pending_jobs();
        if !keep_running() {
            break;
        }
        let limit = NSDate::dateWithTimeIntervalSinceNow(0.01);
        if super::preview::has_windows() {
            let app = NSApplication::sharedApplication(mtm);
            // `NSApplication` owns event dispatch; a plain run-loop slice never
            // delivers a click to a preview window's buttons.
            let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&limit),
                // SAFETY: `NSDefaultRunLoopMode` is an immutable framework constant.
                unsafe { NSDefaultRunLoopMode },
                true,
            );
            if let Some(event) = event {
                app.sendEvent(&event);
            }
            app.updateWindows();
        } else {
            // No window is open, so the process may be a remote session with no
            // window server; a plain run-loop slice needs no `NSApplication`.
            let run_loop = NSRunLoop::mainRunLoop();
            // SAFETY: `NSDefaultRunLoopMode` is an immutable framework constant.
            run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &limit);
        }
    }
    // Dropping an unstarted job closes its channel, so a caller waiting on a
    // loop that has stopped gets an error instead of hanging.
    pending().clear();
}

/// Keep the shared `NSApplication` policy in sync with the preview windows.
///
/// A headless render must not show a Dock icon; a live preview window must not
/// be deactivated or hidden by the policy either. The launch sequence is not
/// finished here: a headless render must keep working without a window server.
pub(crate) fn ensure_application(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    let policy = if super::preview::has_windows() {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Prohibited
    };
    let _ = app.setActivationPolicy(policy);
}

/// Prepare the application for a window that does not exist yet.
///
/// The policy changes while the process still has no windows. Switching it
/// afterwards would animate an already visible window, which WebKit reads as
/// an occluded surface and stops painting. A bare CLI process has no bundle
/// launch sequence; finishing it is what makes event dispatch work.
pub(crate) fn prepare_window_application(mtm: MainThreadMarker) {
    static LAUNCHED: AtomicBool = AtomicBool::new(false);
    let app = NSApplication::sharedApplication(mtm);
    if !LAUNCHED.swap(true, Ordering::SeqCst) {
        app.finishLaunching();
    }
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
}

/// Whether this process belongs to a graphical login session.
///
/// A daemon or an SSH-only session has no window server, so creating a window
/// would either fail or show nothing. The CoreGraphics session dictionary is
/// how the system answers that question. The call is thread-agnostic, which
/// lets the service advertise the capability from a worker thread; the GUI
/// thread requirement belongs to window creation itself.
pub(crate) fn gui_session_available() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> *const std::ffi::c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const std::ffi::c_void);
    }
    // SAFETY: the copy rule transfers ownership of a non-null dictionary;
    // releasing it keeps one question from leaking one dictionary.
    unsafe {
        let session = CGSessionCopyCurrentDictionary();
        if session.is_null() {
            return false;
        }
        CFRelease(session);
    }
    true
}

fn timeout_millis(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    #[default]
    Loading,
    Finished,
    Failed,
}

#[derive(Debug, Default)]
pub(crate) struct NavigationIvars {
    phase: Cell<Phase>,
    error: RefCell<Option<String>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // Drop implementation.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationIvars]
    pub(crate) struct NavigationWatcher;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for NavigationWatcher {}

    // SAFETY: the delegate protocol has no safety requirements beyond running
    // on the main thread, which the class thread kind declares.
    unsafe impl WKNavigationDelegate for NavigationWatcher {
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.ivars().phase.set(Phase::Finished);
        }

        // Load the string once; refuse every later navigation. Meta refresh,
        // links and form submission are removed by the sanitizer, and this is
        // the last line that stops a surviving one from leaving the page.
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy(
            &self,
            _web_view: &WKWebView,
            navigation_action: &WKNavigationAction,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // SAFETY: both objects live for the duration of the callback.
            let url = unsafe { navigation_action.request().URL() };
            let allowed = url
                .as_ref()
                .and_then(|url| url.absoluteString())
                .is_none_or(|value| value.to_string() == "about:blank");
            decision_handler.call((if allowed {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.fail(error);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.fail(error);
        }
    }
);

impl NavigationWatcher {
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavigationIvars::default());
        // SAFETY: NSObject's `init` returns an initialised instance; the ivars
        // were set above.
        unsafe { msg_send![super(this), init] }
    }

    pub(crate) fn phase(&self) -> Phase {
        self.ivars().phase.get()
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.ivars().error.borrow().clone()
    }

    fn fail(&self, error: &NSError) {
        *self.ivars().error.borrow_mut() = Some(error.localizedDescription().to_string());
        self.ivars().phase.set(Phase::Failed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// The pending queue is process-wide, so every test that pumps it runs one
    /// at a time and starts from an empty queue.
    static QUEUE: Mutex<()> = Mutex::new(());

    fn empty_queue() -> std::sync::MutexGuard<'static, ()> {
        let guard = QUEUE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending().clear();
        guard
    }

    /// Wait until the caller thread has queued its job, then run it.
    fn pump_queued_jobs() {
        let started = Instant::now();
        while pending().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the caller never queued a job"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        run_pending_jobs();
    }

    #[test]
    fn a_panicking_job_is_reported_as_a_render_error() {
        let _guard = empty_queue();
        let caller = std::thread::spawn(|| {
            run_on_main(Duration::from_secs(30), |_| -> Result<(), PlatformError> {
                panic!("webkit exploded")
            })
            .unwrap_err()
        });
        pump_queued_jobs();
        let error = caller.join().unwrap();
        match error {
            PlatformError::Render(message) => {
                assert!(message.contains("host panic: webkit exploded"), "{message}");
            }
            other => panic!("expected a render error, got {other:?}"),
        }
    }

    #[test]
    fn a_job_that_outlives_the_deadline_is_dropped_before_it_runs() {
        let _guard = empty_queue();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let caller = std::thread::spawn(move || {
            run_on_main(Duration::from_millis(20), move |_| {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
        });
        let error = caller.join().unwrap().unwrap_err();
        assert!(matches!(error, PlatformError::Timeout(_)), "{error:?}");
        run_pending_jobs();
        assert!(
            !ran.load(Ordering::SeqCst),
            "the abandoned job still ran on the main thread"
        );
    }

    #[test]
    fn a_job_returns_its_value_through_the_queue() {
        let _guard = empty_queue();
        let caller = std::thread::spawn(|| run_on_main(Duration::from_secs(5), |_| Ok(7_u32)));
        pump_queued_jobs();
        assert_eq!(caller.join().unwrap().unwrap(), 7);
    }

    #[test]
    fn the_outer_deadline_exceeds_the_render_budget() {
        // 200 pages at the default 10 s timeout: one navigation plus one
        // capture per page. The wait must outlast the whole budget, otherwise
        // a long document would be abandoned while still on schedule.
        let budget = render_budget(DEFAULT_TIMEOUT, 200);
        assert_eq!(budget, DEFAULT_TIMEOUT.saturating_mul(201));
        assert!(outer_deadline(budget) > budget);
        assert_eq!(
            render_budget(DEFAULT_TIMEOUT, 0),
            DEFAULT_TIMEOUT.saturating_mul(2)
        );
    }
}
