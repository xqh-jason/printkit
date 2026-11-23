//! `/admin`: administrator login, origin pairing, token issuance and shutdown.
//!
//! Every state-changing request needs three things: an `Origin` naming this
//! service, an administrator session cookie, and a CSRF token that appears
//! both as a cookie and in the `X-CSRF-Token` header. The admin token itself
//! is compared in constant time and never leaves the data directory. Write
//! bodies accept up to `MAX_ADMIN_BODY_BYTES` (one document plus its envelope),
//! not Salvo's 64 KiB default.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use printkit_protocol::{ErrorCode, Scope};
use salvo::http::cookie::{Cookie, SameSite};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use super::AppState;
use super::auth::scope_names;
use super::lock::{
    CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, constant_time_eq, new_access_token, random_secret,
};

const SESSION_HOURS: i64 = 8;

/// Administrator sessions: one random cookie value maps to an expiry time.
#[derive(Debug)]
pub struct Sessions {
    entries: Mutex<HashMap<String, OffsetDateTime>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn create(&self) -> Option<String> {
        let token = random_secret().ok()?;
        let now = OffsetDateTime::now_utc();
        let mut entries = self.entries.lock().ok()?;
        entries.retain(|_, expiry| *expiry > now);
        entries.insert(token.clone(), now + Duration::hours(SESSION_HOURS));
        Some(token)
    }

    fn valid(&self, token: &str) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        entries
            .get(token)
            .is_some_and(|expiry| *expiry > OffsetDateTime::now_utc())
    }

    /// Remove one session. Returns whether it was still live.
    fn remove(&self, token: &str) -> bool {
        self.entries
            .lock()
            .ok()
            .and_then(|mut entries| entries.remove(token))
            .is_some_and(|expiry| expiry > OffsetDateTime::now_utc())
    }
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    token: String,
}

#[derive(Debug, Deserialize)]
struct OriginBody {
    origin: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenBody {
    origin: String,
    #[serde(default)]
    raw: bool,
    /// Grants the `admin` scope. Only an authenticated administrator can
    /// request it, so this is issuance rather than a `print` upgrade.
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    non_browser: bool,
}

#[handler]
pub async fn session(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if ensure_csrf(req, res).is_err() {
        return internal_error(res, "cannot generate a CSRF token");
    }
    ok(res, json!({ "authenticated": admin_session(&state, req) }));
}

#[handler]
pub async fn login(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, false) {
        return deny(res, status);
    }
    req.set_secure_max_size(printkit_facade::limits::MAX_ADMIN_BODY_BYTES);
    let body: LoginBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Protocol,
                format!("invalid login body: {error}"),
            );
        }
    };
    if !login_allowed(&state) {
        return respond(
            res,
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": { "code": "E_AUTH", "message": "too many failed logins; wait a minute" } })
                .to_string(),
        );
    }
    if !constant_time_eq(&body.token, &state.admin_token) {
        record_login_failure(&state);
        return deny(res, WriteDenied::Session);
    }
    reset_login_failures(&state);
    let Some(session_id) = state.sessions.create() else {
        return internal_error(res, "cannot create a session");
    };
    // `/records/*` live outside `/admin`; the administrator cookie reaches them.
    res.add_cookie(
        Cookie::build((SESSION_COOKIE, session_id))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Strict)
            .max_age(Duration::hours(SESSION_HOURS))
            .build(),
    );
    ok(res, json!({ "ok": true }));
}

#[handler]
pub async fn logout(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    if let Some(cookie) = req.cookie(SESSION_COOKIE) {
        state.sessions.remove(cookie.value());
    }
    res.add_cookie(
        Cookie::build((SESSION_COOKIE, ""))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Strict)
            .max_age(Duration::seconds(0))
            .build(),
    );
    ok(res, json!({ "ok": true }));
}

