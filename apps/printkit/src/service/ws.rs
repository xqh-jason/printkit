//! WebSocket face: one `hello`, then request/response envelopes plus pushed
//! `job` events.
//!
//! The transport `Origin` header is captured before the upgrade because the
//! handshake handler no longer sees the HTTP request. Authentication itself
//! lives in [`super::auth`]; this module frames messages and routes method
//! calls.
//!
//! The read half and the outbound event queue are polled by one future: a
//! WebSocket is a single mutable object, so splitting the socket would need an
//! owning split that Salvo does not provide. Draining the queue first keeps a
//! chatty job from starving the client's requests.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use printkit_facade::{
    BatchPart, EngineError, JobFailure, JobId, JobPhase, JobUpdate, JobUpdateHook, PrintBatch,
    PrintPdf, RawSend, Stage, TraceId, Tracer, UsageRecorder, Warning, WindowId, limits,
};
use printkit_plugin_api::{
    RecordQuery as ApiRecordQuery, RecordRow, RecordScope, RecordStats as ApiRecordStats,
};
use printkit_protocol::{
    BatchResult, Capabilities, DocPrintBatchParams, DocPrintPdfBatchParams, DocPrintPdfParams,
    Empty, ErrorBody, ErrorCode, ExportPdfParams, ExportPdfResult, Hello, JobError, JobEvent,
    JobIdParams, JobState, LogsExportParams, METHODS, PROTOCOL_VERSION, PaperKind, PaperSize,
    PdfBatchResult, Platform, PreviewCloseParams, PreviewPagesParams, PrintersPapersParams,
    RawSendParams, RecordView, RecordsExportParams, RecordsExportResult, RecordsListParams,
    RecordsListResult, RecordsStatsParams, RecordsStatsResult, RecordsSummaryParams,
    RecordsSummaryView, Rejected, Request as ProtocolRequest, Response as ProtocolResponse, Scope,
    StatsBucketView, StatsGroupView, SysInfo, TraceSubscribeParams, TraceTailParams, WarningBody,
    WarningCode, Welcome, WindowRef,
};
use salvo::http::header::ORIGIN;
use salvo::prelude::*;
use salvo::websocket::{Message, WebSocket, WebSocketUpgrade};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::sync::mpsc::{Receiver, channel};

use super::AppState;
use super::auth::{Rejection, Session, authenticate};
use super::events::JobEventHub;
use super::observability::{self, ObservabilityError, TracePlan};
use super::preview::PreviewError;

/// One process-wide connection budget, matching the security model.
const MAX_CONNECTIONS: usize = 8;

/// The unauthenticated handshake: a client that sends no `hello` must not keep
/// a connection slot or a socket alive.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// One Origin may not take every global slot.
const MAX_CONNECTIONS_PER_ORIGIN: usize = 4;

/// A hello larger than this is refused before it is parsed as JSON.
const MAX_HELLO_BYTES: usize = 4 * 1024;
/// Events (job progress and trace replays) may be dropped when a client does
/// not read fast enough; responses are written directly to the socket.
const OUTBOUND_QUEUE: usize = 256;

/// An authenticated connection with no traffic in either direction for this
/// long is closed. An in-flight job started through the socket keeps it alive
/// past this window.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn idle_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("PRINTKIT_WS_IDLE_SECS")
        && let Ok(seconds) = value.parse::<u64>()
    {
        return Duration::from_secs(seconds);
    }
    IDLE_TIMEOUT
}

/// A deadline that any request or event frame pushes forward.
struct IdleTimer {
    timeout: Duration,
    deadline: tokio::time::Instant,
}

impl IdleTimer {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: tokio::time::Instant::now() + timeout,
        }
    }

    fn touch(&mut self) {
        self.deadline = tokio::time::Instant::now() + self.timeout;
    }

    fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

/// Drop finished jobs from `in_flight` and report whether any job on this
/// socket is still running. A job whose pool record disappeared is treated as
/// finished: the registry evicts only terminal entries.
fn keep_alive_for_in_flight(state: &AppState, in_flight: &mut HashSet<String>) -> bool {
    in_flight.retain(|job_id| {
        state
            .pool
            .status(&JobId::from(job_id.as_str()))
            .is_some_and(|status| {
                !matches!(
                    status.phase,
                    JobPhase::Done | JobPhase::Failed | JobPhase::Cancelled
                )
            })
    });
    !in_flight.is_empty()
}

async fn origin_whitelisted(state: &Arc<AppState>, origin: &str) -> Result<bool, ()> {
    let store = state.store.clone();
    let origin = origin.to_owned();
    match tokio::task::spawn_blocking(move || store.origins()).await {
        Ok(Ok(origins)) => Ok(origins.iter().any(|allowed| allowed == &origin)),
        _ => Err(()),
    }
}

pub async fn upgrade(req: &mut Request, res: &mut Response, state: Arc<AppState>) {
    let origin = req
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // A browser must be paired before it receives an upgrade; a local script
    // (no Origin header) is allowed through and still gets only print scope.
    if let Some(origin) = origin.as_deref() {
        match origin_whitelisted(&state, origin).await {
            Ok(true) => {}
            Ok(false) => {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Text::Plain("origin is not whitelisted"));
                return;
            }
            Err(()) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Text::Plain("cannot read the origin whitelist"));
                return;
            }
        }
    }
    // Cheap HTTP-level rejection while every slot is taken; the authoritative
    // check happens after the handshake, so a stalled handshake never reserves
    // one.
    if state.connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
        res.status_code(StatusCode::TOO_MANY_REQUESTS);
        res.render(Text::Plain("too many WebSocket connections"));
        return;
    }
    let connection_state = state.clone();
    let connection_key = origin.clone();
    let result = WebSocketUpgrade::new()
        // The contract caps one message at 32 MiB; the transport must refuse a
        // larger frame before the application sees it.
        .max_message_size(limits::MAX_WS_MESSAGE_BYTES)
        .max_frame_size(limits::MAX_WS_MESSAGE_BYTES)
        .upgrade(req, res, move |ws| async move {
            let mut ws = ws;
            // The handshake has completed. Acquire here so a client that never
            // finishes upgrading cannot occupy a slot.
            let Some(_slot) = ConnectionSlot::acquire(
                &connection_state.connections,
                &connection_state.connection_origins,
                connection_key.as_deref(),
            ) else {
                let _ = ws
                    .send(Message::close_with(
                        1013_u16,
                        "too many WebSocket connections",
                    ))
                    .await;
                return;
            };
            run(connection_state, origin, ws).await;
        })
        .await;
    if result.is_err() {
        res.status_code(StatusCode::UPGRADE_REQUIRED);
    }
}

/// What the connection loop acts on next.
enum Next {
    /// One frame from the client, or `None` when the socket closed or failed.
    Inbound(Option<Message>),
    /// One serialized event waiting to be written.
    Outbound(String),
    /// No inbound frame arrived before the connection's idle deadline.
    Idle,
}

/// Wait for an outbound event or a client frame. Outbound events are drained
/// first so a chatty job cannot starve requests; the caller moves the deadline
/// forward on every frame it handles, so only silence in both directions
/// reaches [`Next::Idle`].
async fn next_message(
    ws: &mut WebSocket,
    out: &mut Receiver<String>,
    deadline: tokio::time::Instant,
) -> Next {
    tokio::select! {
        biased;
        Some(text) = out.recv() => Next::Outbound(text),
        result = tokio::time::timeout_at(deadline, ws.recv()) => match result {
            Ok(Some(Ok(message))) => Next::Inbound(Some(message)),
            Ok(Some(Err(_))) | Ok(None) => Next::Inbound(None),
            Err(_) => Next::Idle,
        },
    }
}

