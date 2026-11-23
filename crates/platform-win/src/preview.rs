//! Visible preview windows, one STA thread each.
//!
//! A `WebView2` controller belongs to the thread that created it, and its
//! callbacks only run while that thread pumps messages. Every preview window
//! therefore owns a thread, a top-level `WS_OVERLAPPEDWINDOW` and its own
//! controller. The service talks to the window through a command channel;
//! `PreviewActions` callbacks run on the window thread and must return
//! promptly, exactly like the macOS implementation.
//!
//! The window has no toolbar: the title bar, the `Ctrl+P` accelerator and the
//! close button are enough. `Ctrl+P` arrives through WebView2's
//! `AcceleratorKeyPressed` event (the web view owns keyboard focus, so a plain
//! `WM_KEYDOWN` would never see it) and calls [`PreviewActions::print`]. A
//! native button would need a child window and controller bounds below it; that
//! is deliberately left out until a Windows host can verify it.
//!
//! Page images come from the DevTools protocol (`Page.captureScreenshot` with
//! `captureBeyondViewport` and a millimetre-derived clip). `CapturePreview`
//! only captures the visible viewport, so it is a fallback: its image is only
//! returned when its pixel size matches the requested page/DPI, and otherwise
//! the caller receives `Unsupported` with both sizes rather than a silently
//! cropped page.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use printkit_platform::{PlatformError, PreviewActions, PreviewRequest, RenderSpec, WindowId};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
    ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Environment,
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CallDevToolsProtocolMethodCompletedHandler,
    CapturePreviewCompletedHandler,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_P};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BN_CLICKED, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, HICON, HMENU,
    IDC_ARROW, IsWindow, LoadCursorW, MINMAXINFO, MSG, MoveWindow, PM_REMOVE, PeekMessageW,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SM_REMOTESESSION, SW_SHOW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_GETMINMAXINFO,
    WM_NCDESTROY, WM_SIZE, WNDCLASS_STYLES, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE,
};
use windows::core::{Error, HSTRING, PCWSTR, w};

use super::host::{
    ComApartment, UserDataDir, create_controller, create_environment, navigate, pump_until,
    render_error,
};

/// Windows the service may keep open at once. The service enforces the same
/// limit before it calls the platform; this is the platform's own guard.
pub const MAX_WINDOWS: usize = 8;
/// Toolbar band above the web view, in physical pixels.
const TOOLBAR_HEIGHT: i32 = 36;
const TOOLBAR_MARGIN: i32 = 5;
const PRINT_BUTTON_ID: u16 = 1001;
const PRINT_BUTTON_WIDTH: i32 = 90;
const PRINT_BUTTON_HEIGHT: i32 = 26;
const MIN_WINDOW_WIDTH: i32 = 420;
const MIN_WINDOW_HEIGHT: i32 = 240;
const _: () = assert!(TOOLBAR_HEIGHT > PRINT_BUTTON_HEIGHT);
const _: () = assert!(MIN_WINDOW_HEIGHT > TOOLBAR_HEIGHT);
/// Waiting for a window to appear, including one navigation.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// A page image is a capture of a loaded view; a slow host fails loudly.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// Draining the command queue keeps the message pump responsive.
const COMMAND_POLL: Duration = Duration::from_millis(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);
static REGISTRY: OnceLock<Mutex<HashMap<WindowId, mpsc::Sender<PreviewCommand>>>> = OnceLock::new();
static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<WindowId, mpsc::Sender<PreviewCommand>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_supports_preview(session: u32, remote: bool, width: i32, height: i32) -> bool {
    session != 0 && (remote || (width > 0 && height > 0))
}

/// Whether this process owns an interactive desktop.
///
/// Session 0 (a service) has no window station. A remote (RDP) session still
/// owns a desktop, so `SM_REMOTESESSION` alone must not disable previews.
pub(crate) fn available() -> bool {
    let mut session = 0u32;
    // SAFETY: both calls take plain values and write to a local.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.is_err() || session == 0
    {
        return false;
    }
    // SAFETY: GetSystemMetrics reads process-wide screen metrics.
    let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    // SAFETY: same thread-safe metric read.
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    session_supports_preview(session, remote, width, height)
}

