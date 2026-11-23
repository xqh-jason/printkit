//! Native preview windows.
//!
//! Windows are created, resized and destroyed only on the GUI thread. The
//! service talks to them through [`super::MacRenderHost`]; every public entry
//! lands in [`run_on_main`](super::host::run_on_main) and returns to the caller
//! on its own thread.
//!
//! The web view receives exactly the HTML the print path renders. Exporting a
//! page takes a snapshot of that same view, so a PNG cannot drift from what
//! the user sees, and no second layout exists.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use bytes::Bytes;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSBackingStoreType, NSBitmapImageFileType, NSBitmapImageRep, NSButton, NSColor,
    NSDeviceRGBColorSpace, NSGraphicsContext, NSImage, NSScreen, NSScrollView, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSError, NSNotification, NSNumber, NSRunLoop,
    NSString,
};
use objc2_web_kit::{WKSnapshotConfiguration, WKWebView, WKWebViewConfiguration};
use printkit_platform::{
    MM_TO_PT, PlatformError, PreviewActions, PreviewRequest, RenderSpec, WindowId,
};

use super::host::{
    NavigationWatcher, ensure_application, gui_session_available, prepare_window_application,
    wait_for_navigation,
};

const TOOLBAR_HEIGHT: f64 = 32.0;
const WINDOW_MIN_WIDTH: f64 = 420.0;
const MIN_ZOOM: f64 = 0.25;
const MAX_ZOOM: f64 = 3.0;
/// A page snapshot is a capture of an already rendered view; a slow machine
/// must fail loudly instead of returning a blank image.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);
static OPEN_WINDOWS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static WINDOWS: RefCell<HashMap<WindowId, PreviewWindow>> = RefCell::new(HashMap::new());
}

struct PreviewWindow {
    window: Retained<NSWindow>,
    container: Retained<NSView>,
    toolbar: Retained<NSView>,
    scroll: Retained<NSScrollView>,
    webview: Retained<WKWebView>,
    navigation: Retained<NavigationWatcher>,
    target: Retained<PreviewTarget>,
    delegate: Retained<WindowDelegate>,
    spec: RenderSpec,
    /// Page currently aligned with the top of the viewport, zero-based.
    page: Cell<usize>,
}

/// Whether any preview window is alive in this process.
pub(crate) fn has_windows() -> bool {
    OPEN_WINDOWS.load(Ordering::Acquire) > 0
}

/// Whether a window can be shown at all.
///
/// Called from the service's worker threads when capabilities are reported,
/// so it must not require the GUI thread; `show_window` checks that separately.
pub(crate) fn available() -> bool {
    gui_session_available()
}

pub(crate) fn show_window(
    request: PreviewRequest,
    timeout: Duration,
) -> Result<WindowId, PlatformError> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(PlatformError::Unavailable(
            "a preview window needs the GUI thread".to_owned(),
        ));
    };
    if !gui_session_available() {
        return Err(PlatformError::Unavailable(
            "no graphical login session is attached to this process".to_owned(),
        ));
    }
    if request.window.is_some() {
        ensure_application(mtm);
    }

    if let Some(id) = request.window {
        let existing = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
        let Some(mut window) = existing else {
            return Err(PlatformError::WindowNotFound(id.to_string()));
        };
        if let Err(error) = window.update(request, timeout) {
            // The old content is gone and the new one failed to load; keep the
            // window count honest instead of leaving a half-updated entry.
            window.window.close();
            OPEN_WINDOWS.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        WINDOWS.with(|windows| {
            windows.borrow_mut().insert(id, window);
        });
        return Ok(id);
    }

    let id = WindowId::new(NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    prepare_window_application(mtm);
    let window = PreviewWindow::create(mtm, id, request, timeout)?;
    OPEN_WINDOWS.fetch_add(1, Ordering::AcqRel);
    WINDOWS.with(|windows| {
        windows.borrow_mut().insert(id, window);
    });
    activate(mtm);
    Ok(id)
}

pub(crate) fn window_png(id: WindowId, page: u32, dpi: u32) -> Result<Bytes, PlatformError> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(PlatformError::Unavailable(
            "a preview window needs the GUI thread".to_owned(),
        ));
    };
    let _ = mtm;
    let (webview, spec) = WINDOWS.with(|windows| {
        windows
            .borrow()
            .get(&id)
            .map(|window| (window.webview.clone(), window.spec.clone()))
            .ok_or_else(|| PlatformError::WindowNotFound(id.to_string()))
    })?;
    if usize::try_from(page).map_or(true, |page| page >= spec.pages) {
        return Err(PlatformError::Render(format!(
            "page {page} is outside {} pages",
            spec.pages
        )));
    }
    let scale = webview
        .window()
        .map(|window| window.backingScaleFactor())
        .unwrap_or(1.0);
    snapshot_page(&webview, spec, page, dpi, scale)
}