async fn run(state: Arc<AppState>, origin: Option<String>, mut ws: WebSocket) {
    let first = match tokio::time::timeout(HELLO_TIMEOUT, ws.recv()).await {
        Ok(Some(Ok(message))) => message,
        Ok(_) => return,
        Err(_) => {
            // No authenticated request exists yet, so the connection closes
            // instead of waiting forever for a `hello`.
            let _ = ws
                .send(Message::close_with(1008_u16, "hello timeout"))
                .await;
            return;
        }
    };
    if !first.is_text() {
        let rejection = unparsed_rejection(
            &state,
            ErrorCode::Protocol,
            "hello must be a text frame",
            origin.as_deref(),
        );
        send_rejected(&mut ws, &rejection).await;
        return;
    }
    let first_text = first.as_str().unwrap_or_default();
    if first_text.len() > MAX_HELLO_BYTES {
        let message = format!("hello exceeds {MAX_HELLO_BYTES} bytes");
        let rejection =
            unparsed_rejection(&state, ErrorCode::TooLarge, &message, origin.as_deref());
        send_rejected(&mut ws, &rejection).await;
        return;
    }
    let hello = match parse_hello(first_text) {
        Ok(hello) => hello,
        Err(message) => {
            let rejection =
                unparsed_rejection(&state, ErrorCode::Protocol, &message, origin.as_deref());
            send_rejected(&mut ws, &rejection).await;
            return;
        }
    };

    let session = match authenticate(&state, &hello, origin.as_deref()).await {
        Ok(session) => session,
        Err(rejection) => {
            send_rejected(&mut ws, &rejection).await;
            return;
        }
    };
    if send_value(&mut ws, &welcome(&state, &session))
        .await
        .is_err()
    {
        return;
    }

    let (out_tx, mut out_rx) = channel::<String>(OUTBOUND_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    // The broadcast receiver needs its own task: a borrowed `recv` inside the
    // select would not keep the connection alive on its own.
    let mut job_events = state
        .job_events
        .subscribe(session.origin.as_deref().unwrap_or_default());
    let forward_tx = out_tx.clone();
    let forward_dropped = Arc::clone(&dropped);
    let forward = tokio::spawn(async move {
        loop {
            match job_events.recv().await {
                Ok(text) => {
                    if !observability::enqueue_event(&forward_tx, &forward_dropped, text) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut trace_task: Option<tokio::task::JoinHandle<()>> = None;
    // In-flight jobs started through this socket keep it alive even when a
    // long render emits no frames; the idle timeout is for a connection with
    // nothing left to do, not for a busy one.
    let mut in_flight: HashSet<String> = HashSet::new();
    let mut idle = IdleTimer::new(idle_timeout());
    loop {
        match next_message(&mut ws, &mut out_rx, idle.deadline()).await {
            Next::Idle => {
                if keep_alive_for_in_flight(&state, &mut in_flight) {
                    idle.touch();
                    continue;
                }
                let _ = ws.send(Message::close_with(1001_u16, "idle timeout")).await;
                break;
            }
            Next::Outbound(text) => {
                // Any traffic, in either direction, proves the connection is
                // still wanted; a steady event stream must not be mistaken for
                // an idle socket after one timeout window.
                idle.touch();
                if ws.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            Next::Inbound(None) => break,
            Next::Inbound(Some(message)) => {
                idle.touch();
                if message.is_close() {
                    break;
                }
                if !message.is_text() {
                    let response = error_response(
                        String::new(),
                        ErrorCode::Protocol,
                        "requests must be JSON text frames".to_owned(),
                        None,
                    );
                    if send_value(&mut ws, &response).await.is_err() {
                        break;
                    }
                    continue;
                }
                let text = message.as_str().unwrap_or_default();
                if text.len() > limits::MAX_WS_MESSAGE_BYTES {
                    let response = error_response(
                        String::new(),
                        ErrorCode::TooLarge,
                        format!("message exceeds {} bytes", limits::MAX_WS_MESSAGE_BYTES),
                        None,
                    );
                    let _ = send_value(&mut ws, &response).await;
                    break;
                }
                let request: ProtocolRequest = match serde_json::from_str(text) {
                    Ok(request) => request,
                    Err(error) => {
                        let id = serde_json::from_str::<Value>(text)
                            .ok()
                            .and_then(|value| {
                                value.get("id").and_then(Value::as_str).map(str::to_owned)
                            })
                            .unwrap_or_default();
                        let response = error_response(
                            id,
                            ErrorCode::Protocol,
                            format!("cannot parse the request: {error}"),
                            None,
                        );
                        if send_value(&mut ws, &response).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let pending_idempotency =
                    request_idempotency_key(&session, &request.method, &request.params)
                        .ok()
                        .flatten();
                // Hold the per-key lock until the response is written, the job
                // is started and the cache is updated; a concurrent duplicate
                // waits here instead of reserving a second job.
                let key_lock = pending_idempotency.as_ref().map(|key| {
                    let mut locks = state
                        .idempotency_locks
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    super::prune_idempotency_locks(&mut locks);
                    Arc::clone(
                        locks
                            .entry(key.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
                    )
                });
                let _idem_guard = match key_lock {
                    Some(lock) => Some(lock.lock_owned().await),
                    None => None,
                };
                match dispatch(&state, &session, request).await {
                    Dispatch::Reply(response) => {
                        if send_value(&mut ws, &response).await.is_err() {
                            break;
                        }
                    }
                    Dispatch::Start { response, start } => {
                        // The response names the job id; only then may the pool
                        // report its queue position, or the client would see an
                        // event for an id it has not learned yet.
                        if let Some(job_id) = response
                            .result
                            .as_ref()
                            .and_then(|result| result.get("jobId"))
                            .and_then(Value::as_str)
                        {
                            in_flight.insert(job_id.to_owned());
                        }
                        if send_value(&mut ws, &response).await.is_err() {
                            break;
                        }
                        start();
                        remember_idempotent(&state.idempotency, pending_idempotency, &response);
                    }
                    Dispatch::Subscribe { response, plan } => {
                        // The acknowledgement precedes the first streamed
                        // event, so a client always knows the subscription
                        // exists before it has events to store.
                        if send_value(&mut ws, &response).await.is_err() {
                            break;
                        }
                        if let Some(task) = trace_task.take() {
                            task.abort();
                        } else {
                            state.plugins.subscribe_begin();
                        }
                        trace_task = Some(observability::start_stream(
                            plan,
                            state.plugins.observability_gate(),
                            out_tx.clone(),
                            Arc::clone(&dropped),
                        ));
                    }
                }
            }
        }
    }
    // A window belongs to the connection that opened it; a dropped socket must
    // not leave an interactive window the client can no longer close.
    state
        .previews
        .close_session(state.as_ref(), session.connection)
        .await;
    forward.abort();
    if trace_task.is_some() {
        state.plugins.subscribe_end();
    }
    if let Some(task) = trace_task {
        task.abort();
    }
}

fn parse_hello(text: &str) -> Result<Hello, String> {
    let mut value: Value =
        serde_json::from_str(text).map_err(|error| format!("cannot parse hello: {error}"))?;
    if value.get("type").and_then(Value::as_str) != Some("hello") {
        return Err("the first frame must be a hello".to_owned());
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    serde_json::from_value(value).map_err(|error| format!("invalid hello: {error}"))
}

fn welcome(state: &AppState, session: &Session) -> Value {
    let families = state.plugins.active_families();
    welcome_with(session, &families)
}

fn welcome_with(session: &Session, families: &[String]) -> Value {
    let capabilities = Vec::new();
    let welcome = Welcome {
        protocol: PROTOCOL_VERSION,
        engine: env!("CARGO_PKG_VERSION").to_owned(),
        granted: session.scopes.clone(),
        capabilities,
        plugin_families: Some(families.to_vec()),
        warnings: Vec::new(),
    };
    let mut value = serde_json::to_value(welcome).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_owned(), json!("welcome"));
    }
    value
}

fn rejected(rejection: &Rejection) -> Value {
    let rejected = Rejected {
        error: ErrorBody {
            code: rejection.code,
            message: rejection.message.clone(),
            detail: Some(rejection.detail()),
        },
    };
    let mut value = serde_json::to_value(rejected).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_owned(), json!("rejected"));
    }
    value
}

async fn send_rejected(ws: &mut WebSocket, rejection: &Rejection) {
    let _ = send_value(ws, &rejected(rejection)).await;
}

/// Every outbound frame is a JSON value the client switches on by `type`.
async fn send_value<T>(ws: &mut WebSocket, value: &T) -> Result<(), ()>
where
    T: serde::Serialize + ?Sized,
{
    let text = serde_json::to_string(value).map_err(|_| ())?;
    ws.send(Message::text(text)).await.map_err(|_| ())
}

/// A handshake frame that never became a `hello`. It still produces an `auth`
/// event so the client can quote the trace id in a report.
fn unparsed_rejection(
    state: &AppState,
    code: ErrorCode,
    message: &str,
    origin: Option<&str>,
) -> Rejection {
    let trace_id = TraceId::new();
    let tracer = Tracer::new(state.sink.as_ref(), trace_id.clone());
    let mut event = tracer
        .event(Stage::Auth)
        .failed(code.as_str())
        .detail(message.to_owned());
    if let Some(origin) = origin {
        event = event.origin(origin.to_owned());
    }
    event.emit();
    Rejection {
        code,
        message: message.to_owned(),
        trace_id,
    }
}

/// Outcome of one request: a response now, or a response plus a reserved job
/// that only becomes schedulable after the response is written.
pub enum Dispatch {
    Reply(ProtocolResponse),
    Start {
        response: ProtocolResponse,
        /// Activated only after the response frame is written.
        start: Box<dyn FnOnce() + Send>,
    },
    Subscribe {
        response: ProtocolResponse,
        plan: TracePlan,
    },
}

/// Longest `requestId` this service remembers, in bytes. It matches the HTTP
/// `Idempotency-Key` limit so both transports refuse oversized keys.
const MAX_REQUEST_ID_BYTES: usize = 128;

/// Whether a `requestId` can be remembered and echoed safely.
///
/// Printable ASCII keeps the value log- and header-friendly and makes the
/// idempotency key unambiguous; whitespace-only values are already trimmed and
/// refused by the length check.
fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Route one request. Scope is checked from the frozen method table before
/// the method body runs.
pub async fn dispatch(
    state: &Arc<AppState>,
    session: &Session,
    request: ProtocolRequest,
) -> Dispatch {
    let mut request = request;
    let id = request.id.clone();
    let Some(descriptor) = METHODS
        .iter()
        .find(|descriptor| descriptor.method == request.method)
    else {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Unsupported,
            format!("unknown method: {}", request.method),
            None,
        ));
    };
    if let Some(required) = descriptor.scope
        && !session.has(required)
    {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Auth,
            format!(
                "the token does not grant the {} scope required by {}",
                match required {
                    Scope::Print => "print",
                    Scope::Raw => "raw",
                    Scope::Admin => "admin",
                },
                request.method
            ),
            None,
        ));
    }

    if let Some(plugin) = descriptor.plugin
        && !state.plugins.active(plugin)
    {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Unsupported,
            format!("plugin {plugin} is disabled"),
            Some(json!({
                "plugin": plugin,
                "enable": format!("printkit plugin enable {plugin}"),
            })),
        ));
    }

    if matches!(
        request.method.as_str(),
        "doc.print" | "doc.printBatch" | "doc.printPdf" | "raw.send"
    ) && state.draining.load(std::sync::atomic::Ordering::SeqCst)
    {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Unsupported,
            "the service is shutting down".to_owned(),
            None,
        ));
    }
    // A removed origin applies to every method on an existing socket, not
    // only print/raw: records, printers, job status and logs are equally
    // origin-scoped.
    if let Some(origin) = session.origin.as_deref()
        && state
            .revoked_origins
            .lock()
            .is_ok_and(|revoked| revoked.contains(origin))
    {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Origin,
            "this origin is no longer allowlisted".to_owned(),
            None,
        ));
    }
    let idempotency_key = match request_idempotency_key(session, &request.method, &request.params) {
        Ok(key) => key,
        Err(message) => {
            return Dispatch::Reply(error_response(id, ErrorCode::Schema, message, None));
        }
    };
    if request.method == "doc.print"
        && let Some(params) = request.params.as_object_mut()
    {
        params.remove("requestId");
    }
    if let Some(key) = idempotency_key.as_deref()
        && let Ok(mut cache) = state.idempotency.lock()
        && let Some(entry) = cache.get(key)
    {
        let result = match request.method.as_str() {
            "doc.printBatch" => json!({
                "jobId": entry.job_id,
                "docCount": entry.doc_count,
                "pageCount": entry.page_count
            }),
            "doc.printPdfBatch" => json!({
                "jobId": entry.job_id,
                "pages": entry.page_count
            }),
            _ => json!({ "jobId": entry.job_id }),
        };
        return Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: Some(result),
            warnings: Vec::new(),
            error: None,
        });
    }
    match request.method.as_str() {
        "sys.info" => Dispatch::Reply(sys_info(state, id)),
        "printers.list" => Dispatch::Reply(printers_list(state, id).await),
        "printers.papers" => Dispatch::Reply(printers_papers(state, id, request.params).await),
        "job.status" => Dispatch::Reply(job_status(state, session, id, request.params)),
        "job.cancel" => Dispatch::Reply(cancel_job(state, session, id, request.params).await),
        "trace.subscribe" => trace_subscribe(state, id, request.params),
        "trace.tail" => trace_tail(state, id, request.params).await,
        "logs.export" => export_logs(state, id, request.params).await,
        "records.list" => list_records(state, session, id, request.params).await,
        "records.stats" => records_stats(state, session, id, request.params).await,
        "records.summary" => records_summary(state, session, id, request.params).await,
        "records.export" => records_export(state, session, id, request.params).await,
        "doc.print" => print_document(state, session, id, request.params).await,
        "doc.printBatch" => print_batch(state, session, id, request.params).await,
        "doc.printPdf" => print_pdf(state, session, id, request.params).await,
        "doc.printPdfBatch" => print_pdf_batch(state, session, id, request.params).await,
        "doc.preview" => preview_document(state, session, id, request.params).await,
        "preview.pages" => preview_pages(state, session, id, request.params).await,
        "preview.close" => preview_close(state, session, id, request.params).await,
        "doc.exportPdf" => Dispatch::Reply(export_pdf(state, id, request.params).await),
        "raw.send" => raw_send(state, session, id, request.params).await,
        _ => Dispatch::Reply(error_response(
            id,
            ErrorCode::Unsupported,
            format!("{} is not implemented by this build", request.method),
            None,
        )),
    }
}