/// Destroy every window when the service stops.
pub(crate) fn close_all() {
    let handles: Vec<mpsc::Sender<PreviewCommand>> = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .map(|(_, sender)| sender)
        .collect();
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    for sender in handles {
        let (reply, answer) = mpsc::channel();
        if sender.send(PreviewCommand::Close(Some(reply))).is_ok() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            let _ = answer.recv_timeout(remaining);
        }
    }
}

pub(crate) fn show_window(
    request: PreviewRequest,
    timeout: Duration,
) -> Result<WindowId, PlatformError> {
    if !available() {
        return Err(PlatformError::Unavailable(
            "no interactive desktop is attached to this process".to_owned(),
        ));
    }
    if let Some(id) = request.window {
        return update_window(id, request, timeout);
    }

    let id = WindowId::new(NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    let (sender, receiver) = mpsc::channel();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let panic_sender = ready_sender.clone();
    let panic_sent = Arc::new(AtomicBool::new(false));
    let inner_sent = Arc::clone(&panic_sent);
    std::thread::Builder::new()
        .name(format!("printkit-preview-{}", id.get()))
        .spawn(move || {
            let deadline = Instant::now() + timeout;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // The apartment must stay alive for the whole window, not just
                // creation: WebView2 callbacks keep arriving on this thread.
                let apartment = match ComApartment::init() {
                    Ok(apartment) => apartment,
                    Err(error) => {
                        let _ = inner_sent.swap(true, Ordering::SeqCst);
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let created = PreviewWindow::create(id, request, deadline, timeout);
                match created {
                    Ok(mut window) => match insert_window(id, sender) {
                        Ok(()) => {
                            let _ = inner_sent.swap(true, Ordering::SeqCst);
                            let _ = ready_sender.send(Ok(()));
                            window.run(&receiver, timeout);
                        }
                        Err(error) => {
                            remove_window(id);
                            let _ = inner_sent.swap(true, Ordering::SeqCst);
                            let _ = ready_sender.send(Err(error));
                        }
                    },
                    Err(error) => {
                        let _ = inner_sent.swap(true, Ordering::SeqCst);
                        let _ = ready_sender.send(Err(error));
                    }
                }
                drop(apartment);
            }));
            if result.is_err() && !panic_sent.load(Ordering::SeqCst) {
                let _ = panic_sender.send(Err(PlatformError::Render(
                    "preview host panicked".to_owned(),
                )));
            }
        })
        .map_err(|error| {
            PlatformError::Unavailable(format!("cannot start the preview thread: {error}"))
        })?;

    match ready_receiver.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(id),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(PlatformError::Timeout(START_TIMEOUT.as_millis() as u64)),
    }
}

fn update_window(
    id: WindowId,
    request: PreviewRequest,
    timeout: Duration,
) -> Result<WindowId, PlatformError> {
    let sender = lookup(id)?;
    let (reply, answer) = mpsc::channel();
    sender
        .send(PreviewCommand::Update(request, reply))
        .map_err(|_| PlatformError::WindowNotFound(id.to_string()))?;
    match answer.recv_timeout(timeout + Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(id),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(PlatformError::Timeout(timeout.as_millis() as u64)),
    }
}

pub(crate) fn window_png(id: WindowId, page: u32, dpi: u32) -> Result<Bytes, PlatformError> {
    let sender = lookup(id)?;
    let (reply, answer) = mpsc::channel();
    sender
        .send(PreviewCommand::Capture { page, dpi, reply })
        .map_err(|_| PlatformError::WindowNotFound(id.to_string()))?;
    match answer.recv_timeout(SNAPSHOT_TIMEOUT + Duration::from_secs(2)) {
        Ok(result) => result,
        Err(_) => Err(PlatformError::Timeout(SNAPSHOT_TIMEOUT.as_millis() as u64)),
    }
}