pub(crate) fn close_window(id: WindowId) -> Result<(), PlatformError> {
    let window = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    let Some(window) = window else {
        return Err(PlatformError::WindowNotFound(id.to_string()));
    };
    OPEN_WINDOWS.fetch_sub(1, Ordering::AcqRel);
    // Closing can call the delegate synchronously; the entry is already out of
    // the map, so that path only reports the close to the service.
    window.window.close();
    if let Some(mtm) = MainThreadMarker::new() {
        ensure_application(mtm);
    }
    Ok(())
}

/// Close every window, used when a session or the service ends.
pub(crate) fn close_all() {
    let windows: Vec<PreviewWindow> = WINDOWS.with(|windows| {
        windows
            .borrow_mut()
            .drain()
            .map(|(_, window)| window)
            .collect()
    });
    OPEN_WINDOWS.store(0, Ordering::Release);
    for window in windows {
        window.window.close();
    }
    if let Some(mtm) = MainThreadMarker::new() {
        ensure_application(mtm);
    }
}

fn activate(mtm: MainThreadMarker) {
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.activate();
}

impl PreviewWindow {
    fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        request: PreviewRequest,
        timeout: Duration,
    ) -> Result<Self, PlatformError> {
        let spec = request.spec.clone();
        let (width, height) = window_size(&request.spec, mtm);
        let content = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        // SAFETY: called on the GUI thread, as the initialiser requires.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // The map owns the window; AppKit must not release it behind our back.
        // SAFETY: the window is live and this process retains it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(&window_title(&request.title)));

        let container = NSView::initWithFrame(NSView::alloc(mtm), content);
        window.setContentView(Some(&container));

        let toolbar = NSView::initWithFrame(
            NSView::alloc(mtm),
            CGRect::new(
                CGPoint::new(0.0, height - TOOLBAR_HEIGHT),
                CGSize::new(width, TOOLBAR_HEIGHT),
            ),
        );
        container.addSubview(&toolbar);

        let target = PreviewTarget::new(mtm, id, Arc::clone(&request.actions));
        for (title, selector, x) in [
            ("<", sel!(previousPage:), 8.0),
            (">", sel!(nextPage:), 68.0),
            ("-", sel!(zoomOut:), 136.0),
            ("+", sel!(zoomIn:), 196.0),
            ("Print", sel!(printWindow:), 264.0),
        ] {
            push_button(mtm, &toolbar, title, selector, x, &target)?;
        }

        let scroll_frame = CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width, height - TOOLBAR_HEIGHT),
        );
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), scroll_frame);
        scroll.setHasVerticalScroller(true);
        scroll.setAllowsMagnification(true);
        scroll.setMinMagnification(MIN_ZOOM);
        scroll.setMaxMagnification(MAX_ZOOM);
        container.addSubview(&scroll);

        let webview = make_webview(mtm, spec.clone())?;
        scroll.setDocumentView(Some(&webview));
        let delegate = WindowDelegate::new(mtm, id, Arc::clone(&request.actions));
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        window.center();
        window.makeKeyAndOrderFront(None);

        let navigation = NavigationWatcher::new(mtm);
        // SAFETY: the watcher is stored in the window record before the load
        // starts; the delegate property is weak.
        unsafe { webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation))) };
        webview.load_html(&request.html);
        wait_for_navigation(&navigation, timeout)?;

        let this = Self {
            window,
            container,
            toolbar,
            scroll,
            webview,
            navigation,
            target,
            delegate,
            spec,
            page: Cell::new(0),
        };
        this.show_page(0);
        Ok(this)
    }

    fn update(&mut self, request: PreviewRequest, timeout: Duration) -> Result<(), PlatformError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(PlatformError::Unavailable(
                "a preview window needs the GUI thread".to_owned(),
            ));
        };
        self.spec = request.spec;
        self.target.set_actions(Arc::clone(&request.actions));
        self.delegate.set_actions(request.actions);
        self.window
            .setTitle(&NSString::from_str(&window_title(&request.title)));
        self.layout(mtm);

        let navigation = NavigationWatcher::new(mtm);
        // SAFETY: the new watcher replaces the previous one in the same record
        // before the navigation begins.
        unsafe {
            self.webview
                .setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation)))
        };
        self.navigation = navigation;
        self.webview.load_html(&request.html);
        wait_for_navigation(&self.navigation, timeout)?;
        self.show_page(0);
        Ok(())
    }

    fn layout(&self, mtm: MainThreadMarker) {
        let (width, height) = window_size(&self.spec, mtm);
        let content = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height));
        self.window.setContentSize(CGSize::new(width, height));
        self.container.setFrame(content);
        self.toolbar.setFrame(CGRect::new(
            CGPoint::new(0.0, height - TOOLBAR_HEIGHT),
            CGSize::new(width, TOOLBAR_HEIGHT),
        ));
        self.scroll.setFrame(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width, height - TOOLBAR_HEIGHT),
        ));
        self.webview.setFrame(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(
                self.spec.content_width_mm() * MM_TO_PT,
                self.spec.content_height_mm() * MM_TO_PT,
            ),
        ));
    }

    fn show_page(&self, page: usize) {
        let page = page.min(self.spec.pages.saturating_sub(1));
        self.page.set(page);
        let (_width_mm, page_height_mm) = self.spec.size_mm(page);
        let offset_pt = self.spec.offset_mm(page) * MM_TO_PT;
        let document_height = self.spec.content_height_mm() * MM_TO_PT;
        // A flipped document view has its origin at the top; otherwise the
        // scroll view counts from the bottom, so page 0 sits at the far end.
        let y = if self.webview.isFlipped() {
            offset_pt
        } else {
            (document_height - offset_pt - page_height_mm * MM_TO_PT).max(0.0)
        };
        let clip = self.scroll.contentView();
        clip.scrollToPoint(CGPoint::new(0.0, y));
        self.scroll.reflectScrolledClipView(&clip);
    }

    fn zoom_by(&self, factor: f64) {
        let next = (self.scroll.magnification() * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.scroll.setMagnification(next);
    }
}