/// Validate a client `requestId` and build the namespaced cache key.
///
/// Kept separate from dispatch so the connection loop can remember the key
/// only after the response was written and the job was started.
pub(crate) fn request_idempotency_key(
    session: &Session,
    method: &str,
    params: &Value,
) -> Result<Option<String>, String> {
    let Some(value) = params
        .as_object()
        .and_then(|params| params.get("requestId"))
    else {
        return Ok(None);
    };
    let Value::String(value) = value else {
        return Err("requestId must be a string".to_owned());
    };
    let value = value.trim();
    if !valid_request_id(value) {
        return Err("requestId must be 1-128 printable ASCII characters".to_owned());
    }
    Ok(Some(super::idempotency_key(
        "ws",
        method,
        session.origin.as_deref(),
        value,
    )))
}

/// Remember a job only after its response was written and `start()` ran.
///
/// A send failure or a cancelled reservation leaves no cache entry, so a
/// reconnect with the same `requestId` starts a fresh job.
pub(crate) fn remember_idempotent(
    cache: &std::sync::Mutex<super::IdempotencyCache>,
    key: Option<String>,
    response: &ProtocolResponse,
) {
    let Some(key) = key else {
        return;
    };
    let Some(result) = response.result.as_ref() else {
        return;
    };
    let Some(job_id) = result.get("jobId").and_then(Value::as_str) else {
        return;
    };
    let doc_count = result.get("docCount").and_then(Value::as_u64).unwrap_or(0) as u32;
    let page_count = result
        .get("pageCount")
        .or_else(|| result.get("pages"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(
            key,
            super::IdempotencyEntry {
                job_id: job_id.to_owned(),
                doc_count,
                page_count,
            },
        );
    }
}

fn empty_result(id: String) -> ProtocolResponse {
    ProtocolResponse {
        id,
        ok: true,
        result: serde_json::to_value(Empty {}).ok(),
        warnings: Vec::new(),
        error: None,
    }
}

fn observability_reply(id: String, error: ObservabilityError) -> Dispatch {
    Dispatch::Reply(error_response(id, error.code, error.message, None))
}

fn parse_params<T>(id: &str, params: Value) -> Result<T, Box<Dispatch>>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(params).map_err(|error| {
        Box::new(Dispatch::Reply(error_response(
            id.to_owned(),
            ErrorCode::Schema,
            format!("invalid parameters: {error}"),
            Some(json!({ "message": error.to_string() })),
        )))
    })
}

fn parse_optional_params<T>(id: &str, params: Value) -> Result<T, Box<Dispatch>>
where
    T: serde::de::DeserializeOwned + Default,
{
    if params.is_null() {
        return Ok(T::default());
    }
    parse_params(id, params)
}

fn trace_subscribe(state: &AppState, id: String, params: Value) -> Dispatch {
    let params: TraceSubscribeParams = match parse_optional_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    match observability::plan_subscription(state, params) {
        Ok(plan) => Dispatch::Subscribe {
            response: empty_result(id),
            plan,
        },
        Err(error) => observability_reply(id, error),
    }
}

async fn trace_tail(state: &AppState, id: String, params: Value) -> Dispatch {
    let params: TraceTailParams = match parse_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let dir = printkit_facade::logs_dir(&state.data_dir);
    let result =
        tokio::task::spawn_blocking(move || observability::tail_events(&dir, params)).await;
    match result {
        Ok(Ok(result)) => Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(result).ok(),
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Err(error)) => observability_reply(id, error),
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the trace task failed: {error}"),
            None,
        )),
    }
}

async fn export_logs(state: &AppState, id: String, params: Value) -> Dispatch {
    let params: LogsExportParams = match parse_optional_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let data_dir = state.data_dir.clone();
    let ttl = state.export_ttl_hours;
    let doctor = state.doctor_report.clone();
    let result = tokio::task::spawn_blocking(move || {
        observability::export_bundle(&data_dir, ttl, &doctor, params)
    })
    .await;
    match result {
        Ok(Ok(result)) => Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(result).ok(),
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Err(error)) => observability_reply(id, error),
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the export task failed: {error}"),
            None,
        )),
    }
}

async fn list_records(state: &AppState, session: &Session, id: String, params: Value) -> Dispatch {
    let params: RecordsListParams = match parse_optional_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let domain = match state.plugins.records_domain() {
        Some(domain) => domain,
        None => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Unsupported,
                "plugin records is disabled".to_owned(),
                None,
            ));
        }
    };
    let query = match plugin_query(session, params.query, params.cursor, params.limit) {
        Ok(query) => query,
        Err(message) => {
            return Dispatch::Reply(error_response(id, ErrorCode::Auth, message, None));
        }
    };
    let result = tokio::task::spawn_blocking(move || domain.list(&query)).await;
    match result {
        Ok(Ok(result)) => Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(RecordsListResult {
                records: result.records.into_iter().map(row_view).collect(),
                next_cursor: result.next_cursor,
            })
            .ok(),
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Err(message)) => {
            Dispatch::Reply(error_response(id, ErrorCode::JobFailed, message, None))
        }
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the records task failed: {error}"),
            None,
        )),
    }
}

