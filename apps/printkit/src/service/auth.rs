//! Handshake authorisation: origin whitelist, token lookup and scope binding.
//!
//! The origin header and the token are two independent factors. A browser
//! token only works from the exact origin it was issued for, and a script
//! token only works without an `Origin` header. A handshake without a token
//! prints from the local machine or from a whitelisted origin, but never
//! reaches `raw` or `admin`; those scopes need an issued token. Every refusal
//! writes one `auth` trace event, so `printkit logs --trace <id>` explains a
//! failed handshake without reading server output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use printkit_facade::{Stage, TraceId, Tracer};
use printkit_protocol::{ErrorCode, Hello, PROTOCOL_VERSION, Scope};

use super::AppState;
use super::admin::canonical_origin;
use super::lock::{constant_time_eq, hash_token};

/// A validated connection.
///
/// `connection` is the key a preview window belongs to: one client replaces
/// its own window instead of accumulating one per document.
#[derive(Debug, Clone)]
pub struct Session {
    pub connection: u64,
    pub scopes: Vec<Scope>,
    /// Origin the token is bound to. `records.*` forces it as a filter for a
    /// `print` token; an `admin` token ignores it.
    pub origin: Option<String>,
}

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

impl Session {
    /// `admin` covers every scope; a `print` token never covers `raw`.
    pub fn has(&self, required: Scope) -> bool {
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&required)
    }
}

/// A refused handshake, already traced. The trace id is handed to the client
/// so it can point at the log line it just caused.
#[derive(Debug, Clone)]
pub struct Rejection {
    pub code: ErrorCode,
    pub message: String,
    pub trace_id: TraceId,
}

impl Rejection {
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({ "traceId": self.trace_id.as_str() })
    }
}

/// How a handshake without a token is treated.
///
/// Production is always [`LocalPrintPolicy::Open`]: a local script or a
/// whitelisted browser prints without a token and only receives the `print`
/// scope. [`LocalPrintPolicy::Token`] exists so tests keep covering the
/// refusal path; it is never read from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalPrintPolicy {
    Open,
    #[cfg_attr(not(test), allow(dead_code))]
    Token,
}

/// Validate a `hello` against the transport's `Origin` header.
pub async fn authenticate(
    state: &AppState,
    hello: &Hello,
    origin_header: Option<&str>,
) -> Result<Session, Rejection> {
    authenticate_with(state, hello, origin_header, LocalPrintPolicy::Open).await
}