fn window_size(spec: &RenderSpec, mtm: MainThreadMarker) -> (f64, f64) {
    let width = (spec.content_width_mm() * MM_TO_PT).max(WINDOW_MIN_WIDTH);
    let screen_height = NSScreen::mainScreen(mtm)
        .map(|screen| screen.visibleFrame().size.height)
        .unwrap_or(900.0);
    let height = (spec.height_pt() + TOOLBAR_HEIGHT).min(screen_height * 0.9);
    (width, height)
}

fn window_title(title: &str) -> String {
    if title.is_empty() {
        "printkit preview".to_owned()
    } else {
        format!("printkit preview - {title}")
    }
}

fn push_button(
    mtm: MainThreadMarker,
    toolbar: &NSView,
    title: &str,
    selector: objc2::runtime::Sel,
    x: f64,
    target: &PreviewTarget,
) -> Result<(), PlatformError> {
    // SAFETY: the selector belongs to `PreviewTarget` and the target is alive
    // for at least as long as the button.
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(erase_target(target)),
            Some(selector),
            mtm,
        )
    };
    button.setFrame(CGRect::new(CGPoint::new(x, 4.0), CGSize::new(52.0, 24.0)));
    toolbar.addSubview(&button);
    Ok(())
}

fn make_webview(
    mtm: MainThreadMarker,
    spec: RenderSpec,
) -> Result<Retained<WKWebView>, PlatformError> {
    // SAFETY: called on the GUI thread, as `new` requires.
    let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
    // SAFETY: main thread, freshly created configuration.
    unsafe {
        configuration
            .defaultWebpagePreferences()
            .setAllowsContentJavaScript(false);
        configuration
            .preferences()
            .setJavaScriptCanOpenWindowsAutomatically(false);
    }
    let frame = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(
            spec.content_width_mm() * MM_TO_PT,
            spec.content_height_mm() * MM_TO_PT,
        ),
    );
    // SAFETY: main thread, the configuration outlives the initialiser call.
    let webview = unsafe {
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &configuration)
    };
    // CSS pixels are 96 dpi while AppKit points are 72 dpi; the same zoom as
    // the print path keeps millimetre geometry physical on screen.
    // SAFETY: main thread; the web view is live.
    unsafe { webview.setPageZoom(printkit_platform::CSS_PX_TO_PT) };
    Ok(webview)
}

trait WebviewLoad {
    fn load_html(&self, html: &str);
}

impl WebviewLoad for WKWebView {
    fn load_html(&self, html: &str) {
        let ns_html = NSString::from_str(html);
        // SAFETY: main thread, live web view, `baseURL = nil` keeps the page
        // from resolving anything outside the inline document.
        unsafe { self.loadHTMLString_baseURL(&ns_html, None) };
    }
}

struct PreviewTargetIvars {
    window: WindowId,
    actions: RefCell<Arc<dyn PreviewActions>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // Drop implementation.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PreviewTargetIvars]
    struct PreviewTarget;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for PreviewTarget {}