#[handler]
pub async fn list_origins(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if !admin_session(&state, req) {
        return deny(res, WriteDenied::Session);
    }
    match read_origins(&state).await {
        Ok(origins) => ok(res, json!({ "origins": origins })),
        Err(()) => internal_error(res, "cannot read the origin whitelist"),
    }
}

#[handler]
pub async fn add_origin(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    req.set_secure_max_size(printkit_facade::limits::MAX_ADMIN_BODY_BYTES);
    let body: OriginBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Protocol,
                format!("invalid origin body: {error}"),
            );
        }
    };
    let origin = match canonical_origin(body.origin.trim()) {
        Ok(origin) => origin,
        Err(message) => return bad_request(res, ErrorCode::Origin, message),
    };
    if state.admin_origins.iter().any(|own| own == &origin) {
        return bad_request(
            res,
            ErrorCode::Origin,
            "the administrator origin cannot be a business origin",
        );
    }
    let store = state.store.clone();
    let added = origin.clone();
    let result = tokio::task::spawn_blocking(move || store.add_origin(&origin)).await;
    match result {
        Ok(Ok(origins)) => {
            if let Ok(mut revoked) = state.revoked_origins.lock() {
                revoked.remove(&added);
            }
            ok(res, json!({ "origins": origins }))
        }
        _ => internal_error(res, "cannot write the origin whitelist"),
    }
}

/// `DELETE /admin/origins`: remove one origin and return the remaining list.
///
/// Removing an origin that is not listed succeeds with the unchanged list, so
/// a retried CLI command is safe. The administrator origins are not part of
/// the whitelist and cannot be removed here.
#[handler]
pub async fn remove_origin(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    req.set_secure_max_size(printkit_facade::limits::MAX_ADMIN_BODY_BYTES);
    let body: OriginBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Protocol,
                format!("invalid origin body: {error}"),
            );
        }
    };
    let origin = match canonical_origin(body.origin.trim()) {
        Ok(origin) => origin,
        Err(message) => return bad_request(res, ErrorCode::Origin, message),
    };
    let store = state.store.clone();
    let revoke = Arc::clone(&store);
    let removed = origin.clone();
    let result = tokio::task::spawn_blocking(move || {
        let origins = store.remove_origin(&removed)?;
        revoke.revoke_origin(&removed)?;
        Ok::<_, printkit_store::StoreError>(origins)
    })
    .await;
    match result {
        Ok(Ok(origins)) => {
            if let Ok(mut revoked) = state.revoked_origins.lock() {
                revoked.insert(origin);
            }
            ok(res, json!({ "origins": origins }))
        }
        _ => internal_error(res, "cannot write the origin whitelist"),
    }
}

#[handler]
pub async fn issue_token(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    req.set_secure_max_size(printkit_facade::limits::MAX_ADMIN_BODY_BYTES);
    let body: TokenBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Protocol,
                format!("invalid token body: {error}"),
            );
        }
    };
    let origin = match canonical_origin(body.origin.trim()) {
        Ok(origin) => origin,
        Err(message) => return bad_request(res, ErrorCode::Origin, message),
    };
    let allowed = match read_origins(&state).await {
        Ok(origins) => origins,
        Err(()) => return internal_error(res, "cannot read the origin whitelist"),
    };
    if !allowed.iter().any(|candidate| candidate == &origin) {
        return bad_request(
            res,
            ErrorCode::Origin,
            "the origin must be whitelisted before a token is issued",
        );
    }
    let mut scopes = vec![Scope::Print];
    if body.raw {
        scopes.push(Scope::Raw);
    }
    if body.admin {
        scopes.push(Scope::Admin);
    }
    let scope_list = scope_names(&scopes);
    let granted: Vec<String> = scope_list
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    let token = match new_access_token() {
        Ok(token) => token,
        Err(error) => return internal_error(res, &format!("cannot generate a token: {error}")),
    };
    let digest = super::lock::hash_token(&token);
    let store = state.store.clone();
    let origin_for_store = origin.clone();
    let task = tokio::task::spawn_blocking(move || {
        store.insert_token(&origin_for_store, &digest, &scope_list, body.non_browser)
    })
    .await;
    match task {
        Ok(Ok(id)) => {
            respond(
                res,
                StatusCode::CREATED,
                json!({
                    "token": token,
                    "id": id,
                    "origin": origin,
                    "scopes": granted,
                    "nonBrowser": body.non_browser,
                })
                .to_string(),
            );
        }
        _ => internal_error(res, "cannot store the token"),
    }
}