async fn records_stats(state: &AppState, session: &Session, id: String, params: Value) -> Dispatch {
    let params: RecordsStatsParams = match parse_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let domain = match state.plugins.records_domain() {
        Some(domain) => domain,
        None => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Unsupported,
                "plugin records is disabled".to_owned(),
                None,
            ));
        }
    };
    let query = match plugin_query(session, params.query, None, None) {
        Ok(query) => query,
        Err(message) => {
            return Dispatch::Reply(error_response(id, ErrorCode::Auth, message, None));
        }
    };
    let group = match params.group_by {
        StatsGroupView::Tag => "tag",
        StatsGroupView::Template => "template",
        StatsGroupView::Printer => "printer",
        StatsGroupView::Day => "day",
    };
    let result = tokio::task::spawn_blocking(move || domain.stats(&query, group)).await;
    match result {
        Ok(Ok(buckets)) => Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(RecordsStatsResult {
                buckets: buckets.iter().map(bucket_view).collect(),
            })
            .ok(),
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Err(message)) => {
            Dispatch::Reply(error_response(id, ErrorCode::JobFailed, message, None))
        }
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the statistics task failed: {error}"),
            None,
        )),
    }
}

async fn records_summary(
    state: &AppState,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let params: RecordsSummaryParams = match parse_optional_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let domain = match state.plugins.records_domain() {
        Some(domain) => domain,
        None => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Unsupported,
                "plugin records is disabled".to_owned(),
                None,
            ));
        }
    };
    let query = match plugin_query(session, params.query, None, None) {
        Ok(query) => query,
        Err(message) => {
            return Dispatch::Reply(error_response(id, ErrorCode::Auth, message, None));
        }
    };
    let result = tokio::task::spawn_blocking(move || domain.summary(&query)).await;
    match result {
        Ok(Ok(summary)) => {
            let (slowest_printer, slowest_avg_dur_ms) = match summary.slowest_printer {
                Some(printer) => (Some(printer), summary.slowest_avg_dur_ms),
                None => (None, 0.0),
            };
            Dispatch::Reply(ProtocolResponse {
                id,
                ok: true,
                result: serde_json::to_value(RecordsSummaryView {
                    today_sheets: summary.today_sheets,
                    today_done_sheets: summary.today_done_sheets,
                    today_done: summary.today_done,
                    today_failed: summary.today_failed,
                    today_cancelled: summary.today_cancelled,
                    total: summary.total,
                    failed: summary.failed,
                    cancelled: summary.cancelled,
                    pending: summary.pending,
                    avg_dur_ms: summary.avg_dur_ms,
                    slowest_printer,
                    slowest_avg_dur_ms,
                })
                .ok(),
                warnings: Vec::new(),
                error: None,
            })
        }
        Ok(Err(message)) => {
            Dispatch::Reply(error_response(id, ErrorCode::JobFailed, message, None))
        }
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the summary task failed: {error}"),
            None,
        )),
    }
}

async fn records_export(
    state: &AppState,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let params: RecordsExportParams = match parse_optional_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return *reply,
    };
    let domain = match state.plugins.records_domain() {
        Some(domain) => domain,
        None => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Unsupported,
                "plugin records is disabled".to_owned(),
                None,
            ));
        }
    };
    let query = match plugin_query(session, params.query, None, None) {
        Ok(query) => query,
        Err(message) => {
            return Dispatch::Reply(error_response(id, ErrorCode::Auth, message, None));
        }
    };
    let result = tokio::task::spawn_blocking(move || domain.csv(&query)).await;
    match result {
        Ok(Ok(csv)) => Dispatch::Reply(ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(RecordsExportResult {
                csv: String::from_utf8_lossy(&csv).into_owned(),
            })
            .ok(),
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Err(message)) => {
            Dispatch::Reply(error_response(id, ErrorCode::JobFailed, message, None))
        }
        Err(error) => Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the export task failed: {error}"),
            None,
        )),
    }
}

fn plugin_query(
    session: &Session,
    query: printkit_protocol::RecordsQuery,
    cursor: Option<i64>,
    limit: Option<u32>,
) -> Result<ApiRecordQuery, String> {
    let scope = if session.has(Scope::Admin) {
        RecordScope::All
    } else {
        RecordScope::Origin(
            session
                .origin
                .clone()
                .ok_or_else(|| "the token is not bound to an origin".to_owned())?,
        )
    };
    Ok(ApiRecordQuery {
        scope,
        from: query.from,
        to: query.to,
        tags: query.tags,
        printer: query.printer,
        status: query.status.map(|status| status.as_str().to_owned()),
        cursor,
        limit: limit.unwrap_or(100).clamp(1, 500),
    })
}

fn row_view(row: RecordRow) -> RecordView {
    RecordView {
        id: row.id,
        seq: row.seq,
        at: row.at,
        origin: row.origin,
        template: row.template,
        tags: row.tags,
        printer: row.printer,
        pages: row.pages,
        copies: row.copies,
        status: row.status,
        dur_ms: row.dur_ms,
        error_code: row.error_code,
        trace_id: row.trace_id,
    }
}

fn bucket_view(bucket: &ApiRecordStats) -> StatsBucketView {
    StatsBucketView {
        key: bucket.key.clone(),
        count: bucket.count,
        sheets: bucket.sheets,
        avg_dur_ms: bucket.avg_dur_ms,
    }
}

/// The platform this binary was built for. The facade refuses other hosts.
fn platform_id() -> Platform {
    #[cfg(target_os = "macos")]
    {
        Platform::Macos
    }
    #[cfg(windows)]
    {
        Platform::Windows
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Platform::Linux
    }
}

fn sys_info(state: &AppState, id: String) -> ProtocolResponse {
    // Capability bits are the implemented behaviour, not the platform's
    // potential. A bit flips only when the matching entry point is delivered.
    let capabilities = Capabilities {
        duplex: state.print_backend.duplex_supported(),
        paper_source: false,
        cancel_submitted: state.print_backend.cancel_submitted_supported(),
        // Raw jobs carry a spooler id on both current backends; the backend
        // owns this bit rather than the service hardcoding it.
        cancel_raw: state.print_backend.cancel_raw_supported(),
        preview: state.render_host.preview_available(),
        raster_fallback: false,
        // Raw submission exists on both platforms; a printer whose driver
        // cannot take RAW still answers RawUnsupported per queue.
        raw: true,
        // Both shipped backends hand a merged PDF to the OS print system
        // (Windows through the WebView2 PDF viewer); neither gets a spooler id.
        batch: state.print_backend.prints_pdf(),
        pdf_passthrough: state.print_backend.prints_pdf(),
        render_workers: state.pool.workers(),
    };
    let info = SysInfo {
        engine: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PROTOCOL_VERSION,
        platform: platform_id(),
        capabilities,
    };
    ProtocolResponse {
        id,
        ok: true,
        result: serde_json::to_value(info).ok(),
        warnings: Vec::new(),
        error: None,
    }
}

async fn printers_list(state: &AppState, id: String) -> ProtocolResponse {
    let backend = Arc::clone(&state.print_backend);
    let result = tokio::task::spawn_blocking(move || backend.printers()).await;
    match result {
        Ok(Ok(printers)) => {
            let listed: Vec<printkit_protocol::Printer> = printers
                .into_iter()
                .map(|printer| printkit_protocol::Printer {
                    name: printer.name,
                    is_default: printer.is_default,
                    raw: printer.raw,
                    mime_types: printer.mime_types,
                    duplex: printer.duplex,
                    paper_source: printer.paper_source,
                })
                .collect();
            ProtocolResponse {
                id,
                ok: true,
                result: serde_json::to_value(listed).ok(),
                warnings: Vec::new(),
                error: None,
            }
        }
        Ok(Err(error)) => platform_reply(id, &error),
        Err(error) => error_response(
            id,
            ErrorCode::JobFailed,
            format!("the printer list task failed: {error}"),
            None,
        ),
    }
}

async fn printers_papers(state: &AppState, id: String, params: Value) -> ProtocolResponse {
    let params: PrintersPapersParams = match parse_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return reply_response(*reply),
    };
    let backend = Arc::clone(&state.print_backend);
    let name = params.name;
    let result = tokio::task::spawn_blocking(move || backend.papers(&name)).await;
    match result {
        Ok(Ok(papers)) => {
            let listed: Vec<PaperSize> = papers
                .into_iter()
                .map(|paper| PaperSize {
                    name: paper.name,
                    width_mm: paper.width_mm,
                    height_mm: paper.height_mm,
                    kind: if paper.named {
                        PaperKind::Named
                    } else {
                        PaperKind::Custom
                    },
                })
                .collect();
            ProtocolResponse {
                id,
                ok: true,
                result: serde_json::to_value(listed).ok(),
                warnings: Vec::new(),
                error: None,
            }
        }
        Ok(Err(error)) => platform_reply(id, &error),
        Err(error) => error_response(
            id,
            ErrorCode::JobFailed,
            format!("the paper list task failed: {error}"),
            None,
        ),
    }
}