    impl PreviewTarget {
        #[unsafe(method(previousPage:))]
        fn previous_page(&self, _sender: Option<&AnyObject>) {
            let id = self.ivars().window;
            WINDOWS.with(|windows| {
                if let Some(window) = windows.borrow().get(&id) {
                    let page = current_page(window);
                    window.show_page(page.saturating_sub(1));
                }
            });
        }

        #[unsafe(method(nextPage:))]
        fn next_page(&self, _sender: Option<&AnyObject>) {
            let id = self.ivars().window;
            WINDOWS.with(|windows| {
                if let Some(window) = windows.borrow().get(&id) {
                    let page = (current_page(window) + 1).min(window.spec.pages.saturating_sub(1));
                    window.show_page(page);
                }
            });
        }

        #[unsafe(method(zoomIn:))]
        fn zoom_in(&self, _sender: Option<&AnyObject>) {
            self.adjust_zoom(1.25);
        }

        #[unsafe(method(zoomOut:))]
        fn zoom_out(&self, _sender: Option<&AnyObject>) {
            self.adjust_zoom(0.8);
        }

        #[unsafe(method(printWindow:))]
        fn print_window(&self, _sender: Option<&AnyObject>) {
            self.ivars().actions.borrow().print(self.ivars().window);
        }
    }
);

impl PreviewTarget {
    fn new(
        mtm: MainThreadMarker,
        window: WindowId,
        actions: Arc<dyn PreviewActions>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PreviewTargetIvars {
            window,
            actions: RefCell::new(actions),
        });
        // SAFETY: NSObject's `init` returns an initialised instance; the ivars
        // were set above.
        unsafe { msg_send![super(this), init] }
    }
    fn set_actions(&self, actions: Arc<dyn PreviewActions>) {
        *self.ivars().actions.borrow_mut() = actions;
    }

    fn adjust_zoom(&self, factor: f64) {
        let id = self.ivars().window;
        WINDOWS.with(|windows| {
            if let Some(window) = windows.borrow().get(&id) {
                window.zoom_by(factor);
            }
        });
    }
}

/// Erase a concrete class reference to `AnyObject` for an AppKit target slot.
fn erase_target(target: &PreviewTarget) -> &AnyObject {
    // SAFETY: every Objective-C object shares the same header; the reference
    // keeps the caller's lifetime.
    unsafe { &*(target as *const PreviewTarget as *const AnyObject) }
}

struct WindowDelegateIvars {
    window: WindowId,
    actions: RefCell<Arc<dyn PreviewActions>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // Drop implementation.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = WindowDelegateIvars]
    struct WindowDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for WindowDelegate {}

    // SAFETY: the delegate protocol requires main-thread calls, which the
    // thread kind declares.
    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let id = self.ivars().window;
            let removed = WINDOWS.with(|windows| windows.borrow_mut().remove(&id).is_some());
            if removed {
                OPEN_WINDOWS.fetch_sub(1, Ordering::AcqRel);
            }
            // A window closed by the service is already out of the map; the
            // service's removal is idempotent either way.
            self.ivars().actions.borrow().closed(id);
            if let Some(mtm) = MainThreadMarker::new() {
                ensure_application(mtm);
            }
        }
    }
);

impl WindowDelegate {
    fn new(
        mtm: MainThreadMarker,
        window: WindowId,
        actions: Arc<dyn PreviewActions>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowDelegateIvars {
            window,
            actions: RefCell::new(actions),
        });
        // SAFETY: NSObject's `init` returns an initialised instance; the ivars
        // were set above.
        unsafe { msg_send![super(this), init] }
    }

    fn set_actions(&self, actions: Arc<dyn PreviewActions>) {
        *self.ivars().actions.borrow_mut() = actions;
    }
}

/// Zero-based page currently aligned with the viewport.
fn current_page(window: &PreviewWindow) -> usize {
    if window.spec.pages == 0 {
        return window.page.get();
    }
    let clip = window.scroll.contentView();
    let y = clip.bounds().origin.y;
    let from_top = if window.webview.isFlipped() {
        y
    } else {
        (window.spec.content_height_mm() * MM_TO_PT - y).max(0.0)
    };
    page_at(&window.spec, from_top)
}

/// Page whose vertical band contains `from_top` (points from the layout top).
fn page_at(spec: &RenderSpec, from_top: f64) -> usize {
    for page in 0..spec.pages {
        let start = spec.offset_mm(page) * MM_TO_PT;
        let end = start + spec.size_mm(page).1 * MM_TO_PT;
        if from_top < end {
            return page;
        }
    }
    spec.pages.saturating_sub(1)
}