/// Revoke one issued token. The row keeps its audit data; handshakes stop
/// accepting the hash immediately.
#[handler]
pub async fn revoke_token(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    let Some(id) = req
        .param::<String>("tokenId")
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return respond(
            res,
            StatusCode::BAD_REQUEST,
            json!({ "error": { "code": "E_SCHEMA", "message": "tokenId must be an integer" } })
                .to_string(),
        );
    };
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.revoke_token(id)).await {
        Ok(Ok(true)) => ok(res, json!({ "id": id, "revoked": true })),
        Ok(Ok(false)) => respond(
            res,
            StatusCode::NOT_FOUND,
            json!({ "error": { "code": "E_NOT_FOUND", "message": "unknown or already revoked token" } })
                .to_string(),
        ),
        _ => internal_error(res, "cannot revoke the token"),
    }
}

/// Open one preview window in the running service.
///
/// This is the local control-plane path used by `printkit preview` when
/// another process already owns the data directory. The WebSocket
/// `doc.preview` method remains the client-facing path; both reach the same
/// registry and the same print callback.
#[handler]
pub async fn preview(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    // A valid document may be far larger than Salvo's 64 KiB default body cap.
    req.set_secure_max_size(printkit_facade::limits::MAX_DOCUMENT_BYTES);
    let document: Value = match req.parse_json().await {
        Ok(document) => document,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Schema,
                format!("invalid document body: {error}"),
            );
        }
    };
    let bytes = match serde_json::to_vec(&document) {
        Ok(bytes) => bytes,
        Err(error) => {
            return bad_request(
                res,
                ErrorCode::Schema,
                format!("cannot encode the document: {error}"),
            );
        }
    };
    // Each command invocation is its own preview owner: a fresh connection id
    // means the ninth window hits the process limit instead of replacing one.
    let origin = state.admin_origins.first().cloned().unwrap_or_default();
    let preview_session = super::preview::internal_session(origin);
    let on_print =
        super::ws::preview_print_action(&state, &preview_session, Arc::new(bytes.clone()));
    match state
        .previews
        .open(&state, &preview_session, bytes, on_print)
        .await
    {
        Ok(opened) => {
            let warnings: Vec<Value> = opened
                .warnings
                .iter()
                .map(|warning| json!({ "code": warning.code, "message": warning.message }))
                .collect();
            ok(
                res,
                json!({
                    "windowId": opened.window.get().to_string(),
                    "warnings": warnings,
                }),
            )
        }
        Err(error) => {
            let status = preview_status(&error);
            let body = json!({
                "error": {
                    "code": error.code().as_str(),
                    "message": error.message(),
                    "detail": error.detail(),
                }
            });
            respond(res, status, body.to_string());
        }
    }
}

fn preview_status(error: &super::preview::PreviewError) -> StatusCode {
    match error.code() {
        ErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Schema => StatusCode::BAD_REQUEST,
        ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[handler]
pub async fn shutdown(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    let (drained, inflight) = super::drain_jobs(&state, StdDuration::from_secs(2)).await;
    respond(
        res,
        StatusCode::ACCEPTED,
        json!({ "ok": true, "drained": drained, "inflight": inflight }).to_string(),
    );
    // Let the response leave the socket before the listener starts draining.
    let handle = state.server.lock().ok().and_then(|slot| slot.clone());
    if let Some(handle) = handle {
        tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_millis(50)).await;
            handle.stop_graceful(Some(StdDuration::from_secs(2)));
        });
    }
}