pub(crate) async fn authenticate_with(
    state: &AppState,
    hello: &Hello,
    origin_header: Option<&str>,
    policy: LocalPrintPolicy,
) -> Result<Session, Rejection> {
    let trace_id = TraceId::new();
    let started = Instant::now();
    let reported_origin = origin_header.or(hello.origin.as_deref());
    let reject = |code: ErrorCode, message: String| {
        let tracer = Tracer::new(state.sink.as_ref(), trace_id.clone());
        let mut event = tracer
            .event(Stage::Auth)
            .failed(code.as_str())
            .dur_ms(started.elapsed())
            .detail(message.clone());
        if let Some(origin) = reported_origin {
            event = event.origin(origin.to_owned());
        }
        event.emit();
        Rejection {
            code,
            message,
            trace_id: trace_id.clone(),
        }
    };

    if hello.protocol != PROTOCOL_VERSION {
        return Err(reject(
            ErrorCode::Protocol,
            format!(
                "protocol {} is not supported; this build speaks {PROTOCOL_VERSION}",
                hello.protocol
            ),
        ));
    }

    let origins = match store_call({
        let store = state.store.clone();
        move || store.origins()
    })
    .await
    {
        Ok(origins) => origins,
        Err(error) => {
            return Err(reject(
                ErrorCode::JobFailed,
                format!("cannot read the origin whitelist: {error}"),
            ));
        }
    };

    // The whitelist is checked before the token: an unpaired page must not
    // learn whether it guessed a valid token.
    if let Some(origin) = origin_header
        && !origins
            .iter()
            .any(|allowed| canonical_origin(allowed).is_ok_and(|canonical| canonical == origin))
    {
        return Err(reject(
            ErrorCode::Origin,
            format!("origin {origin} is not whitelisted"),
        ));
    }

    let Some(presented) = hello.token.as_deref().filter(|token| !token.is_empty()) else {
        if matches!(policy, LocalPrintPolicy::Token) {
            return Err(reject(
                ErrorCode::Auth,
                "the handshake carries no token".to_owned(),
            ));
        }
        // No token: a local script (no Origin) or a whitelisted page. The
        // whitelist was checked above, so this session only prints; `raw` and
        // `admin` always need a token issued by an administrator.
        return Ok(accept(
            state,
            trace_id,
            started,
            origin_header,
            vec![Scope::Print],
            origin_header.map(str::to_owned),
        ));
    };

    let digest = hash_token(presented);
    let token = match store_call({
        let store = state.store.clone();
        move || store.token_by_hash(&digest)
    })
    .await
    {
        Ok(Some(token)) => token,
        Ok(None) => {
            return Err(reject(
                ErrorCode::Auth,
                "the token is unknown or was revoked".to_owned(),
            ));
        }
        Err(error) => {
            return Err(reject(
                ErrorCode::JobFailed,
                format!("cannot read authorisations: {error}"),
            ));
        }
    };

    if token.non_browser {
        if origin_header.is_some() {
            return Err(reject(
                ErrorCode::Origin,
                "a script token cannot be used from a browser".to_owned(),
            ));
        }
    } else {
        let Some(header) = origin_header else {
            return Err(reject(
                ErrorCode::Origin,
                "a browser token requires an Origin header".to_owned(),
            ));
        };
        if let Some(reported) = hello.origin.as_deref()
            && !constant_time_eq(reported, header)
        {
            return Err(reject(
                ErrorCode::Origin,
                "the reported origin differs from the transport origin".to_owned(),
            ));
        }
        let bound = canonical_origin(&token.origin).unwrap_or_else(|_| token.origin.clone());
        if !constant_time_eq(&bound, header) {
            return Err(reject(
                ErrorCode::Origin,
                format!("the token is bound to {}, not to {header}", token.origin),
            ));
        }
    }

    // Best effort: a failed timestamp update must not reject a valid token.
    let store = state.store.clone();
    let id = token.id;
    let _ = tokio::task::spawn_blocking(move || store.touch_token(id)).await;

    Ok(accept(
        state,
        trace_id,
        started,
        origin_header,
        parse_scopes(&token.scope),
        Some(token.origin),
    ))
}

/// Record one successful handshake and mint the session it grants.
fn accept(
    state: &AppState,
    trace_id: TraceId,
    started: Instant,
    origin_header: Option<&str>,
    scopes: Vec<Scope>,
    session_origin: Option<String>,
) -> Session {
    let tracer = Tracer::new(state.sink.as_ref(), trace_id);
    let mut event = tracer.event(Stage::Auth).dur_ms(started.elapsed());
    if let Some(origin) = origin_header {
        event = event.origin(origin.to_owned());
    }
    event.emit();

    Session {
        connection: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
        scopes,
        origin: session_origin,
    }
}

fn parse_scopes(value: &str) -> Vec<Scope> {
    value
        .split(',')
        .filter_map(|name| match name.trim() {
            "print" => Some(Scope::Print),
            "raw" => Some(Scope::Raw),
            "admin" => Some(Scope::Admin),
            _ => None,
        })
        .collect()
}

pub fn scope_names(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|scope| match scope {
            Scope::Print => "print",
            Scope::Raw => "raw",
            Scope::Admin => "admin",
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Run a blocking SQLite call off the async workers.
async fn store_call<T, F>(call: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, printkit_store::StoreError> + Send + 'static,
{
    match tokio::task::spawn_blocking(call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("store task failed: {error}")),
    }
}