/// Where WebKit's asynchronous snapshot callback leaves its result.
type SnapshotSlot = Rc<RefCell<Option<Result<Retained<NSImage>, String>>>>;

fn snapshot_page(
    webview: &WKWebView,
    spec: RenderSpec,
    page: u32,
    dpi: u32,
    scale: f64,
) -> Result<Bytes, PlatformError> {
    let page = page as usize;
    let (width_mm, height_mm) = spec.size_mm(page);
    let width_pt = width_mm * MM_TO_PT;
    let height_pt = height_mm * MM_TO_PT;
    let width_px = (width_mm / 25.4 * f64::from(dpi)).round().max(1.0);
    let height_px = (height_mm / 25.4 * f64::from(dpi)).round().max(1.0);

    let rect = CGRect::new(
        CGPoint::new(0.0, spec.offset_mm(page) * MM_TO_PT),
        CGSize::new(width_pt, height_pt),
    );
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(PlatformError::Unavailable(
            "a preview window needs the GUI thread".to_owned(),
        ));
    };
    // SAFETY: called on the GUI thread, as `new` requires.
    let configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
    // SAFETY: the configuration is live and owned by this scope.
    unsafe { configuration.setRect(rect) };
    // The snapshot width is in points. Dividing by the backing scale asks
    // AppKit for the requested pixel width without a second resampling pass.
    let snapshot_width = NSNumber::numberWithDouble(width_px / scale.max(1.0));
    // SAFETY: as above.
    unsafe { configuration.setSnapshotWidth(Some(&snapshot_width)) };

    let slot: SnapshotSlot = Rc::new(RefCell::new(None));
    let block_slot = Rc::clone(&slot);
    let completion = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let result = if !error.is_null() {
            // SAFETY: WebKit hands over a valid error for this callback.
            let error = unsafe { &*error };
            Err(error.localizedDescription().to_string())
        } else if image.is_null() {
            Err("the snapshot completed without an image".to_owned())
        } else {
            // SAFETY: the image is valid for the callback; retaining it lets
            // the caller keep it after WebKit releases its reference.
            unsafe { Retained::retain(image) }
                .ok_or_else(|| "the snapshot image could not be retained".to_owned())
        };
        *block_slot.borrow_mut() = Some(result);
    });
    // SAFETY: live web view on the GUI thread; the block stays alive until the
    // callback has run or the timeout below gives up.
    unsafe {
        webview.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &completion)
    };

    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    loop {
        if let Some(result) = slot.borrow_mut().take() {
            let image = result.map_err(PlatformError::Render)?;
            return encode_png(&image, width_px, height_px);
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout(SNAPSHOT_TIMEOUT.as_millis() as u64));
        }
        let run_loop = NSRunLoop::currentRunLoop();
        let limit = NSDate::dateWithTimeIntervalSinceNow(0.01);
        // SAFETY: `NSDefaultRunLoopMode` is an immutable framework constant.
        run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &limit);
    }
}

fn encode_png(image: &NSImage, width_px: f64, height_px: f64) -> Result<Bytes, PlatformError> {
    let width = width_px as isize;
    let height = height_px as isize;
    // SAFETY: the colour space name is an immutable framework constant; the
    // initialiser accepts null planes and allocates the buffer.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }
    .ok_or_else(|| PlatformError::Render("cannot allocate the PNG bitmap".to_owned()))?;
    let Some(context) = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep) else {
        return Err(PlatformError::Render(
            "cannot create a drawing context for the PNG".to_owned(),
        ));
    };
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    NSColor::whiteColor().setFill();
    objc2_app_kit::NSRectFill(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width_px, height_px),
    ));
    image.drawInRect(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width_px, height_px),
    ));
    NSGraphicsContext::restoreGraphicsState_class();

    let properties: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    // SAFETY: the rep is complete and the properties dictionary is empty.
    let data =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
            .ok_or_else(|| PlatformError::Render("cannot encode the snapshot as PNG".to_owned()))?;
    Ok(Bytes::copy_from_slice(&data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_at_walks_mixed_page_heights() {
        let spec =
            RenderSpec::new(210.0, 297.0).with_page_sizes(vec![(210.0, 297.0), (148.0, 210.0)]);
        assert_eq!(page_at(&spec, 0.0), 0);
        assert_eq!(page_at(&spec, 297.0 * MM_TO_PT - 1.0), 0);
        assert_eq!(page_at(&spec, 297.0 * MM_TO_PT + 1.0), 1);
        assert_eq!(page_at(&spec, 600.0 * MM_TO_PT), 1);
    }
}