fn obtain(depot: &Depot) -> Option<Arc<AppState>> {
    depot.obtain::<Arc<AppState>>().ok().cloned()
}

pub(crate) fn admin_session(state: &AppState, req: &Request) -> bool {
    req.cookie(SESSION_COOKIE)
        .is_some_and(|cookie| state.sessions.valid(cookie.value()))
}

fn ensure_csrf(req: &Request, res: &mut Response) -> Result<(), ()> {
    if req.cookie(CSRF_COOKIE).is_some() {
        return Ok(());
    }
    let token = random_secret().map_err(|_| ())?;
    res.add_cookie(
        // Records endpoints live outside `/admin` on the same origin, and the
        // desktop control client sends shutdown from whichever page is open.
        // A path-scoped cookie would be invisible there.
        Cookie::build((CSRF_COOKIE, token))
            .path("/")
            .same_site(SameSite::Strict)
            .build(),
    );
    Ok(())
}

/// Origin, session and CSRF, in that order. Reads pass `require_session` with
/// CSRF disabled; the session is still checked.
/// Why a state-changing request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteDenied {
    Session,
    Origin,
    Csrf,
}

impl WriteDenied {
    fn status(self) -> StatusCode {
        match self {
            Self::Session => StatusCode::UNAUTHORIZED,
            Self::Origin | Self::Csrf => StatusCode::FORBIDDEN,
        }
    }

    fn code(self) -> ErrorCode {
        match self {
            Self::Session => ErrorCode::Auth,
            Self::Origin => ErrorCode::Origin,
            Self::Csrf => ErrorCode::Csrf,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Session => "an administrator session is required",
            Self::Origin => "the request origin is not accepted",
            Self::Csrf => "the CSRF token is missing or invalid",
        }
    }
}
pub(crate) fn check_write(
    state: &AppState,
    req: &Request,
    require_session: bool,
) -> Result<(), WriteDenied> {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|value| value.to_str().ok());
    if !origin.is_some_and(|origin| state.admin_origins.iter().any(|own| own == origin)) {
        return Err(WriteDenied::Origin);
    }
    if require_session && !admin_session(state, req) {
        return Err(WriteDenied::Session);
    }
    let cookie = req.cookie(CSRF_COOKIE).map(Cookie::value);
    let header = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok());
    match (cookie, header) {
        (Some(cookie), Some(header)) if constant_time_eq(cookie, header) => Ok(()),
        _ => Err(WriteDenied::Csrf),
    }
}

async fn read_origins(state: &AppState) -> Result<Vec<String>, ()> {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.origins()).await {
        Ok(Ok(origins)) => Ok(origins
            .into_iter()
            .map(|origin| canonical_origin(&origin).unwrap_or(origin))
            .collect()),
        _ => Err(()),
    }
}

/// `scheme://host[:port]` only: no path, query, fragment or wildcard. An
/// IPv6 host is written in brackets, e.g. `http://[::1]:5173`.
/// `scheme://host[:port]` only: no path, query, fragment or wildcard. The
/// result is the RFC 6454 serialization (lower-case scheme/host and no
/// default port) so a stored allowlist entry matches what a browser sends.
pub(crate) fn canonical_origin(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err("an origin must be a single non-empty URL");
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return Err("an origin must start with http:// or https://");
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("an origin scheme must be http or https");
    }
    if rest.contains(['/', '?', '#', '@']) {
        return Err("an origin must not carry a path, query, fragment or credentials");
    }
    let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        let Some((host, tail)) = bracketed.split_once(']') else {
            return Err("an IPv6 host must close its bracket");
        };
        if !host.contains(':')
            || !host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err("a bracketed host must be an IPv6 literal");
        }
        let port = match tail {
            "" => None,
            _ => match tail.strip_prefix(':') {
                Some(port) => Some(port),
                None => {
                    return Err("an origin must not carry a path, query, fragment or credentials");
                }
            },
        };
        (format!("[{}]", host.to_ascii_lowercase()), port)
    } else {
        let (host, port) = match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        };
        if host.is_empty() || host == "*" {
            return Err("an origin needs a concrete host");
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        Some(text) => {
            if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
                return Err("an origin port must be numeric");
            }
            text.parse::<u16>()
                .map_err(|_| "an origin port must fit in 0..=65535")?
        }
        None => return Ok(format!("{scheme}://{host}")),
    };
    if (scheme == "http" && port == 80) || (scheme == "https" && port == 443) {
        Ok(format!("{scheme}://{host}"))
    } else {
        Ok(format!("{scheme}://{host}:{port}"))
    }
}