fn reply_response(dispatch: Dispatch) -> ProtocolResponse {
    match dispatch {
        Dispatch::Reply(response) => response,
        Dispatch::Start { response, .. } => response,
        Dispatch::Subscribe { response, .. } => response,
    }
}

fn platform_reply(id: String, error: &printkit_facade::PlatformError) -> ProtocolResponse {
    error_response(id, platform_error_code(error), error.to_string(), None)
}

fn platform_error_code(error: &printkit_facade::PlatformError) -> ErrorCode {
    use printkit_facade::PlatformError;
    match error {
        PlatformError::PrinterNotFound(_) | PlatformError::NoDefaultPrinter => {
            ErrorCode::PrinterNotFound
        }
        PlatformError::Unavailable(_) | PlatformError::Unsupported(_) => ErrorCode::Unsupported,
        PlatformError::Timeout(_) => ErrorCode::Timeout,
        PlatformError::Print(_) | PlatformError::Render(_) => ErrorCode::JobFailed,
        PlatformError::CancelUnsupported => ErrorCode::CancelUnsupported,
        PlatformError::RawUnsupported(_) => ErrorCode::RawUnsupported,
        PlatformError::WindowNotFound(_) | PlatformError::JobNotFound { .. } => ErrorCode::NotFound,
    }
}

fn job_status(state: &AppState, session: &Session, id: String, params: Value) -> ProtocolResponse {
    let params: JobIdParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                ErrorCode::Schema,
                format!("invalid job.status parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            );
        }
    };
    let job_id = JobId::from(params.job_id);
    if state
        .print_jobs
        .get_owned(&job_id, session.origin.as_deref())
        .is_none()
    {
        return error_response(
            id,
            ErrorCode::NotFound,
            format!("unknown job: {job_id}"),
            None,
        );
    }
    let Some(status) = state.pool.status(&job_id) else {
        return error_response(
            id,
            ErrorCode::NotFound,
            format!("unknown job: {job_id}"),
            None,
        );
    };
    let body = printkit_protocol::JobStatus {
        job_id: job_id.as_str().to_owned(),
        status: job_state(status.phase),
        queue_pos: status.queue_pos,
        pages: state
            .job_pages
            .lock()
            .ok()
            .and_then(|pages| pages.get(job_id.as_str()).copied()),
        warnings: status
            .warnings
            .iter()
            .filter_map(|warning| warning_body(state, warning))
            .collect(),
        error: status.failure.map(job_error),
    };
    ProtocolResponse {
        id,
        ok: true,
        result: serde_json::to_value(body).ok(),
        warnings: Vec::new(),
        error: None,
    }
}

pub(crate) fn remember_pages(state: &AppState, job_id: &str, pages: u32) {
    let Ok(mut known) = state.job_pages.lock() else {
        return;
    };
    if known.len() >= 4096 {
        known.clear();
    }
    known.insert(job_id.to_owned(), pages);
}

fn job_state(phase: JobPhase) -> JobState {
    match phase {
        JobPhase::Queued => JobState::Queued,
        JobPhase::Printing => JobState::Printing,
        JobPhase::Done => JobState::Done,
        JobPhase::Failed => JobState::Failed,
        JobPhase::Cancelled => JobState::Cancelled,
    }
}

fn job_error(failure: JobFailure) -> JobError {
    JobError {
        code: error_code(failure.code),
        message: failure.message,
    }
}

async fn cancel_job(
    state: &AppState,
    session: &Session,
    id: String,
    params: Value,
) -> ProtocolResponse {
    let params: JobIdParams = match parse_params(&id, params) {
        Ok(params) => params,
        Err(reply) => return reply_response(*reply),
    };
    let job_id = JobId::from(params.job_id);
    let Some(control) = state
        .print_jobs
        .get_owned(&job_id, session.origin.as_deref())
    else {
        return error_response(
            id,
            ErrorCode::NotFound,
            format!("unknown job: {job_id}"),
            None,
        );
    };
    control.request_cancel();
    // `Done` means the document reached the platform's print path, not that the
    // sheet was printed, so a submitted job is retracted through the platform
    // even when its pool record is already terminal. WebView2 `Print` jobs carry
    // no spooler id and answer CancelUnsupported.
    if let Some(submitted) = control.submitted() {
        let backend = Arc::clone(&state.print_backend);
        return match tokio::task::spawn_blocking(move || backend.cancel_submitted(&submitted)).await
        {
            Ok(Ok(())) => empty_result(id),
            Ok(Err(error)) => platform_reply(id, &error),
            Err(error) => error_response(
                id,
                ErrorCode::JobFailed,
                format!("the cancel task failed: {error}"),
                None,
            ),
        };
    }
    match state.pool.cancel_job(&job_id) {
        Some(printkit_facade::CancelDisposition::Queued)
        | Some(printkit_facade::CancelDisposition::Requested) => empty_result(id),
        Some(printkit_facade::CancelDisposition::ExternalQueued) => {
            send_job_event(
                &state.job_events,
                state.sink.as_ref(),
                session.origin.as_deref().unwrap_or_default(),
                &job_id,
                &JobUpdate::Cancelled,
            );
            empty_result(id)
        }
        Some(printkit_facade::CancelDisposition::Terminal) | None => error_response(
            id,
            ErrorCode::NotFound,
            format!("job {job_id} already finished"),
            None,
        ),
    }
}

async fn print_document(
    state: &Arc<AppState>,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let document = match serde_json::to_vec(&params) {
        Ok(document) => document,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("cannot encode the document: {error}"),
                None,
            ));
        }
    };
    let notify = job_notifier(state, session);
    let usage = usage_recorder(state, session);
    let worker = Arc::clone(state);
    let origin = session.origin.clone();
    let result = run_reserve(id.clone(), move || {
        printkit_facade::reserve_print(
            &document,
            &worker.pool,
            worker.render_host.as_ref(),
            Arc::clone(&worker.print_backend),
            &worker.print_jobs,
            worker.sink.clone(),
            &worker.fetch,
            notify,
            usage,
        )
    })
    .await;
    let reserved = match result {
        Ok(reserved) => reserved,
        Err(dispatch) => return dispatch,
    };
    state
        .print_jobs
        .bind_origin(&reserved.job_id, origin.as_deref());
    remember_pages(state, reserved.job_id.as_str(), reserved.pages);
    let job_id = reserved.job_id.as_str().to_owned();
    Dispatch::Start {
        response: ProtocolResponse {
            id,
            ok: true,
            result: Some(json!({ "jobId": job_id })),
            warnings: Vec::new(),
            error: None,
        },
        start: Box::new(move || reserved.reservation.activate()),
    }
}

/// `doc.preview`: show the document in a native window.
///
/// The document bytes are kept for the window's Print control, which reserves
/// the job through the same [`printkit_facade::reserve_print`] call as
/// `doc.print`. Preview itself only serialises HTML through the engine.
async fn preview_document(
    state: &Arc<AppState>,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let document = match serde_json::to_vec(&params) {
        Ok(document) => document,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("cannot encode the document: {error}"),
                None,
            ));
        }
    };
    let on_print = preview_print_action(state, session, Arc::new(document.clone()));
    match state
        .previews
        .open(state, session, document, on_print)
        .await
    {
        Ok(opened) => {
            let result = WindowRef {
                window_id: opened.window.get().to_string(),
            };
            Dispatch::Reply(ProtocolResponse {
                id,
                ok: true,
                result: serde_json::to_value(result).ok(),
                warnings: opened
                    .warnings
                    .iter()
                    .filter_map(|warning| warning_body(state.as_ref(), warning))
                    .collect(),
                error: None,
            })
        }
        Err(error) => preview_error(id, error),
    }
}

/// `preview.pages`: export one page of a live window as PNG.
async fn preview_pages(state: &AppState, session: &Session, id: String, params: Value) -> Dispatch {
    let params: PreviewPagesParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid preview.pages parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    let window = match preview_window(&params.window_id, &id) {
        Ok(window) => window,
        Err(reply) => return *reply,
    };
    match state
        .previews
        .pages(state, session, window, params.page_index, params.dpi)
        .await
    {
        Ok(png) => {
            let png = base64::engine::general_purpose::STANDARD.encode(png);
            Dispatch::Reply(ProtocolResponse {
                id,
                ok: true,
                result: Some(json!({ "png": png })),
                warnings: Vec::new(),
                error: None,
            })
        }
        Err(error) => preview_error(id, error),
    }
}

/// `preview.close`: destroy the window and invalidate its handle.
async fn preview_close(state: &AppState, session: &Session, id: String, params: Value) -> Dispatch {
    let params: PreviewCloseParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid preview.close parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    let window = match preview_window(&params.window_id, &id) {
        Ok(window) => window,
        Err(reply) => return *reply,
    };
    match state.previews.close(state, session, window).await {
        Ok(()) => Dispatch::Reply(empty_result(id)),
        Err(error) => preview_error(id, error),
    }
}

