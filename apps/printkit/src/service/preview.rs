//! Preview windows owned by one `serve` process.
//!
//! The registry enforces the two limits that belong to the service rather
//! than to a platform: at most `preview.max_windows` windows at once, and one
//! window per connected client. A client that previews again replaces the
//! content of the window it already owns, so a page does not accumulate
//! windows while the user edits a document.
//!
//! Platform windows are addressed by [`WindowId`] and live in the render host.
//! This module never touches a web view; it keeps the document bytes so the
//! window's Print control can reserve the same job `doc.print` would.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use bytes::Bytes;
use printkit_facade::{
    EngineError, PlatformError, PreviewActions, PreviewRequest, RenderSpec, Stage, Tracer, Warning,
    WindowId, limits, prepare_preview, trace_preview_refusal,
};
use printkit_protocol::{ErrorCode, Scope};

use super::AppState;
use super::auth::Session;

/// Connection ids for windows opened from the administrator control plane.
///
/// The high bit keeps them disjoint from WebSocket connection ids, which start
/// at one. Each admin preview request owns its window, so two invocations do
/// not overwrite each other.
static NEXT_INTERNAL_SESSION: AtomicU64 = AtomicU64::new(1 << 32);

pub fn internal_session(origin: String) -> Session {
    Session {
        connection: NEXT_INTERNAL_SESSION.fetch_add(1, Ordering::Relaxed),
        scopes: vec![Scope::Print],
        origin: Some(origin),
    }
}

/// What went wrong before or inside the platform window.
#[derive(Debug)]
pub enum PreviewError {
    Engine(EngineError),
    Platform(PlatformError),
    /// The process already holds the configured number of windows.
    TooMany {
        limit: usize,
    },
    /// A request field is outside its documented range.
    Invalid {
        message: String,
        pointer: Option<&'static str>,
    },
}

impl PreviewError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Engine(error) => match error {
                EngineError::Document(_) | EngineError::InvalidRequest { .. } => ErrorCode::Schema,
                EngineError::Fetch(_) => ErrorCode::Schema,
                EngineError::Platform(error) => platform_code(error),
                _ => ErrorCode::JobFailed,
            },
            Self::Platform(error) => platform_code(error),
            Self::TooMany { .. } => ErrorCode::TooLarge,
            Self::Invalid { .. } => ErrorCode::Schema,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Engine(error) => error.to_string(),
            Self::Platform(error) => error.to_string(),
            Self::TooMany { limit } => {
                format!("this process holds at most {limit} preview windows")
            }
            Self::Invalid { message, .. } => message.clone(),
        }
    }

    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            Self::Engine(error) => error.detail(),
            Self::Platform(_) => None,
            Self::TooMany { limit } => Some(serde_json::json!({ "limit": limit })),
            Self::Invalid { pointer, .. } => {
                pointer.map(|pointer| serde_json::json!({ "pointer": pointer }))
            }
        }
    }
}

fn platform_code(error: &PlatformError) -> ErrorCode {
    match error {
        PlatformError::Unavailable(_) | PlatformError::Unsupported(_) => ErrorCode::Unsupported,
        PlatformError::WindowNotFound(_) | PlatformError::JobNotFound { .. } => ErrorCode::NotFound,
        PlatformError::Timeout(_) => ErrorCode::Timeout,
        PlatformError::PrinterNotFound(_) | PlatformError::NoDefaultPrinter => {
            ErrorCode::PrinterNotFound
        }
        PlatformError::CancelUnsupported => ErrorCode::CancelUnsupported,
        PlatformError::RawUnsupported(_) => ErrorCode::RawUnsupported,
        PlatformError::Print(_) | PlatformError::Render(_) => ErrorCode::JobFailed,
    }
}

pub struct PreviewManager {
    limit: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_session: HashMap<u64, WindowId>,
    windows: HashMap<WindowId, Entry>,
    /// Slots claimed by an `open` that has not reached the platform yet, so
    /// two concurrent clients cannot both take the last slot.
    pending: usize,
}