pub(crate) fn close_window(id: WindowId) -> Result<(), PlatformError> {
    let sender = remove_window(id).ok_or_else(|| PlatformError::WindowNotFound(id.to_string()))?;
    let (reply, answer) = mpsc::channel();
    sender
        .send(PreviewCommand::Close(Some(reply)))
        .map_err(|_| PlatformError::WindowNotFound(id.to_string()))?;
    match answer.recv_timeout(CLOSE_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(PlatformError::Timeout(CLOSE_TIMEOUT.as_millis() as u64)),
    }
}

fn insert_window(id: WindowId, sender: mpsc::Sender<PreviewCommand>) -> Result<(), PlatformError> {
    let mut windows = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if windows.len() >= MAX_WINDOWS {
        return Err(PlatformError::Unsupported(format!(
            "at most {MAX_WINDOWS} preview windows can be open"
        )));
    }
    windows.insert(id, sender);
    Ok(())
}

fn remove_window(id: WindowId) -> Option<mpsc::Sender<PreviewCommand>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id)
}

fn lookup(id: WindowId) -> Result<mpsc::Sender<PreviewCommand>, PlatformError> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .cloned()
        .ok_or_else(|| PlatformError::WindowNotFound(id.to_string()))
}

enum PreviewCommand {
    Update(PreviewRequest, mpsc::Sender<Result<(), PlatformError>>),
    Capture {
        page: u32,
        dpi: u32,
        reply: mpsc::Sender<Result<Bytes, PlatformError>>,
    },
    Close(Option<mpsc::Sender<Result<(), PlatformError>>>),
}

/// State the window procedure needs. It lives in `GWLP_USERDATA` for the
/// lifetime of the `HWND` and is released on `WM_NCDESTROY`.
struct WindowState {
    id: WindowId,
    actions: Mutex<Arc<dyn PreviewActions>>,
}

struct PreviewWindow {
    id: WindowId,
    hwnd: HWND,
    controller: ICoreWebView2Controller,
    core: ICoreWebView2,
    _environment: ICoreWebView2Environment,
    _user_data: UserDataDir,
    state: Arc<WindowState>,
    print_button: HWND,
    spec: RenderSpec,
}