const LOGIN_WINDOW: StdDuration = StdDuration::from_secs(60);
const LOGIN_FAILURE_LIMIT: u32 = 5;

fn login_allowed(state: &AppState) -> bool {
    let mut guard = state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.1.elapsed() > LOGIN_WINDOW {
        guard.0 = 0;
        guard.1 = Instant::now();
    }
    guard.0 < LOGIN_FAILURE_LIMIT
}

fn record_login_failure(state: &AppState) {
    let mut guard = state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.1.elapsed() > LOGIN_WINDOW {
        guard.0 = 0;
        guard.1 = Instant::now();
    }
    guard.0 = guard.0.saturating_add(1);
}

fn reset_login_failures(state: &AppState) {
    let mut guard = state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.0 = 0;
    guard.1 = Instant::now();
}

fn respond(res: &mut Response, status: StatusCode, body: String) {
    res.status_code(status);
    res.render(Text::Json(body));
}

fn ok(res: &mut Response, body: Value) {
    respond(res, StatusCode::OK, body.to_string());
}

fn bad_request(res: &mut Response, code: ErrorCode, message: impl AsRef<str>) {
    respond(
        res,
        StatusCode::BAD_REQUEST,
        json!({ "ok": false, "error": { "code": code.as_str(), "message": message.as_ref() } })
            .to_string(),
    );
}

pub(crate) fn deny(res: &mut Response, denial: WriteDenied) {
    let code = denial.code();
    let message = denial.message();
    respond(
        res,
        denial.status(),
        json!({ "ok": false, "error": { "code": code.as_str(), "message": message } }).to_string(),
    );
}