/// The window's Print control, moved off the GUI thread.
pub(crate) fn preview_print_action(
    state: &Arc<AppState>,
    session: &Session,
    document: Arc<Vec<u8>>,
) -> Arc<dyn Fn(WindowId) + Send + Sync> {
    let state = Arc::clone(state);
    let session = session.clone();
    Arc::new(move |_window| {
        let state = Arc::clone(&state);
        // The failure path below still needs the state after `work` runs;
        // keeping a second handle avoids moving it into the closure.
        let report = Arc::clone(&state);
        let document = Arc::clone(&document);
        let session = session.clone();
        let work = move || {
            let notify = job_notifier(&state, &session);
            let usage = usage_recorder(&state, &session);
            if let Ok(reserved) = printkit_facade::reserve_print(
                document.as_slice(),
                &state.pool,
                state.render_host.as_ref(),
                Arc::clone(&state.print_backend),
                &state.print_jobs,
                state.sink.clone(),
                &state.fetch,
                notify,
                usage,
            ) {
                reserved.reservation.activate();
            }
        };
        // The GUI thread must never run the job: rendering dispatches back to
        // the main loop, so blocking here would wait on itself. The render
        // pool queues the work; one thread per click is cheap.
        if let Err(error) = std::thread::Builder::new()
            .name("preview-print".to_owned())
            .spawn(work)
        {
            let tracer = Tracer::new(report.sink.as_ref(), TraceId::new());
            tracer
                .event(Stage::Submit)
                .failed("E_JOB_FAILED")
                .detail(format!("cannot start the preview print thread: {error}"))
                .emit();
        }
    })
}

fn preview_window(value: &str, id: &str) -> Result<WindowId, Box<Dispatch>> {
    value.parse::<u64>().map(WindowId::new).map_err(|_| {
        Box::new(Dispatch::Reply(error_response(
            id.to_owned(),
            ErrorCode::NotFound,
            format!("unknown windowId: {value}"),
            None,
        )))
    })
}

fn preview_error(id: String, error: PreviewError) -> Dispatch {
    Dispatch::Reply(error_response(
        id,
        error.code(),
        error.message(),
        error.detail(),
    ))
}
async fn print_batch(
    state: &Arc<AppState>,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let params: DocPrintBatchParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid doc.printBatch parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    let _batch_permit = match Arc::clone(&state.batch_slots).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::JobFailed,
                "the batch limiter is unavailable".to_owned(),
                None,
            ));
        }
    };
    let doc_count = u32::try_from(params.docs.len()).unwrap_or(u32::MAX);
    let mut parts = Vec::with_capacity(params.docs.len());
    for document in &params.docs {
        match serde_json::to_vec(document) {
            Ok(bytes) => parts.push(BatchPart::Document(bytes)),
            Err(error) => {
                return Dispatch::Reply(error_response(
                    id,
                    ErrorCode::Schema,
                    format!("cannot encode the document: {error}"),
                    None,
                ));
            }
        }
    }
    let request = PrintBatch {
        parts,
        printer: params.printer,
        copies: params.copies.unwrap_or(1),
        collate: params.collate.unwrap_or(true),
        title: "printkit batch".to_owned(),
    };
    let notify = job_notifier(state, session);
    let usage = usage_recorder(state, session);
    let worker = Arc::clone(state);
    let origin = session.origin.clone();
    let bound_origin = origin.clone();
    let result = run_reserve(id.clone(), move || {
        printkit_facade::begin_print_batch(
            request,
            &worker.pool,
            Arc::clone(&worker.print_backend),
            Arc::clone(&worker.render_host),
            &worker.print_jobs,
            worker.sink.clone(),
            worker.fetch,
            notify,
            bound_origin,
            usage,
        )
    })
    .await;
    let reserved = match result {
        Ok(reserved) => reserved,
        Err(dispatch) => return dispatch,
    };
    state
        .print_jobs
        .bind_origin(&reserved.job_id, origin.as_deref());
    remember_pages(state, reserved.job_id.as_str(), reserved.pages);
    let result = BatchResult {
        job_id: reserved.job_id.as_str().to_owned(),
        doc_count,
        page_count: reserved.pages,
    };
    Dispatch::Start {
        response: ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(result).ok(),
            warnings: Vec::new(),
            error: None,
        },
        start: Box::new(move || reserved.activate()),
    }
}

async fn print_pdf_batch(
    state: &Arc<AppState>,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let params: DocPrintPdfBatchParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid doc.printPdfBatch parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    if params.pdfs.is_empty() {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::Schema,
            "a PDF batch needs at least one PDF".to_owned(),
            Some(json!({ "pointer": "/pdfs" })),
        ));
    }
    if params.pdfs.len() > limits::MAX_BATCH_DOCS {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!("a PDF batch holds at most {} PDFs", limits::MAX_BATCH_DOCS),
            Some(json!({ "limit": limits::MAX_BATCH_DOCS })),
        ));
    }
    let encoded: usize = params.pdfs.iter().map(|part| part.bytes.len()).sum();
    if encoded > limits::MAX_WS_BASE64_BYTES {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!(
                "the WS PDF batch payload is {encoded} base64 bytes, above the {} byte WebSocket limit; use POST /print/batch for the 128 MiB path",
                limits::MAX_WS_BASE64_BYTES
            ),
            Some(json!({ "limit": limits::MAX_WS_BASE64_BYTES })),
        ));
    }
    let mut parts = Vec::with_capacity(params.pdfs.len());
    for (index, part) in params.pdfs.iter().enumerate() {
        let copies = part.copies.unwrap_or(1);
        if copies != 1 {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("PDF part {index}: copies must be 1; use the top-level copies"),
                Some(json!({ "pointer": format!("/pdfs/{index}/copies") })),
            ));
        }
        if part.bytes.len() > limits::MAX_WS_BASE64_BYTES {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::TooLarge,
                format!(
                    "PDF part {index}: the base64 payload exceeds {} bytes",
                    limits::MAX_WS_BASE64_BYTES
                ),
                Some(json!({ "limit": limits::MAX_WS_BASE64_BYTES })),
            ));
        }
        let pdf = match base64::engine::general_purpose::STANDARD.decode(part.bytes.as_bytes()) {
            Ok(pdf) => pdf,
            Err(error) => {
                return Dispatch::Reply(error_response(
                    id,
                    ErrorCode::Schema,
                    format!("PDF part {index}: not valid base64: {error}"),
                    Some(json!({ "pointer": format!("/pdfs/{index}/bytes") })),
                ));
            }
        };
        parts.push(BatchPart::Pdf(bytes::Bytes::from(pdf)));
    }
    let copies = params.copies.unwrap_or(1);
    let collate = params.collate.unwrap_or(true);
    let notify = job_notifier(state, session);
    let usage = usage_recorder(state, session);
    let worker = Arc::clone(state);
    let origin = session.origin.clone();
    let bound_origin = origin.clone();
    let title = "printkit pdf batch".to_owned();
    let result = run_reserve(id.clone(), move || {
        if let [BatchPart::Pdf(pdf)] = parts.as_slice() {
            printkit_facade::begin_print_pdf(
                printkit_facade::PrintPdf {
                    pdf: pdf.clone(),
                    printer: params.printer,
                    copies,
                    collate,
                    title,
                },
                &worker.pool,
                Arc::clone(&worker.print_backend),
                &worker.print_jobs,
                worker.sink.clone(),
                notify,
                bound_origin,
                usage,
            )
        } else {
            printkit_facade::begin_print_batch(
                printkit_facade::PrintBatch {
                    parts,
                    printer: params.printer,
                    copies,
                    collate,
                    title,
                },
                &worker.pool,
                Arc::clone(&worker.print_backend),
                Arc::clone(&worker.render_host),
                &worker.print_jobs,
                worker.sink.clone(),
                worker.fetch,
                notify,
                bound_origin,
                usage,
            )
        }
    })
    .await;
    let reserved = match result {
        Ok(reserved) => reserved,
        Err(dispatch) => return dispatch,
    };
    state
        .print_jobs
        .bind_origin(&reserved.job_id, origin.as_deref());
    remember_pages(state, reserved.job_id.as_str(), reserved.pages);
    // Metadata loss is reported once, on the job's done event; this response
    // stays free of duplicate warnings.
    let result = PdfBatchResult {
        job_id: reserved.job_id.as_str().to_owned(),
        pages: reserved.pages,
    };
    Dispatch::Start {
        response: ProtocolResponse {
            id,
            ok: true,
            result: serde_json::to_value(result).ok(),
            warnings: Vec::new(),
            error: None,
        },
        start: Box::new(move || reserved.activate()),
    }
}