impl PreviewWindow {
    fn create(
        id: WindowId,
        request: PreviewRequest,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Self, PlatformError> {
        ensure_class()?;
        let state = Arc::new(WindowState {
            id,
            actions: Mutex::new(Arc::clone(&request.actions)),
        });
        let title = HSTRING::from(window_title(&request.title));
        let rect = window_rect(&request.spec);
        // SAFETY: class and title are live; the window has no parent.
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                w!("PrintkitPreviewWindow"),
                &title,
                WS_OVERLAPPEDWINDOW,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(module_instance()?),
                None,
            )
        }
        .map_err(|error| {
            PlatformError::Unavailable(format!("cannot create the preview window: {error}"))
        })?;
        let guard = WindowGuard(Some(hwnd));
        // SAFETY: the window is live and owned by this thread; the pointer is
        // released on WM_NCDESTROY.
        unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                Arc::into_raw(Arc::clone(&state)) as isize,
            );
        }

        // A plain system BUTTON is enough; it lives in the toolbar band and is
        // repositioned with the window. Its command id is handled in preview_proc.
        // SAFETY: the parent window is live and the class/title are static.
        let print_button = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Print"),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                8,
                TOOLBAR_MARGIN,
                PRINT_BUTTON_WIDTH,
                PRINT_BUTTON_HEIGHT,
                Some(hwnd),
                Some(HMENU(usize::from(PRINT_BUTTON_ID) as *mut core::ffi::c_void)),
                Some(module_instance()?),
                None,
            )
        }
        .map_err(|error| {
            PlatformError::Unavailable(format!("cannot create the Print button: {error}"))
        })?;

        let user_data = UserDataDir::create()?;
        let environment = create_environment(user_data.path(), deadline, timeout)?;
        let controller = create_controller(&environment, hwnd, deadline, timeout)?;
        // SAFETY: the controller was created on this thread and is live.
        let core = unsafe { controller.CoreWebView2() }.map_err(render_error)?;
        // SAFETY: every call runs on the owning thread.
        unsafe {
            core.Settings()
                .map_err(render_error)?
                .SetIsScriptEnabled(false)
                .map_err(render_error)?;
        }

        // The web view owns keyboard focus, so `Ctrl+P` must be taken from the
        // accelerator event and marked handled before Chromium opens its own
        // print dialog.
        let key_state = Arc::clone(&state);
        let accelerator =
            AcceleratorKeyPressedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN;
                let mut key = 0u32;
                // SAFETY: the event arguments are live for this callback.
                unsafe {
                    let _ = args.KeyEventKind(&mut kind);
                    let _ = args.VirtualKey(&mut key);
                }
                // SAFETY: keyboard state is process-wide and read-only here.
                if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                    && key == u32::from(VK_P.0)
                    && unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0
                {
                    // SAFETY: the arguments object is live; handling it stops
                    // the browser's own `Ctrl+P` dialog.
                    unsafe {
                        let _ = args.SetHandled(true);
                    }
                    let actions = key_state
                        .actions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    actions.print(key_state.id);
                }
                Ok(())
            }));
        let mut accelerator_token = 0i64;
        // SAFETY: the controller and handler are live on this thread.
        unsafe {
            controller
                .add_AcceleratorKeyPressed(&accelerator, &mut accelerator_token)
                .map_err(render_error)?;
        }

        navigate(&core, &request.html, deadline, timeout)?;
        // SAFETY: the window was created on this thread.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let hwnd = guard.disarm();
        let window = Self {
            id,
            hwnd,
            controller,
            core,
            _environment: environment,
            _user_data: user_data,
            state,
            print_button,
            spec: request.spec,
        };
        window.layout_children();
        Ok(window)
    }

    fn run(&mut self, commands: &mpsc::Receiver<PreviewCommand>, timeout: Duration) {
        let mut message = MSG::default();
        loop {
            match commands.recv_timeout(COMMAND_POLL) {
                Ok(PreviewCommand::Update(request, reply)) => {
                    let result = self.apply_update(request, timeout);
                    let _ = reply.send(result);
                }
                Ok(PreviewCommand::Capture { page, dpi, reply }) => {
                    let result = self.capture(page, dpi);
                    let _ = reply.send(result);
                }
                Ok(PreviewCommand::Close(answer)) => {
                    // SAFETY: the window is live on this thread.
                    unsafe {
                        let _ = DestroyWindow(self.hwnd);
                    }
                    let _ = answer.map(|sender| sender.send(Ok(())));
                    remove_window(self.id);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    remove_window(self.id);
                    return;
                }
            }
            self.pump(&mut message);
            // SAFETY: a plain liveness query.
            if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
                remove_window(self.id);
                return;
            }
        }
    }

    fn apply_update(
        &mut self,
        request: PreviewRequest,
        timeout: Duration,
    ) -> Result<(), PlatformError> {
        self.spec = request.spec.clone();
        *self
            .state
            .actions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&request.actions);
        let title = HSTRING::from(window_title(&request.title));
        // SAFETY: the window is live on this thread.
        unsafe {
            SetWindowTextW(self.hwnd, &title).map_err(render_error)?;
        }
        let (width, height) = window_size(&self.spec);
        let mut current = RECT::default();
        // SAFETY: same thread and live window.
        unsafe { GetWindowRect(self.hwnd, &mut current) }.map_err(render_error)?;
        // Refresh content without making the window jump back to the centre.
        // SAFETY: same thread and live window.
        unsafe {
            MoveWindow(self.hwnd, current.left, current.top, width, height, true)
                .map_err(render_error)?;
        }
        self.layout_children();
        navigate(&self.core, &request.html, Instant::now() + timeout, timeout)?;
        self.layout_children();
        Ok(())
    }

    /// Dispatch queued messages; `WM_SIZE` also re-fits the controller.
    fn pump(&mut self, message: &mut MSG) {
        // SAFETY: `message` is a valid MSG and this thread owns the queue.
        unsafe {
            while PeekMessageW(message, None, 0, 0, PM_REMOVE).as_bool() {
                let resize = message.hwnd == self.hwnd && message.message == WM_SIZE;
                let _ = TranslateMessage(message);
                DispatchMessageW(message);
                if resize {
                    self.layout_children();
                }
            }
        }
    }

    /// Put the Print button in the toolbar band and the web view below it.
    fn layout_children(&self) {
        let Ok(rect) = client_rect(self.hwnd) else {
            return;
        };
        let web_top = TOOLBAR_HEIGHT.min(rect.bottom);
        let web = RECT {
            left: 0,
            top: web_top,
            right: rect.right,
            bottom: rect.bottom.max(web_top),
        };
        // SAFETY: the button and controller belong to this thread and window.
        unsafe {
            let _ = MoveWindow(
                self.print_button,
                8,
                TOOLBAR_MARGIN,
                PRINT_BUTTON_WIDTH,
                PRINT_BUTTON_HEIGHT,
                true,
            );
            let _ = self.controller.SetBounds(web);
        }
    }

    fn capture(&self, page: u32, dpi: u32) -> Result<Bytes, PlatformError> {
        let index = page_index(page, self.spec.pages)?;
        let (width_mm, height_mm) = self.spec.size_mm(index);
        let expected_width = pixels(width_mm, dpi);
        let expected_height = pixels(height_mm, dpi);
        let params = screenshot_params(&self.spec, index, dpi)?.to_string();

        match self.capture_devtools(&params) {
            Ok(png) => match png_dimensions(&png) {
                Some((width, height))
                    if close_enough(width, expected_width)
                        && close_enough(height, expected_height) =>
                {
                    Ok(Bytes::from(png))
                }
                Some((width, height)) => Err(PlatformError::Render(format!(
                    "DevTools captured {width}x{height}, expected {expected_width}x{expected_height}"
                ))),
                None => Err(PlatformError::Render(
                    "DevTools returned no PNG image".to_owned(),
                )),
            },
            Err(devtools_error) => {
                self.capture_viewport(expected_width, expected_height, &devtools_error)
            }
        }
    }

    fn capture_devtools(&self, params: &str) -> Result<Vec<u8>, PlatformError> {
        let (sender, receiver) = mpsc::channel();
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |result, json| {
                let _ = sender.send((result, json));
                Ok(())
            }));
        let method = HSTRING::from("Page.captureScreenshot");
        let params = HSTRING::from(params);
        // SAFETY: the core object and handler live on this thread.
        unsafe {
            self.core
                .CallDevToolsProtocolMethod(&method, &params, &handler)
        }
        .map_err(render_error)?;
        let (result, json) = pump_until(
            receiver,
            Instant::now() + SNAPSHOT_TIMEOUT,
            SNAPSHOT_TIMEOUT,
        )?;
        result.map_err(|error| {
            PlatformError::Render(format!("DevTools Page.captureScreenshot failed: {error}"))
        })?;
        let value: serde_json::Value = serde_json::from_str(&json).map_err(|error| {
            PlatformError::Render(format!("DevTools returned unreadable JSON: {error}"))
        })?;
        let data = value
            .get("data")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                PlatformError::Render("DevTools returned no screenshot data".to_owned())
            })?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| {
                PlatformError::Render(format!("DevTools returned invalid base64: {error}"))
            })
    }

    /// `CapturePreview` only sees the viewport; return it solely when its
    /// pixels match the requested page, never as a silently cropped page.
    fn capture_viewport(
        &self,
        expected_width: u32,
        expected_height: u32,
        devtools_error: &PlatformError,
    ) -> Result<Bytes, PlatformError> {
        let (sender, receiver) = mpsc::channel();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            let _ = sender.send(result);
            Ok(())
        }));
        // SAFETY: a private HGLOBAL stream owned by the returned IStream.
        let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true) }
            .map_err(render_error)?;
        // SAFETY: the core object, stream and handler live on this thread.
        unsafe {
            self.core
                .CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                    &stream,
                    &handler,
                )
                .map_err(render_error)?;
        }
        let result = pump_until(
            receiver,
            Instant::now() + SNAPSHOT_TIMEOUT,
            SNAPSHOT_TIMEOUT,
        )?;
        result.map_err(|error| PlatformError::Render(format!("CapturePreview failed: {error}")))?;
        let png = read_stream(&stream)?;
        match png_dimensions(&png) {
            Some((width, height))
                if close_enough(width, expected_width) && close_enough(height, expected_height) =>
            {
                Ok(Bytes::from(png))
            }
            Some((width, height)) => Err(PlatformError::Unsupported(format!(
                "DevTools screenshot unavailable ({devtools_error}); CapturePreview only \
                 captures the visible viewport ({width}x{height}, expected \
                 {expected_width}x{expected_height})"
            ))),
            None => Err(PlatformError::Render(
                "CapturePreview returned no PNG image".to_owned(),
            )),
        }
    }
}