fn internal_error(res: &mut Response, message: &str) {
    respond(
        res,
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({
            "ok": false,
            "error": { "code": "E_JOB_FAILED", "message": message }
        })
        .to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::{Sessions, canonical_origin};

    #[test]
    fn origins_are_canonicalized_to_the_browser_serialization() {
        for valid in [
            "https://app.example",
            "http://localhost:5173",
            "http://127.0.0.1:5173",
            "http://[::1]:5173",
            "http://[::1]",
        ] {
            assert!(
                canonical_origin(valid).is_ok(),
                "{valid} should be accepted"
            );
        }
        for invalid in [
            "",
            "*",
            "app.example",
            "ftp://app.example",
            "https://app.example/",
            "https://app.example?x=1",
            "https://user@app.example",
            "http://localhost:",
            "http://[]",
            "http://[::1]x",
            "http://[::1]:abc",
            "http://[example.com]",
            "http://app.example:65536",
        ] {
            assert!(
                canonical_origin(invalid).is_err(),
                "{invalid} should be rejected"
            );
        }
        assert_eq!(
            canonical_origin("HTTPS://APP.example:443").unwrap(),
            "https://app.example"
        );
        assert_eq!(
            canonical_origin("http://app.example:80").unwrap(),
            "http://app.example"
        );
    }

    #[test]
    fn a_session_is_single_use_after_logout() {
        let sessions = Sessions::new();
        let token = sessions.create().expect("session");
        assert!(sessions.valid(&token));
        assert!(sessions.remove(&token));
        assert!(!sessions.valid(&token));
        assert!(!sessions.remove(&token));
    }
}

/// `GET /admin/jobs/{jobId}`: job state for the desktop, which has no WS
/// client. The admin session is local and may inspect every job; browser
/// print sessions stay origin-scoped through the WS methods.
#[handler]
pub async fn job_status(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if !admin_session(&state, req) {
        return deny(res, WriteDenied::Session);
    }
    let Some(job_id) = req.param::<String>("jobId") else {
        return bad_request(res, ErrorCode::Schema, "jobId is required");
    };
    let id = printkit_facade::JobId::from(job_id.clone());
    let Some(status) = state.pool.status(&id) else {
        return respond(
            res,
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": { "code": "E_NOT_FOUND", "message": format!("unknown job: {job_id}") } })
                .to_string(),
        );
    };
    let phase = match status.phase {
        printkit_facade::JobPhase::Queued => printkit_protocol::JobState::Queued,
        printkit_facade::JobPhase::Printing => printkit_protocol::JobState::Printing,
        printkit_facade::JobPhase::Done => printkit_protocol::JobState::Done,
        printkit_facade::JobPhase::Failed => printkit_protocol::JobState::Failed,
        printkit_facade::JobPhase::Cancelled => printkit_protocol::JobState::Cancelled,
    };
    let pages = state
        .job_pages
        .lock()
        .ok()
        .and_then(|pages| pages.get(&job_id).copied());
    let error = status
        .failure
        .map(|failure| json!({ "code": failure.code, "message": failure.message }));
    let warnings: Vec<Value> = status
        .warnings
        .iter()
        .map(|warning| json!({ "code": warning.code, "message": warning.message }))
        .collect();
    ok(
        res,
        json!({
            "ok": true,
            "jobId": job_id,
            "status": phase,
            "queuePos": status.queue_pos,
            "pages": pages,
            "warnings": warnings,
            "error": error,
        }),
    );
}

/// `POST /admin/jobs/{jobId}/cancel`: same retraction rules as `job.cancel`.
#[handler]
pub async fn job_cancel(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return internal_error(res, "service state is missing");
    };
    if let Err(status) = check_write(&state, req, true) {
        return deny(res, status);
    }
    let Some(job_id) = req.param::<String>("jobId") else {
        return bad_request(res, ErrorCode::Schema, "jobId is required");
    };
    let id = printkit_facade::JobId::from(job_id.clone());
    let Some(control) = state.print_jobs.get(&id) else {
        return respond(
            res,
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": { "code": "E_NOT_FOUND", "message": format!("unknown job: {job_id}") } })
                .to_string(),
        );
    };
    control.request_cancel();
    if let Some(submitted) = control.submitted() {
        let backend = Arc::clone(&state.print_backend);
        return match tokio::task::spawn_blocking(move || backend.cancel_submitted(&submitted))
            .await
        {
            Ok(Ok(())) => ok(res, json!({ "ok": true, "jobId": job_id, "cancelled": true })),
            Ok(Err(error)) => respond(
                res,
                StatusCode::CONFLICT,
                json!({ "ok": false, "error": { "code": "E_CANCEL_UNSUPPORTED", "message": error.to_string() } })
                    .to_string(),
            ),
            Err(error) => internal_error(res, &format!("the cancel task failed: {error}")),
        };
    }
    match state.pool.cancel_job(&id) {
        Some(printkit_facade::CancelDisposition::Queued)
        | Some(printkit_facade::CancelDisposition::Requested)
        | Some(printkit_facade::CancelDisposition::ExternalQueued) => {
            ok(res, json!({ "ok": true, "jobId": job_id, "cancelled": true }))
        }
        Some(printkit_facade::CancelDisposition::Terminal) | None => respond(
            res,
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": { "code": "E_NOT_FOUND", "message": format!("job {job_id} already finished") } })
                .to_string(),
        ),
    }
}