async fn print_pdf(
    state: &Arc<AppState>,
    session: &Session,
    id: String,
    params: Value,
) -> Dispatch {
    let params: DocPrintPdfParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid doc.printPdf parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    if params.bytes.len() > limits::MAX_WS_BASE64_BYTES {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!(
                "the base64 PDF exceeds {} bytes; use POST /print/batch",
                limits::MAX_WS_BASE64_BYTES
            ),
            Some(json!({ "limit": limits::MAX_WS_BASE64_BYTES })),
        ));
    }
    let pdf = match base64::engine::general_purpose::STANDARD.decode(params.bytes.as_bytes()) {
        Ok(pdf) => pdf,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("the PDF is not valid base64: {error}"),
                None,
            ));
        }
    };
    if pdf.len() > limits::MAX_WS_PDF_BYTES {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!(
                "the PDF exceeds {} bytes; use POST /print/batch",
                limits::MAX_WS_PDF_BYTES
            ),
            Some(json!({ "limit": limits::MAX_WS_PDF_BYTES })),
        ));
    }
    let request = PrintPdf {
        pdf: bytes::Bytes::from(pdf),
        printer: params.printer,
        copies: params.copies.unwrap_or(1),
        collate: true,
        title: "printkit pdf".to_owned(),
    };
    let notify = job_notifier(state, session);
    let usage = usage_recorder(state, session);
    let worker = Arc::clone(state);
    let origin = session.origin.clone();
    let bound_origin = origin.clone();
    let result = run_reserve(id.clone(), move || {
        printkit_facade::begin_print_pdf(
            request,
            &worker.pool,
            Arc::clone(&worker.print_backend),
            &worker.print_jobs,
            worker.sink.clone(),
            notify,
            bound_origin,
            usage,
        )
    })
    .await;
    let reserved = match result {
        Ok(reserved) => reserved,
        Err(dispatch) => return dispatch,
    };
    state
        .print_jobs
        .bind_origin(&reserved.job_id, origin.as_deref());
    remember_pages(state, reserved.job_id.as_str(), reserved.pages);
    let job_id = reserved.job_id.as_str().to_owned();
    Dispatch::Start {
        response: ProtocolResponse {
            id,
            ok: true,
            result: Some(json!({ "jobId": job_id })),
            warnings: Vec::new(),
            error: None,
        },
        start: Box::new(move || reserved.activate()),
    }
}

/// `raw.send`: submit device bytes unchanged.
///
/// The `raw` scope was checked against the frozen method table before this
/// ran. Size limits are checked here as well, so an oversized payload never
/// reserves a queue slot.
async fn raw_send(state: &Arc<AppState>, session: &Session, id: String, params: Value) -> Dispatch {
    let params: RawSendParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("invalid raw.send parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            ));
        }
    };
    if params.bytes.len() > limits::MAX_WS_BASE64_BYTES {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!(
                "the base64 payload exceeds {} bytes",
                limits::MAX_WS_BASE64_BYTES
            ),
            Some(json!({ "limit": limits::MAX_WS_BASE64_BYTES })),
        ));
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(params.bytes.as_bytes()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::Schema,
                format!("the raw payload is not valid base64: {error}"),
                None,
            ));
        }
    };
    if bytes.len() > limits::MAX_RAW_BYTES {
        return Dispatch::Reply(error_response(
            id,
            ErrorCode::TooLarge,
            format!("the raw payload exceeds {} bytes", limits::MAX_RAW_BYTES),
            Some(json!({ "limit": limits::MAX_RAW_BYTES })),
        ));
    }
    let notify = job_notifier(state, session);
    let usage = usage_recorder(state, session);
    let request = RawSend {
        printer: Some(params.printer),
        bytes: bytes::Bytes::from(bytes),
        title: "printkit raw".to_owned(),
    };
    let worker = Arc::clone(state);
    let origin = session.origin.clone();
    let result = tokio::task::spawn_blocking(move || {
        printkit_facade::begin_raw_send(
            request,
            &worker.pool,
            Arc::clone(&worker.print_backend),
            &worker.print_jobs,
            worker.sink.clone(),
            notify,
            origin,
            usage,
        )
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return Dispatch::Reply(error_response(
                id,
                ErrorCode::JobFailed,
                format!("the raw send task failed: {error}"),
                None,
            ));
        }
    };
    match result {
        Ok(reserved) => {
            state
                .print_jobs
                .bind_origin(&reserved.job_id, session.origin.as_deref());
            let job_id = reserved.job_id.as_str().to_owned();
            Dispatch::Start {
                response: ProtocolResponse {
                    id,
                    ok: true,
                    result: Some(json!({ "jobId": job_id })),
                    warnings: Vec::new(),
                    error: None,
                },
                start: Box::new(move || reserved.activate()),
            }
        }
        Err(error) => Dispatch::Reply(error_response(
            id,
            engine_code(&error),
            error.to_string(),
            error.detail(),
        )),
    }
}

fn job_notifier(state: &AppState, session: &Session) -> JobUpdateHook {
    let hub = Arc::clone(&state.job_events);
    let sink = Arc::clone(&state.sink);
    let origin = session.origin.clone().unwrap_or_default();
    Box::new(move |job_id, update| send_job_event(&hub, sink.as_ref(), &origin, job_id, &update))
}

fn usage_recorder(state: &AppState, session: &Session) -> Option<UsageRecorder> {
    state
        .plugins
        .usage_recorder(&session.origin.clone().unwrap_or_default())
}

pub(crate) fn send_job_event(
    hub: &JobEventHub,
    sink: &dyn printkit_facade::Sink,
    origin: &str,
    job_id: &JobId,
    update: &JobUpdate,
) {
    let event = match update {
        JobUpdate::Queued { queue_pos } => JobEvent {
            job_id: job_id.as_str().to_owned(),
            status: JobState::Queued,
            queue_pos: Some(*queue_pos),
            progress: None,
            warnings: Vec::new(),
            error: None,
        },
        JobUpdate::Printing => JobEvent {
            job_id: job_id.as_str().to_owned(),
            status: JobState::Printing,
            queue_pos: None,
            progress: None,
            warnings: Vec::new(),
            error: None,
        },
        JobUpdate::Done { warnings } => JobEvent {
            job_id: job_id.as_str().to_owned(),
            status: JobState::Done,
            queue_pos: None,
            progress: None,
            warnings: warnings
                .iter()
                .filter_map(|warning| warning_body_with_sink(sink, warning))
                .collect(),
            error: None,
        },
        JobUpdate::Cancelled => JobEvent {
            job_id: job_id.as_str().to_owned(),
            status: JobState::Cancelled,
            queue_pos: None,
            progress: None,
            warnings: Vec::new(),
            error: None,
        },
        JobUpdate::Failed { failure } => JobEvent {
            job_id: job_id.as_str().to_owned(),
            status: JobState::Failed,
            queue_pos: None,
            progress: None,
            warnings: Vec::new(),
            error: Some(ErrorBody {
                code: error_code(failure.code),
                message: failure.message.clone(),
                detail: failure.detail.clone(),
            }),
        },
    };
    let Ok(mut value) = serde_json::to_value(event) else {
        return;
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_owned(), json!("event"));
        object.insert("event".to_owned(), json!("job"));
    }
    if let Ok(text) = serde_json::to_string(&value) {
        hub.publish(origin, text);
    }
}

/// How long a signed export link stays valid.
const EXPORT_URL_TTL_SECS: u64 = 600;

/// Percent-encode one relative export path for a URL query-adjacent segment.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

async fn export_pdf(state: &Arc<AppState>, id: String, params: Value) -> ProtocolResponse {
    let params: ExportPdfParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                ErrorCode::Schema,
                format!("invalid doc.exportPdf parameters: {error}"),
                Some(json!({ "message": error.to_string() })),
            );
        }
    };
    let document = match serde_json::to_vec(&params.document) {
        Ok(document) => document,
        Err(error) => {
            return error_response(
                id,
                ErrorCode::Schema,
                format!("cannot encode the document: {error}"),
                None,
            );
        }
    };
    let data_dir = state.data_dir.clone();
    let pool = Arc::clone(&state.pool);
    let host = Arc::clone(&state.render_host);
    let sink = Arc::clone(&state.sink);
    let fetch = state.fetch;
    let ttl_hours = state.export_ttl_hours;
    let Some(export_secret) = state.export_secret else {
        return error_response(
            id,
            ErrorCode::JobFailed,
            "export link signing is unavailable".to_owned(),
            None,
        );
    };
    let base = state
        .admin_origins
        .first()
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1".to_owned());
    let path = params.path;
    // Parsing, layout and the blocking pool wait are CPU work: keep them off
    // the async worker threads.
    let result = tokio::task::spawn_blocking(move || {
        super::observability::purge_exports(&data_dir.join("exports"), ttl_hours);
        printkit_facade::export_pdf_to_path(
            &document,
            &data_dir,
            &path,
            &pool,
            host.as_ref(),
            sink,
            &fetch,
        )
    })
    .await;
    match result {
        Ok(Ok(exported)) => {
            // The browser cannot read `file://`; hand it a capability URL bound
            // to this canonical file and a short expiry instead of an open
            // unauthenticated `/exports` tree.
            let expires_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or_default()
                + EXPORT_URL_TTL_SECS;
            let signature =
                super::exports::sign_export(&export_secret, &exported.absolute_path, expires_at);
            let rest = exported
                .path
                .strip_prefix("exports/")
                .unwrap_or(&exported.path);
            let url = format!(
                "{base}/exports/{}?exp={expires_at}&sig={signature}",
                encode_path(rest)
            );
            ProtocolResponse {
                id,
                ok: true,
                result: serde_json::to_value(ExportPdfResult {
                    path: exported.path,
                    absolute_path: exported.absolute_path,
                    pages: exported.pages,
                    bytes: exported.bytes,
                    url: Some(url),
                    expires_at: Some(expires_at),
                })
                .ok(),
                warnings: exported
                    .warnings
                    .iter()
                    .filter_map(|warning| warning_body(state.as_ref(), warning))
                    .collect(),
                error: None,
            }
        }
        Ok(Err(error)) => {
            error_response(id, engine_code(&error), error.to_string(), error.detail())
        }
        Err(error) => error_response(
            id,
            ErrorCode::JobFailed,
            format!("the export task failed: {error}"),
            None,
        ),
    }
}