impl Drop for PreviewWindow {
    fn drop(&mut self) {
        // SAFETY: both handles belong to this thread; closing the controller
        // before the window keeps the web process from seeing a dead parent.
        unsafe {
            let _ = self.controller.Close();
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

/// Destroy the window if setup fails before `PreviewWindow` owns it.
struct WindowGuard(Option<HWND>);

impl WindowGuard {
    fn disarm(mut self) -> HWND {
        self.0.take().unwrap_or_default()
    }
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        if let Some(hwnd) = self.0.take() {
            // SAFETY: created on this thread and not yet owned elsewhere.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

/// Window procedure: close, `Ctrl+P` and state release.
unsafe extern "system" fn preview_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the pointer was stored at creation and released exactly once.
    unsafe {
        if message == WM_NCDESTROY {
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
            if !pointer.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Arc::from_raw(pointer));
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize.x = MIN_WINDOW_WIDTH;
            info.ptMinTrackSize.y = MIN_WINDOW_HEIGHT;
            return LRESULT(0);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        if pointer.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let state = &*pointer;
        match message {
            WM_COMMAND => {
                let control_id = (wparam.0 & 0xffff) as u16;
                let notification = ((wparam.0 >> 16) & 0xffff) as u16;
                if control_id == PRINT_BUTTON_ID && notification == BN_CLICKED as u16 {
                    let actions = state
                        .actions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    actions.print(state.id);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, message, wparam, lparam)
                }
            }
            WM_CLOSE => {
                // The service initiated the close through the command channel,
                // which removes the entry first; a user close still reports.
                remove_window(state.id);
                let actions = state
                    .actions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                actions.closed(state.id);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

fn ensure_class() -> Result<(), PlatformError> {
    if CLASS_REGISTERED.get().is_some() {
        return Ok(());
    }
    // SAFETY: the module handle is the process image; the cursor and class
    // name are static resources owned by it.
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).map_err(render_error)?;
        let class = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(preview_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module_instance()?,
            hIcon: HICON(std::ptr::null_mut()),
            hCursor: cursor,
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("PrintkitPreviewWindow"),
        };
        let registered = RegisterClassW(&class);
        if registered == 0 {
            let error = Error::from_thread();
            if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
                return Err(render_error(error));
            }
        }
    }
    let _ = CLASS_REGISTERED.set(());
    Ok(())
}

fn module_instance() -> Result<HINSTANCE, PlatformError> {
    // SAFETY: no module name asks for the executable image.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(render_error)?;
    Ok(HINSTANCE(module.0))
}

fn window_title(title: &str) -> String {
    if title.is_empty() {
        "printkit preview".to_owned()
    } else {
        format!("printkit preview - {title}")
    }
}

/// Outer window size for a page, in physical pixels.
fn window_size(spec: &RenderSpec) -> (i32, i32) {
    let width = (spec.content_width_mm() * 96.0 / 25.4).max(f64::from(MIN_WINDOW_WIDTH));
    // SAFETY: a thread-safe metric read; zero means no desktop, which `available`
    // rejects before a window is created.
    let screen_height = f64::from(unsafe { GetSystemMetrics(SM_CYSCREEN) });
    let height = (spec.height_mm * 96.0 / 25.4 + f64::from(TOOLBAR_HEIGHT))
        .min(screen_height * 0.9)
        .max(f64::from(MIN_WINDOW_HEIGHT));
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width.round() as i32,
        bottom: height.round() as i32,
    };
    // SAFETY: a pure computation on a local rect.
    unsafe {
        let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn window_rect(spec: &RenderSpec) -> RECT {
    let (width, height) = window_size(spec);
    // SAFETY: thread-safe metric read; zero only without a desktop.
    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    // SAFETY: same metric read.
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let left = (screen_width - width) / 2;
    let top = (screen_height - height) / 2;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn client_rect(hwnd: HWND) -> Result<RECT, PlatformError> {
    let mut rect = RECT::default();
    // SAFETY: the window is live on this thread.
    unsafe { GetClientRect(hwnd, &mut rect) }.map_err(render_error)?;
    Ok(rect)
}

fn page_index(page: u32, pages: usize) -> Result<usize, PlatformError> {
    let index = usize::try_from(page)
        .map_err(|_| PlatformError::Render(format!("page {page} does not fit a usize")))?;
    if index >= pages {
        return Err(PlatformError::Render(format!(
            "page {page} is outside {pages} pages"
        )));
    }
    Ok(index)
}

fn screenshot_params(
    spec: &RenderSpec,
    page: usize,
    dpi: u32,
) -> Result<serde_json::Value, PlatformError> {
    if dpi == 0 {
        return Err(PlatformError::Render("dpi must be positive".to_owned()));
    }
    let (width_mm, height_mm) = spec.size_mm(page);
    let offset_mm = spec.offset_mm(page);
    let scale = f64::from(dpi) / 96.0;
    Ok(serde_json::json!({
        "format": "png",
        "captureBeyondViewport": true,
        "fromSurface": true,
        "clip": {
            "x": 0.0,
            "y": offset_mm * 96.0 / 25.4,
            "width": width_mm * 96.0 / 25.4,
            "height": height_mm * 96.0 / 25.4,
            "scale": scale,
        },
    }))
}

fn pixels(mm: f64, dpi: u32) -> u32 {
    let value = (mm * f64::from(dpi) / 25.4).round();
    value.clamp(1.0, u32::MAX as f64) as u32
}

fn close_enough(actual: u32, expected: u32) -> bool {
    actual.abs_diff(expected) <= 1
}

/// PNG width/height from the IHDR chunk; no decoder needed for a size check.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes.get(..8)? != b"\x89PNG\r\n\x1a\n" || bytes.get(12..16)? != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

fn read_stream(stream: &IStream) -> Result<Vec<u8>, PlatformError> {
    // The writer may have left the position at the end; rewind before reading.
    // SAFETY: a plain stream seek.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }.map_err(|error| {
        PlatformError::Render(format!("cannot rewind the image stream: {error}"))
    })?;
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let mut read = 0u32;
        // SAFETY: `buffer` is writable for its full length.
        let status = unsafe {
            stream.Read(
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                buffer.len() as u32,
                Some(&mut read),
            )
        };
        status.ok().map_err(|error| {
            PlatformError::Render(format!("cannot read the image stream: {error}"))
        })?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_caps_and_forgets_windows() {
        let mut ids = Vec::new();
        for index in 0..MAX_WINDOWS {
            let (sender, _receiver) = mpsc::channel();
            let id = WindowId::new(90_000 + index as u64);
            assert!(insert_window(id, sender).is_ok());
            ids.push(id);
        }
        let (sender, _receiver) = mpsc::channel();
        assert!(matches!(
            insert_window(WindowId::new(99_999), sender),
            Err(PlatformError::Unsupported(_))
        ));
        for id in &ids {
            assert!(remove_window(*id).is_some());
        }
        assert!(lookup(ids[0]).is_err());
    }

    #[test]
    fn preview_availability_needs_a_session_and_a_desktop() {
        assert!(!session_supports_preview(0, false, 1920, 1080));
        assert!(!session_supports_preview(0, true, 1920, 1080));
        assert!(session_supports_preview(1, false, 1920, 1080));
        assert!(!session_supports_preview(1, false, 0, 0));
        // A remote session still owns a desktop even if metrics are odd.
        assert!(session_supports_preview(1, true, 0, 0));
    }

    #[test]
    fn page_indices_are_bounds_checked() {
        assert_eq!(page_index(0, 3).unwrap(), 0);
        assert_eq!(page_index(2, 3).unwrap(), 2);
        assert!(page_index(3, 3).is_err());
        assert!(page_index(u32::MAX, 3).is_err());
    }

    #[test]
    fn screenshot_parameters_follow_page_geometry_and_dpi() {
        let spec = RenderSpec::new(210.0, 297.0).with_pages(2);
        let params = screenshot_params(&spec, 1, 96).unwrap();
        assert_eq!(params["format"], "png");
        assert_eq!(params["captureBeyondViewport"], true);
        assert_eq!(params["fromSurface"], true);
        let clip = &params["clip"];
        let page_px = 297.0 * 96.0 / 25.4;
        assert!((clip["y"].as_f64().unwrap_or_default() - page_px).abs() < 1e-9);
        assert!((clip["height"].as_f64().unwrap_or_default() - page_px).abs() < 1e-9);
        assert!((clip["scale"].as_f64().unwrap_or_default() - 1.0).abs() < 1e-9);

        let high = screenshot_params(&spec, 0, 192).unwrap();
        assert!((high["clip"]["scale"].as_f64().unwrap_or_default() - 2.0).abs() < 1e-9);
        assert!(screenshot_params(&spec, 0, 0).is_err());
    }

    #[test]
    fn pixels_and_png_dimensions_are_checkable() {
        assert_eq!(pixels(210.0, 96), 794);
        assert!(close_enough(794, 794));
        assert!(close_enough(794, 795));
        assert!(!close_enough(794, 800));

        let mut header = vec![0u8; 24];
        header[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        header[12..16].copy_from_slice(b"IHDR");
        header[16..20].copy_from_slice(&794_u32.to_be_bytes());
        header[20..24].copy_from_slice(&1123_u32.to_be_bytes());
        assert_eq!(png_dimensions(&header), Some((794, 1123)));
        assert_eq!(png_dimensions(b"not a png at all"), None);
    }

    #[test]
    fn devtools_base64_decodes_to_bytes() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"png-bytes");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map(|bytes| bytes.to_vec()),
            Ok(b"png-bytes".to_vec())
        );
    }

    #[test]
    fn the_toolbar_band_and_minimum_window_are_consistent() {
        let (width, height) = window_size(&RenderSpec::new(50.0, 50.0));
        assert!(width >= MIN_WINDOW_WIDTH);
        assert!(height >= MIN_WINDOW_HEIGHT);
    }

    #[test]
    fn window_titles_carry_the_document_name() {
        assert_eq!(window_title(""), "printkit preview");
        assert_eq!(window_title("invoice"), "printkit preview - invoice");
    }
}
