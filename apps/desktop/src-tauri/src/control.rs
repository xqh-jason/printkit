//! Loopback control client.
//!
//! The automatic `<data_dir>/admin-token` credential is read here and nowhere
//! else. It is used for one login, then dropped. Command handlers, serde
//! structures, logs and the WebView never receive it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use crate::lifecycle::{ConfigView, ServiceState, ServiceStatus};

const ADMIN_TOKEN_FILE: &str = "admin-token";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(60);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(90);
const STAMP: &[FormatItem<'static>] = format_description!("[year][month][day]-[hour][minute][second]");

#[derive(Debug, Clone)]
pub struct ControlError {
    pub code: String,
    pub status: Option<u16>,
    pub message: String,
}

impl ControlError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            status: None,
            message: message.into(),
        }
    }

    fn status(status: u16, message: impl Into<String>) -> Self {
        Self {
            code: if status == 401 {
                "E_AUTH".to_owned()
            } else {
                "E_JOB_FAILED".to_owned()
            },
            status: Some(status),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(formatter, "{} (HTTP {status}): {}", self.code, self.message),
            None => write!(formatter, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<ureq::Error> for ControlError {
    fn from(error: ureq::Error) -> Self {
        match error {
            ureq::Error::Timeout(_) => Self::new("E_TIMEOUT", error.to_string()),
            _ => Self::new("E_CONNECT", error.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ControlReply {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Result<Value, ControlError> {
        serde_json::from_slice(&self.body)
            .map_err(|error| ControlError::new("E_SCHEMA", format!("invalid JSON: {error}")))
    }
}

pub struct ControlClient {
    data_dir: PathBuf,
    port: u16,
    agent: ureq::Agent,
    export_agent: ureq::Agent,
    csrf: Mutex<String>,
    session: Mutex<String>,
    login: Mutex<()>,
    login_inflight: AtomicBool,
    login_generation: AtomicU64,
}

impl ControlClient {
    pub fn connect(data_dir: PathBuf, port: u16) -> Result<Self, ControlError> {
        Self::connect_with_timeout(data_dir, port, TOTAL_TIMEOUT)
    }

    /// Connect with a caller-supplied budget. The stop path uses this so a
    /// wedged service cannot keep Cmd+Q waiting for the normal 90 s budget.
    pub fn connect_with_timeout(
        data_dir: PathBuf,
        port: u16,
        timeout: Duration,
    ) -> Result<Self, ControlError> {
        let connect_timeout = CONNECT_TIMEOUT.min(timeout);
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .timeout_connect(Some(connect_timeout))
                .timeout_recv_body(Some(REQUEST_TIMEOUT.min(timeout)))
                .max_idle_connections(2)
                .http_status_as_error(false)
                .build(),
        );
        let export_agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .timeout_connect(Some(connect_timeout))
                .timeout_recv_body(Some(EXPORT_TIMEOUT.min(timeout)))
                .max_idle_connections(2)
                .http_status_as_error(false)
                .build(),
        );
        let client = Self {
            data_dir,
            port,
            agent,
            export_agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        client.login_locked()?;
        Ok(client)
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn login_locked(&self) -> Result<(), ControlError> {
        let token = read_admin_token(&self.data_dir)?;

        // The session endpoint also seeds the CSRF cookie.
        let session = self.raw("GET", "/admin/session", None)?;
        if session.status != 200 {
            return Err(ControlError::status(session.status, "cannot read the control session"));
        }
        if !valid_session_body(&session.body) {
            return Err(ControlError::new(
                "E_PROTOCOL",
                "the endpoint is not a printkit admin session",
            ));
        }
        self.capture_csrf(&session);

        let body = serde_json::json!({ "token": token }).to_string();
        drop(token);
        let reply = self.raw("POST", "/admin/login", Some(body.as_bytes()))?;
        if reply.status != 200 {
            return Err(ControlError::status(reply.status, "control login failed"));
        }
        let session = cookie_value(&reply, "pk_admin")
            .ok_or_else(|| ControlError::new("E_AUTH", "control login set no session cookie"))?;
        *lock(&self.session) = session;
        Ok(())
    }

    fn relogin(&self, seen_generation: u64) -> Result<(), ControlError> {
        if self.login_generation.load(Ordering::Acquire) > seen_generation {
            return Ok(());
        }
        if self
            .login_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _guard = lock(&self.login);
            if self.login_generation.load(Ordering::Acquire) > seen_generation {
                self.login_inflight.store(false, Ordering::Release);
                return Ok(());
            }
            let result = self.login_locked();
            if result.is_ok() {
                self.login_generation.fetch_add(1, Ordering::Release);
            }
            self.login_inflight.store(false, Ordering::Release);
            return result;
        }
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        while Instant::now() < deadline {
            if self.login_generation.load(Ordering::Acquire) > seen_generation {
                return Ok(());
            }
            if !self.login_inflight.load(Ordering::Acquire) {
                return Err(ControlError::new("E_AUTH", "concurrent control login failed"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err(ControlError::new("E_TIMEOUT", "timed out waiting for control login"))
    }

    fn capture_csrf(&self, reply: &ControlReply) {
        if let Some(csrf) = cookie_value(reply, "pk_csrf") {
            *lock(&self.csrf) = csrf;
        }
    }

    fn cookie_header(&self) -> String {
        let session = lock(&self.session).clone();
        let csrf = lock(&self.csrf).clone();
        let mut parts = Vec::new();
        if !csrf.is_empty() {
            parts.push(format!("pk_csrf={csrf}"));
        }
        if !session.is_empty() {
            parts.push(format!("pk_admin={session}"));
        }
        parts.join("; ")
    }

    fn raw(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<ControlReply, ControlError> {
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(self.url(path))
            .header("Origin", self.origin());
        let cookies = self.cookie_header();
        if !cookies.is_empty() {
            request = request.header("Cookie", cookies);
        }
        let csrf = lock(&self.csrf).clone();
        if !csrf.is_empty() {
            request = request.header("X-CSRF-Token", csrf);
        }
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        let request = request
            .body(body.unwrap_or_default().to_vec())
            .map_err(|error| ControlError::new("E_PROTOCOL", error.to_string()))?;
        let agent = if is_export(path) {
            &self.export_agent
        } else {
            &self.agent
        };
        let response = agent.run(request)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let mut response = response;
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| ControlError::new("E_CONNECT", error.to_string()))?;
        Ok(ControlReply {
            status,
            headers,
            body,
        })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<ControlReply, ControlError> {
        let generation = self.login_generation.load(Ordering::Acquire);
        let reply = self.raw(method, path, body)?;
        if reply.status == 401 {
            self.relogin(generation)?;
            let retry = self.raw(method, path, body)?;
            if !(200..300).contains(&retry.status) {
                return Err(error_from_reply(&retry));
            }
            return Ok(retry);
        }
        if !(200..300).contains(&reply.status) {
            return Err(error_from_reply(&reply));
        }
        Ok(reply)
    }

    pub fn get_json(&self, path: &str) -> Result<Value, ControlError> {
        self.request("GET", path, None)?.json()
    }

    pub fn post_json(&self, path: &str, value: &Value) -> Result<Value, ControlError> {
        let body = serde_json::to_vec(value)
            .map_err(|error| ControlError::new("E_SCHEMA", error.to_string()))?;
        self.request("POST", path, Some(&body))?.json()
    }

    pub fn get_bytes(&self, path: &str) -> Result<ControlReply, ControlError> {
        self.request("GET", path, None)
    }

    /// Local binary multipart to `/print/batch`. No Origin and no cookies:
    /// a local script without an Origin gets the print scope by design, while
    /// an admin cookie would make the service look for a whitelisted browser
    /// origin instead.
    ///
    /// A 401 still refreshes the control session once and retries the same
    /// bytes with the same idempotency key. The retry deliberately stays
    /// cookie-less for the same reason; a failed second attempt reports the
    /// server error instead of hiding it.
    pub fn post_local_multipart(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ControlError> {
        let generation = self.login_generation.load(Ordering::Acquire);
        let reply = self.send_local_multipart(path, content_type, &body, idempotency_key)?;
        if reply.status == 401 {
            self.relogin(generation)?;
            let retry = self.send_local_multipart(path, content_type, &body, idempotency_key)?;
            if !(200..300).contains(&retry.status) {
                return Err(error_from_reply(&retry));
            }
            return retry.json();
        }
        if !(200..300).contains(&reply.status) {
            return Err(error_from_reply(&reply));
        }
        reply.json()
    }

    fn send_local_multipart(
        &self,
        path: &str,
        content_type: &str,
        body: &[u8],
        idempotency_key: Option<&str>,
    ) -> Result<ControlReply, ControlError> {
        let mut builder = ureq::http::Request::builder()
            .method("POST")
            .uri(self.url(path))
            .header("Content-Type", content_type);
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        let request = builder
            .body(body)
            .map_err(|error| ControlError::new("E_PROTOCOL", error.to_string()))?;
        let mut response = self.agent.run(request)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| ControlError::new("E_CONNECT", error.to_string()))?;
        Ok(ControlReply {
            status,
            headers: Vec::new(),
            body,
        })
    }

    /// DELETE carrying a JSON body, for endpoints that identify the row in the
    /// body instead of a query string.
    pub fn delete_json_body(&self, path: &str, value: &Value) -> Result<Value, ControlError> {
        let body = serde_json::to_vec(value)
            .map_err(|error| ControlError::new("E_SCHEMA", error.to_string()))?;
        self.request("DELETE", path, Some(&body))?.json()
    }

    pub fn post_bytes(&self, path: &str) -> Result<ControlReply, ControlError> {
        self.request("POST", path, Some(b""))
    }

    pub fn download(&self, path: &str, kind: &str, extension: &str) -> Result<PathBuf, ControlError> {
        let reply = if path.starts_with("/admin/logs/export") {
            self.post_bytes(path)?
        } else {
            self.get_bytes(path)?
        };
        write_download(kind, extension, &reply.body)
    }

    pub fn shutdown(&self) -> Result<(), ControlError> {
        self.request("POST", "/admin/shutdown", Some(b""))?;
        Ok(())
    }
}

/// Status port wins for owned and external services; the configured port is
/// only the argument used to start a stopped service.
pub fn port_for(status: &ServiceStatus, _config: Option<&ConfigView>) -> Option<u16> {
    // A non-answering port must never receive the admin token.
    if status.state != ServiceState::Stopped && status.reachable {
        status.port
    } else {
        None
    }
}

/// `/admin/session` is the only pre-login probe; its shape identifies a
/// printkit service before the 0600 token is sent anywhere.
fn valid_session_body(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    if value.as_object().is_some_and(|object| object.is_empty()) {
        // Older services answered `{}`; accept the shape but not HTML/errors.
        return true;
    }
    value
        .get("authenticated")
        .is_some_and(Value::is_boolean)
}

fn is_export(path: &str) -> bool {
    path.starts_with("/admin/logs/export")
        || path.starts_with("/records/export.csv")
}

fn error_from_reply(reply: &ControlReply) -> ControlError {
    if let Ok(value) = serde_json::from_slice::<Value>(&reply.body) {
        if let Some(error) = value.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("E_JOB_FAILED");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("control request failed");
            return ControlError {
                code: code.to_owned(),
                status: Some(reply.status),
                message: message.to_owned(),
            };
        }
    }
    ControlError::status(reply.status, "control request failed")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn read_admin_token(data_dir: &Path) -> Result<String, ControlError> {
    let path = data_dir.join(ADMIN_TOKEN_FILE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path)
            .map_err(|error| ControlError::new("E_AUTH", format!("cannot read the control credential: {error}")))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            return Err(ControlError::new(
                "E_AUTH",
                format!("the control credential must be mode 0600, found {mode:o}"),
            ));
        }
    }
    let token = fs::read_to_string(&path)
        .map_err(|error| ControlError::new("E_AUTH", format!("cannot read the control credential: {error}")))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(ControlError::new("E_AUTH", "the control credential is empty"));
    }
    Ok(token)
}

fn cookie_value(reply: &ControlReply, name: &str) -> Option<String> {
    for (key, value) in &reply.headers {
        if !key.eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        for part in value.split(';') {
            let mut pair = part.trim().splitn(2, '=');
            if pair.next() == Some(name) {
                return pair.next().map(str::to_owned);
            }
        }
    }
    None
}

fn write_download(kind: &str, extension: &str, bytes: &[u8]) -> Result<PathBuf, ControlError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| ControlError::new("E_IO", "HOME is not set"))?;
    let directory = home.join("Downloads");
    fs::create_dir_all(&directory)
        .map_err(|error| ControlError::new("E_IO", format!("cannot create Downloads: {error}")))?;
    write_download_to(&directory, kind, extension, bytes)
}

fn write_download_to(
    directory: &Path,
    kind: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, ControlError> {
    let stamp = OffsetDateTime::now_utc()
        .format(STAMP)
        .unwrap_or_else(|_| "unknown".to_owned());
    let base = format!("printkit-{kind}-{stamp}.{extension}");
    let mut path = directory.join(&base);
    let mut suffix = 2;
    while path.exists() {
        path = directory.join(format!(
            "printkit-{kind}-{stamp}-{suffix}.{extension}"
        ));
        suffix += 1;
    }
    let temporary = path.with_extension(format!("{extension}.tmp"));
    fs::write(&temporary, bytes)
        .map_err(|error| ControlError::new("E_IO", format!("cannot write export: {error}")))?;
    fs::rename(&temporary, &path)
        .map_err(|error| ControlError::new("E_IO", format!("cannot finish export: {error}")))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    fn serve_once(listener: TcpListener, response: &str) {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer);
        socket.write_all(response.as_bytes()).unwrap();
    }

    fn serve_two(listener: TcpListener, first: &str, second: &str) {
        for response in [first, second] {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            socket.write_all(response.as_bytes()).unwrap();
        }
    }

    #[test]
    fn chunked_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            serve_once(
                listener,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            );
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build(),
        );
        let client = ControlClient {
            data_dir: PathBuf::new(),
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        let reply = client.raw("GET", "/chunked", None).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(String::from_utf8(reply.body).unwrap(), "hello world");
    }

    #[test]
    fn cookie_flow() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            serve_two(
                listener,
                "HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf-value; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                "HTTP/1.1 200 OK\r\nSet-Cookie: pk_admin=session-value; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
        });
        let client = ControlClient::connect(directory, port).unwrap();
        assert!(client.cookie_header().contains("pk_csrf=csrf-value"));
        assert!(client.cookie_header().contains("pk_admin=session-value"));
    }

    #[test]
    fn truncated_chunked_body_is_an_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            serve_once(
                listener,
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n",
            );
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = ControlClient {
            data_dir: PathBuf::new(),
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        assert!(client.raw("GET", "/truncated", None).is_err());
    }

    #[test]
    fn relogin_is_single_flight() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let logins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_logins = Arc::clone(&logins);
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                if request.contains("/admin/login") {
                    server_logins.fetch_add(1, Ordering::SeqCst);
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nSet-Cookie: pk_admin=session; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                }
            }
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = Arc::new(ControlClient {
            data_dir: directory,
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        });
        let first = Arc::clone(&client);
        let second = Arc::clone(&client);
        let a = thread::spawn(move || first.relogin(0));
        let b = thread::spawn(move || second.relogin(0));
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        assert_eq!(logins.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn server_error_body_keeps_code_and_status() {
        let reply = ControlReply {
            status: 400,
            headers: Vec::new(),
            body: br#"{"error":{"code":"E_SCHEMA","message":"bad","detail":{"pointer":"/x"}}}"#.to_vec(),
        };
        let error = error_from_reply(&reply);
        assert_eq!(error.code, "E_SCHEMA");
        assert_eq!(error.status, Some(400));
        assert_eq!(error.message, "bad");
    }

    #[test]
    fn a_401_is_retried_exactly_once_after_login() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let protected = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_protected = Arc::clone(&protected);
        thread::spawn(move || {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                let response = if request.contains("/admin/login") {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_admin=session; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else if request.contains("/admin/session") {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    let n = server_protected.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    }
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = ControlClient {
            data_dir: directory,
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        client.request("GET", "/protected", None).unwrap();
        assert_eq!(protected.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn multipart_401_relogs_once_and_retries_the_same_key() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_seen = Arc::clone(&seen);
        thread::spawn(move || {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 65536];
                let _ = socket.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let first_line = request.lines().next().unwrap_or_default().to_owned();
                server_seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(first_line);
                let response = if path == "/admin/session" {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else if path == "/admin/login" {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_admin=session; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    let retried = server_seen
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .filter(|line| line.starts_with("POST /print/batch"))
                        .count();
                    if retried <= 1 {
                        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"jobId\":\"job-1\"}"
                    }
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = ControlClient {
            data_dir: directory,
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        let reply = client
            .post_local_multipart(
                "/print/batch",
                "multipart/mixed; boundary=b",
                b"--b\r\n\r\nbody\r\n--b--\r\n".to_vec(),
                Some("pdf:attempt-1"),
            )
            .unwrap();
        assert_eq!(reply["jobId"], "job-1");
        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            seen.len(),
            4,
            "session, login and two batch attempts only: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|line| line.starts_with("POST /print/batch"))
                .count(),
            2
        );
        assert_eq!(
            seen.iter()
                .filter(|line| line.starts_with("POST /admin/login"))
                .count(),
            1
        );
    }

    #[test]
    fn multipart_retry_still_401_reports_the_server_error() {
        let directory = tempdir();
        fs::write(directory.join(ADMIN_TOKEN_FILE), "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                directory.join(ADMIN_TOKEN_FILE),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 65536];
                let _ = socket.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let response = if path == "/admin/session" {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else if path == "/admin/login" {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: pk_admin=session; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{\"ok\":false,\"error\":{\"code\":\"E_AUTH\",\"message\":\"not signed in\"}}"
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = ControlClient {
            data_dir: directory,
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        };
        let error = client
            .post_local_multipart("/print/batch", "multipart/mixed; boundary=b", vec![], None)
            .unwrap_err();
        assert_eq!(error.code, "E_AUTH");
        assert_eq!(error.status, Some(401));
        assert!(error.message.contains("not signed in"), "{error}");
    }

    #[test]
    fn relogin_failure_is_shared_and_not_repeated() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let logins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_logins = Arc::clone(&logins);
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                if request.contains("/admin/login") {
                    server_logins.fetch_add(1, Ordering::SeqCst);
                    socket
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nSet-Cookie: pk_csrf=csrf; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                }
            }
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder().http_status_as_error(false).build(),
        );
        let client = Arc::new(ControlClient {
            data_dir: directory,
            port,
            agent: agent.clone(),
            export_agent: agent,
            csrf: Mutex::new(String::new()),
            session: Mutex::new(String::new()),
            login: Mutex::new(()),
            login_inflight: AtomicBool::new(false),
            login_generation: AtomicU64::new(0),
        });
        let first = Arc::clone(&client);
        let second = Arc::clone(&client);
        let a = thread::spawn(move || first.relogin(0));
        let b = thread::spawn(move || second.relogin(0));
        assert!(a.join().unwrap().is_err());
        assert!(b.join().unwrap().is_err());
        assert_eq!(logins.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn export_names_do_not_overwrite() {
        let directory = tempdir();
        let first = write_download_to(&directory, "records", "csv", b"one").unwrap();
        let second = write_download_to(&directory, "records", "csv", b"two").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), b"one");
        assert_eq!(fs::read(second).unwrap(), b"two");
    }

    #[test]
    fn external_port_wins_over_config() {
        let status = ServiceStatus {
            state: ServiceState::RunningExternal,
            pid: Some(1),
            port: Some(12345),
            reachable: true,
            lock_path: "serve.lock".to_owned(),
            stale_lock: false,
            detail: None,
        };
        let config = ConfigView {
            data_dir: "/tmp".to_owned(),
            port: 19080,
            json: serde_json::json!({}),
        };
        assert_eq!(port_for(&status, Some(&config)), Some(12345));
    }

    #[test]
    #[ignore]
    fn live_salvo() {
        let data = std::env::var("PRINTKIT_TEST_DATA").expect("PRINTKIT_TEST_DATA");
        let port: u16 = std::env::var("PRINTKIT_TEST_PORT")
            .expect("PRINTKIT_TEST_PORT")
            .parse()
            .expect("port");
        let client = ControlClient::connect(PathBuf::from(data), port).unwrap();
        let events = client.get_bytes("/admin/logs/events").unwrap();
        assert_eq!(events.status, 200);
        assert!(events.header("content-length").is_some());
        let export = client.post_bytes("/admin/logs/export").unwrap();
        assert_eq!(export.status, 200);
        assert!(export
            .header("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked")));
        assert!(export.body.starts_with(b"PK\x03\x04"));
        let csv = client.get_bytes("/records/export.csv").unwrap();
        assert_eq!(csv.status, 200);
        assert!(csv.body.starts_with(b"\xEF\xBB\xBF"));
    }

    #[test]
    fn a_hung_service_times_out_within_the_stop_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let _held = stream;
                thread::sleep(Duration::from_secs(3));
            }
        });
        let dir = tempdir();
        fs::write(dir.join("admin-token"), "secret\n").unwrap();
        let started = Instant::now();
        let error = match ControlClient::connect_with_timeout(dir, port, Duration::from_secs(1)) {
            Ok(_) => panic!("the hung service unexpectedly logged in"),
            Err(error) => error,
        };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "connect took {:?}: {error}",
            started.elapsed()
        );
    }

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "printkit-control-test-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_too_large_reply_keeps_its_code_and_message() {
        let reply = ControlReply {
            status: 413,
            headers: Vec::new(),
            body: br#"{"ok":false,"error":{"code":"E_TOO_LARGE","message":"one application/pdf part exceeds 134217728 bytes"}}"#.to_vec(),
        };
        let error = error_from_reply(&reply);
        assert_eq!(error.code, "E_TOO_LARGE");
        assert_eq!(error.status, Some(413));
        assert!(error.message.contains("134217728"), "{}", error.message);
    }

    #[test]
    fn a_lookalike_port_never_receives_the_admin_token() {
        let directory = tempdir();
        let token_path = directory.join(ADMIN_TOKEN_FILE);
        fs::write(&token_path, "secret-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            serve_once(
                listener,
                "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
        });
        let error = match ControlClient::connect(directory, port) {
            Ok(_) => panic!("a lookalike service must not log in"),
            Err(error) => error,
        };
        assert_eq!(error.code, "E_PROTOCOL", "{error:?}");
    }

    #[test]
    fn only_a_printkit_session_may_receive_the_token() {
        assert!(valid_session_body(br#"{"authenticated":false}"#));
        assert!(valid_session_body(br#"{"authenticated":true}"#));
        assert!(!valid_session_body(br#"{"ok":true}"#));
        assert!(valid_session_body(b"{}"));
        assert!(!valid_session_body(b"<html>login</html>"));
    }

    #[test]
    fn a_closed_or_stopped_service_gets_no_token_port() {
        let status = |state: ServiceState, reachable: bool| ServiceStatus {
            state,
            pid: Some(42),
            port: Some(19080),
            reachable,
            lock_path: "serve.lock".to_owned(),
            stale_lock: false,
            detail: None,
        };
        assert_eq!(
            port_for(&status(ServiceState::RunningOwned, true), None),
            Some(19080)
        );
        assert_eq!(
            port_for(&status(ServiceState::RunningExternal, true), None),
            Some(19080)
        );
        assert_eq!(port_for(&status(ServiceState::Stopped, false), None), None);
        assert_eq!(
            port_for(&status(ServiceState::RunningExternal, false), None),
            None
        );
    }

}