/// Map an engine warning to its wire code.
/// An unknown code is dropped and logged rather than sent unlisted: clients
/// switch on `WarningCode`, and engine and protocol ship from the same build.
fn warning_body(state: &AppState, warning: &Warning) -> Option<WarningBody> {
    warning_body_with_sink(state.sink.as_ref(), warning)
}

fn warning_body_with_sink(
    sink: &dyn printkit_facade::Sink,
    warning: &Warning,
) -> Option<WarningBody> {
    let mapped = WarningCode::ALL.iter().find_map(|code| {
        (code.as_str() == warning.code).then_some(WarningBody {
            code: *code,
            message: warning.message.clone(),
            detail: None,
        })
    });
    if mapped.is_none() {
        // The client cannot represent an unknown code, so at least keep it in
        // the trace stream instead of only on stderr.
        Tracer::new(sink, TraceId::new())
            .event(Stage::Render)
            .detail(format!(
                "dropped warning {}: {}",
                warning.code, warning.message
            ))
            .emit();
        eprintln!(
            "printkit: dropping job warning with unknown code {}",
            warning.code
        );
    }
    mapped
}

/// Run one engine reserve/validate call on the blocking pool. Preparation
/// (URL fetch, layout, WebView measurement, PDF page counting) must never hold
/// a Tokio worker; the returned job is still activated only after the response.
#[allow(clippy::result_large_err)]
async fn run_reserve<T, F>(id: String, task: F) -> Result<T, Dispatch>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, EngineError> + Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(Dispatch::Reply(error_response(
            id,
            engine_code(&error),
            error.to_string(),
            error.detail(),
        ))),
        Err(error) => Err(Dispatch::Reply(error_response(
            id,
            ErrorCode::JobFailed,
            format!("the reserve task failed: {error}"),
            None,
        ))),
    }
}

fn engine_code(error: &EngineError) -> ErrorCode {
    error_code(error.code())
}

fn error_code(code: &str) -> ErrorCode {
    ErrorCode::ALL
        .iter()
        .copied()
        .find(|known| known.as_str() == code)
        .unwrap_or(ErrorCode::JobFailed)
}

fn error_response(
    id: String,
    code: ErrorCode,
    message: String,
    detail: Option<Value>,
) -> ProtocolResponse {
    ProtocolResponse {
        id,
        ok: false,
        result: None,
        warnings: Vec::new(),
        error: Some(ErrorBody {
            code,
            message,
            detail,
        }),
    }
}

/// Held for the lifetime of one authenticated connection.
///
/// Both counters live outside the rest of the state so the acquire/release
/// rule can be tested without building a whole service.
struct ConnectionSlot {
    connections: Arc<AtomicUsize>,
    origins: Arc<Mutex<HashMap<String, usize>>>,
    key: Option<String>,
}

impl ConnectionSlot {
    fn acquire(
        connections: &Arc<AtomicUsize>,
        origins: &Arc<Mutex<HashMap<String, usize>>>,
        key: Option<&str>,
    ) -> Option<Self> {
        let previous = connections.fetch_add(1, Ordering::SeqCst);
        if previous >= MAX_CONNECTIONS {
            connections.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        if let Some(key) = key {
            let mut map = origins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let count = map.entry(key.to_owned()).or_insert(0);
            if *count >= MAX_CONNECTIONS_PER_ORIGIN {
                connections.fetch_sub(1, Ordering::SeqCst);
                return None;
            }
            *count += 1;
        }
        Some(Self {
            connections: Arc::clone(connections),
            origins: Arc::clone(origins),
            key: key.map(str::to_owned),
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::SeqCst);
        let Some(key) = self.key.as_deref() else {
            return;
        };
        let mut map = self
            .origins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = map.get_mut(key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_method_table_and_engine_table_agree() {
        let sdk = include_str!("../../../../packages/sdk-ts/src/methods.ts");
        let sdk_methods: Vec<&str> = sdk
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix('"')?;
                let (name, _) = rest.split_once('"')?;
                Some(name)
            })
            .collect();
        let engine: Vec<&str> = METHODS.iter().map(|descriptor| descriptor.method).collect();
        assert_eq!(sdk_methods, engine);
    }

    #[test]
    fn an_unknown_warning_reaches_the_trace_sink() {
        let sink = printkit_facade::MemorySink::default();
        let warning = Warning {
            code: "W_FUTURE".to_owned(),
            message: "from a newer build".to_owned(),
        };
        assert!(warning_body_with_sink(&sink, &warning).is_none());
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, Stage::Render);
        assert!(
            events[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("W_FUTURE")),
            "{:?}",
            events[0].detail
        );
    }

    #[test]
    fn request_ids_are_bounded_printable_ascii() {
        assert!(valid_request_id("job-1"));
        assert!(valid_request_id(&"a".repeat(MAX_REQUEST_ID_BYTES)));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id(&"a".repeat(MAX_REQUEST_ID_BYTES + 1)));
        assert!(!valid_request_id("has space"));
        assert!(!valid_request_id("line\nbreak"));
        assert!(!valid_request_id("caf\u{e9}"));
    }

    #[test]
    fn only_a_started_job_response_is_remembered() {
        let cache = std::sync::Mutex::new(crate::service::IdempotencyCache::new());
        let key = Some("ws|doc.print|local|dup".to_owned());
        let failed = error_response(
            "1".to_owned(),
            ErrorCode::Schema,
            "bad document".to_owned(),
            None,
        );
        remember_idempotent(&cache, key.clone(), &failed);
        assert!(
            cache
                .lock()
                .unwrap()
                .get("ws|doc.print|local|dup")
                .is_none(),
            "an error response must not be cached"
        );

        let started = ProtocolResponse {
            id: "2".to_owned(),
            ok: true,
            result: Some(json!({ "jobId": "job-1", "docCount": 2, "pageCount": 3 })),
            warnings: Vec::new(),
            error: None,
        };
        remember_idempotent(&cache, key, &started);
        let entry = cache
            .lock()
            .unwrap()
            .get("ws|doc.print|local|dup")
            .expect("the started job is cached");
        assert_eq!(entry.job_id, "job-1");
        assert_eq!(entry.doc_count, 2);
        assert_eq!(entry.page_count, 3);
    }

    #[test]
    fn welcome_never_sends_the_removed_template_capability() {
        let session = Session {
            connection: 1,
            scopes: vec![Scope::Print],
            origin: Some("https://app.example".to_owned()),
        };
        let families = vec!["observability".to_owned(), "records".to_owned()];
        let value = welcome_with(&session, &families);
        assert_eq!(value["capabilities"], json!([]));
        assert_eq!(value["pluginFamilies"], json!(["observability", "records"]));
    }

    #[test]
    fn the_connection_slot_never_leaks_a_refused_or_dropped_slot() {
        let connections = Arc::new(AtomicUsize::new(0));
        let origins = Arc::new(Mutex::new(HashMap::new()));
        let mut slots = Vec::new();
        for index in 0..MAX_CONNECTIONS {
            let key = format!("origin-{index}");
            slots.push(
                ConnectionSlot::acquire(&connections, &origins, Some(key.as_str())).expect("slot"),
            );
        }
        assert_eq!(connections.load(Ordering::SeqCst), MAX_CONNECTIONS);
        assert!(ConnectionSlot::acquire(&connections, &origins, Some("late")).is_none());
        assert_eq!(
            connections.load(Ordering::SeqCst),
            MAX_CONNECTIONS,
            "a refused slot was not rolled back"
        );
        slots.pop();
        assert_eq!(connections.load(Ordering::SeqCst), MAX_CONNECTIONS - 1);
        let replacement =
            ConnectionSlot::acquire(&connections, &origins, Some("late")).expect("freed slot");
        drop(replacement);
        drop(slots);
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn one_origin_cannot_take_every_slot() {
        let connections = Arc::new(AtomicUsize::new(0));
        let origins = Arc::new(Mutex::new(HashMap::new()));
        let mut first = Vec::new();
        for _ in 0..MAX_CONNECTIONS_PER_ORIGIN {
            first.push(ConnectionSlot::acquire(&connections, &origins, Some("one")).expect("slot"));
        }
        assert!(ConnectionSlot::acquire(&connections, &origins, Some("one")).is_none());
        let other =
            ConnectionSlot::acquire(&connections, &origins, Some("two")).expect("other origin");
        drop(other);
        drop(first);
        assert_eq!(connections.load(Ordering::SeqCst), 0);
        assert!(origins.lock().unwrap().is_empty());
    }
}