struct Entry {
    spec: RenderSpec,
    session: u64,
}

/// A window the service opened, with the findings the caller must relay.
pub struct OpenedPreview {
    pub window: WindowId,
    pub warnings: Vec<Warning>,
}

impl PreviewManager {
    pub fn new(limit: u8) -> Self {
        Self {
            limit: usize::from(limit),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().windows.len()
    }

    /// Open a window for `session`, or replace the one it already owns.
    ///
    /// `prepare_preview` leaves the trace at `render`; the outcome of
    /// `show_window` is written here as `result`, so a refused window cannot
    /// look like a successful preview in the log.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        &self,
        state: &Arc<AppState>,
        session: &Session,
        input: Vec<u8>,
        on_print: Arc<dyn Fn(WindowId) + Send + Sync>,
    ) -> Result<OpenedPreview, PreviewError> {
        let started = Instant::now();
        // Take a slot before parsing or rendering: the ninth request must be
        // refused without spending a render on a window that cannot open.
        let slot = {
            let mut inner = self.lock();
            match inner.by_session.get(&session.connection) {
                Some(window) => Ok((Some(*window), false)),
                None if inner.windows.len() + inner.pending >= self.limit => Err(()),
                None => {
                    inner.pending += 1;
                    Ok((None, true))
                }
            }
        };
        let (target, reserved) = match slot {
            Ok(slot) => slot,
            Err(()) => {
                let error = PreviewError::TooMany { limit: self.limit };
                trace_preview_refusal(
                    &input,
                    state.sink.as_ref(),
                    session.origin.as_deref(),
                    error.code().as_str(),
                    &error.message(),
                );
                return Err(error);
            }
        };
        let worker_state = Arc::clone(state);
        let worker_input = input.clone();
        let worker_origin = session.origin.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_preview(
                &worker_input,
                worker_state.sink.as_ref(),
                worker_origin.as_deref(),
                &worker_state.fetch,
                worker_state.render_host.as_ref(),
            )
        })
        .await;
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                if reserved {
                    self.lock().pending -= 1;
                }
                return Err(PreviewError::Engine(error));
            }
            Err(join) => {
                if reserved {
                    self.lock().pending -= 1;
                }
                return Err(PreviewError::Platform(PlatformError::Render(format!(
                    "preview preparation task failed: {join}"
                ))));
            }
        };
        let tracer = Tracer::new(state.sink.as_ref(), prepared.trace_id.clone());
        let pages = prepared.spec.pages;

        let request = PreviewRequest {
            window: target,
            html: prepared.html,
            spec: prepared.spec.clone(),
            title: prepared.title,
            actions: Arc::new(ServiceActions {
                state: Arc::downgrade(state),
                on_print,
            }),
        };
        // `show_window` hops to the GUI main thread and blocks until the
        // window exists; keep that off the async worker.
        let host = Arc::clone(&state.render_host);
        let created = tokio::task::spawn_blocking(move || {
            futures::executor::block_on(host.show_window(request))
        })
        .await
        .map_err(|join| {
            PreviewError::Platform(PlatformError::Render(format!(
                "preview task failed: {join}"
            )))
        })?;
        let window = match created {
            Ok(window) => window,
            Err(error) => {
                if reserved {
                    self.lock().pending -= 1;
                }
                // A failed refresh closes the platform window, so the old
                // handle must not keep occupying a slot in the registry.
                if let Some(target) = target {
                    self.forget(target);
                }
                tracer
                    .event(Stage::Result)
                    .dur_ms(started.elapsed())
                    .failed(platform_code(&error).as_str())
                    .detail(error.to_string())
                    .emit();
                return Err(PreviewError::Platform(error));
            }
        };
        tracer
            .event(Stage::Result)
            .dur_ms(started.elapsed())
            .pages(u32::try_from(pages).unwrap_or(u32::MAX))
            .emit();

        let mut inner = self.lock();
        if reserved {
            inner.pending -= 1;
        }
        inner.by_session.insert(session.connection, window);
        inner.windows.insert(
            window,
            Entry {
                spec: prepared.spec,
                session: session.connection,
            },
        );
        Ok(OpenedPreview {
            window,
            warnings: prepared.warnings,
        })
    }

    /// Capture one paper page of a window owned by `session`.
    ///
    /// A window opened by another client answers `E_NOT_FOUND`, the same as a
    /// closed handle: the id alone must not expose someone else's document.
    pub async fn pages(
        &self,
        state: &AppState,
        session: &Session,
        window: WindowId,
        page: u32,
        dpi: u32,
    ) -> Result<Bytes, PreviewError> {
        if !(u32::from(limits::MIN_DPI)..=u32::from(limits::MAX_DPI)).contains(&dpi) {
            return Err(PreviewError::Invalid {
                message: format!(
                    "dpi must be between {} and {}",
                    limits::MIN_DPI,
                    limits::MAX_DPI
                ),
                pointer: Some("/dpi"),
            });
        }
        let spec = {
            let inner = self.lock();
            inner
                .windows
                .get(&window)
                .filter(|entry| entry.session == session.connection)
                .map(|entry| entry.spec.clone())
                .ok_or_else(|| PlatformError::WindowNotFound(window.to_string()))
                .map_err(PreviewError::Platform)?
        };
        if usize::try_from(page).map_or(true, |page| page >= spec.pages) {
            return Err(PreviewError::Invalid {
                message: format!(
                    "pageIndex {page} is outside the {} pages of this document",
                    spec.pages
                ),
                pointer: Some("/pageIndex"),
            });
        }
        let host = Arc::clone(&state.render_host);
        tokio::task::spawn_blocking(move || {
            futures::executor::block_on(host.window_png(window, page, dpi))
        })
        .await
        .map_err(|join| {
            PreviewError::Platform(PlatformError::Render(format!(
                "preview task failed: {join}"
            )))
        })?
        .map_err(PreviewError::Platform)
    }

    /// Close one window owned by `session`; the handle is invalid afterwards.
    pub async fn close(
        &self,
        state: &AppState,
        session: &Session,
        window: WindowId,
    ) -> Result<(), PreviewError> {
        let known = self.forget_owned(window, session.connection);
        if !known {
            return Err(PreviewError::Platform(PlatformError::WindowNotFound(
                window.to_string(),
            )));
        }
        state
            .render_host
            .close_window(window)
            .await
            .map_err(PreviewError::Platform)
    }

    /// Release the window a closed connection owned.
    pub async fn close_session(&self, state: &AppState, session: u64) {
        let window = {
            let inner = self.lock();
            inner.by_session.get(&session).copied()
        };
        let Some(window) = window else {
            return;
        };
        if self.forget(window) {
            let _ = state.render_host.close_window(window).await;
        }
    }

    /// Drop a handle from the registry. Returns whether it was present.
    ///
    /// Called by the platform when the user closes the window and by
    /// connection teardown. Both paths may race; being idempotent is what
    /// makes that harmless.
    pub fn forget(&self, window: WindowId) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.windows.remove(&window) else {
            return false;
        };
        if inner.by_session.get(&entry.session) == Some(&window) {
            inner.by_session.remove(&entry.session);
        }
        true
    }

    /// Drop a handle only when `session` owns it.
    fn forget_owned(&self, window: WindowId, session: u64) -> bool {
        let mut inner = self.lock();
        let Some(owner) = inner.windows.get(&window).map(|entry| entry.session) else {
            return false;
        };
        if owner != session {
            return false;
        }
        inner.windows.remove(&window);
        if inner.by_session.get(&owner) == Some(&window) {
            inner.by_session.remove(&owner);
        }
        true
    }
}

struct ServiceActions {
    state: Weak<AppState>,
    on_print: Arc<dyn Fn(WindowId) + Send + Sync>,
}

impl PreviewActions for ServiceActions {
    fn print(&self, window: WindowId) {
        (self.on_print)(window);
    }

    fn closed(&self, window: WindowId) {
        if let Some(state) = self.state.upgrade() {
            state.previews.forget(window);
        }
    }
}
