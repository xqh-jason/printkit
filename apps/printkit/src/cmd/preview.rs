//! `printkit preview`: open a document in a native preview window.
//!
//! Single-instance rule: a free data directory means this process takes
//! `serve.lock` and runs the service itself; a busy lock means another process
//! owns the WebView, so this command forwards the document through that
//! service's administrator control plane. There is no silent third mode: if
//! forwarding cannot authenticate or connect, the command reports `E_CONNECT`
//! and tells the user to start `printkit serve` first.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use printkit_facade as facade;
use printkit_protocol::ErrorCode;
use serde_json::{Value, json};

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;
use crate::service::lock::{ADMIN_TOKEN_FILE, LockError, ServeLock};

#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Input document JSON file, or `-` for stdin.
    #[arg(short = 'i', long = "input", value_name = "FILE")]
    pub input: PathBuf,

    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: PreviewArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    let data_dir = config.data_dir.value.clone();
    let port = config.server.port.value;
    let input = read_input(&args.input)?;
    let document: Value = serde_json::from_slice(&input).map_err(|error| Failure {
        code: "E_SCHEMA",
        message: format!("the input is not valid JSON: {error}"),
        detail: None,
        exit_code: 2,
    })?;

    match ServeLock::acquire(&data_dir, port) {
        Ok(lock) => own(config, lock, input, port, args.json),
        Err(LockError::Running {
            port: running_port, ..
        }) => forward(running_port, &data_dir, &document, args.json),
        Err(error) => Err(Failure::io(error.to_string())),
    }
}

/// Run the service in this process and keep it alive while the window exists.
fn own(
    config: AppConfig,
    lock: ServeLock,
    input: Vec<u8>,
    port: u16,
    json_output: bool,
) -> Result<(), Failure> {
    let state = crate::cmd::serve::build(config)?;
    let runtime = crate::cmd::serve::runtime()?;
    // An internal id cannot collide with WebSocket connection ids, so a
    // client disconnecting never tears down the window this command opened.
    let session = crate::service::preview::internal_session(format!("http://127.0.0.1:{port}"));
    let document = Arc::new(input);
    let on_print =
        crate::service::ws::preview_print_action(&state, &session, Arc::clone(&document));
    // `open` offloads the GUI hand-off to a blocking worker, so the main
    // thread must keep pumping the run loop while it waits. Blocking on the
    // future here would deadlock until the worker's 60 s timeout.
    let open_state = Arc::clone(&state);
    let open = runtime.spawn(async move {
        open_state
            .previews
            .open(&open_state, &session, (*document).clone(), on_print)
            .await
    });
    facade::run_main_loop_while(|| !open.is_finished());
    let opened = runtime
        .block_on(open)
        .map_err(|error| {
            Failure::runtime(
                "E_JOB_FAILED",
                format!("the preview task failed: {error}"),
                3,
            )
        })?
        .map_err(preview_failure)?;
    for warning in &opened.warnings {
        eprintln!("warning: {} {}", warning.code, warning.message);
    }
    let window = opened.window;

    if json_output {
        println!("{}", json!({ "windowId": window.get().to_string() }));
    } else {
        println!("windowId: {window}");
    }

    let server = runtime.spawn(crate::cmd::serve::serve(state.clone(), port));
    // The implicit serve lives exactly as long as the window. Closing it stops
    // the listener, so no second instance outlives the command.
    facade::run_main_loop_while(|| !server.is_finished() && state.previews.len() > 0);
    facade::close_all_previews();
    let mut stopping = false;
    for _ in 0..40 {
        if crate::cmd::serve::stop_listener(&state) {
            stopping = true;
            break;
        }
        if server.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !stopping && !server.is_finished() {
        // The listener never reported its handle; do not hang on a task that
        // nothing can stop.
        runtime.shutdown_timeout(Duration::from_secs(2));
        return Err(Failure::io(
            "the implicit serve never started listening".to_owned(),
        ));
    }
    // A bind failure ends the task before the window is closed; report it
    // instead of exiting zero on a window that could never be served.
    match runtime.block_on(server) {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => return Err(failure),
        Err(error) => {
            return Err(Failure::io(format!(
                "the implicit serve task failed: {error}"
            )));
        }
    }
    drop(lock);
    Ok(())
}

fn preview_failure(error: crate::service::preview::PreviewError) -> Failure {
    let code = error.code();
    let exit_code = match code {
        ErrorCode::Unsupported => 4,
        ErrorCode::Schema => 2,
        _ => 3,
    };
    Failure {
        code: code.as_str(),
        message: error.message(),
        detail: error.detail(),
        exit_code,
    }
}

/// Forward the request to the process that owns the data directory.
fn forward(port: u16, data_dir: &Path, document: &Value, json_output: bool) -> Result<(), Failure> {
    let window = forward_document(port, data_dir, document).map_err(|error| error.failure())?;
    if json_output {
        println!("{}", json!({ "windowId": window }));
    } else {
        println!("windowId: {window}");
    }
    Ok(())
}

enum ForwardError {
    /// The running service could not be reached or authenticated.
    Connect(String),
    /// The service answered with a protocol error; keep its code.
    Remote { code: ErrorCode, message: String },
}

impl ForwardError {
    fn failure(self) -> Failure {
        match self {
            Self::Connect(message) => Failure::runtime(
                "E_CONNECT",
                format!(
                    "cannot forward to a running serve: {message}; start `printkit serve` first"
                ),
                3,
            ),
            Self::Remote { code, message } => Failure {
                code: code.as_str(),
                message,
                detail: None,
                exit_code: if code == ErrorCode::Unsupported { 4 } else { 3 },
            },
        }
    }
}

fn forward_document(port: u16, data_dir: &Path, document: &Value) -> Result<String, ForwardError> {
    let admin = std::fs::read_to_string(data_dir.join(ADMIN_TOKEN_FILE)).map_err(|error| {
        ForwardError::Connect(format!("cannot read the administrator token: {error}"))
    })?;
    let origin = format!("http://127.0.0.1:{port}");

    let page = http(port, "GET", "/admin/session", &[], None).map_err(ForwardError::Connect)?;
    let csrf = page
        .cookie("pk_csrf")
        .ok_or_else(|| ForwardError::Connect("the admin session set no CSRF cookie".to_owned()))?;

    let login_body = json!({ "token": admin.trim() }).to_string();
    let csrf_cookie = format!("pk_csrf={csrf}");
    let login = http(
        port,
        "POST",
        "/admin/login",
        &[
            ("Origin", &origin),
            ("Cookie", &csrf_cookie),
            ("X-CSRF-Token", &csrf),
        ],
        Some(&login_body),
    )
    .map_err(ForwardError::Connect)?;
    if login.status != 200 {
        return Err(ForwardError::Connect(format!(
            "the administrator login returned {}",
            login.status
        )));
    }
    let session = login.cookie("pk_admin").ok_or_else(|| {
        ForwardError::Connect("the administrator login set no session cookie".to_owned())
    })?;

    let cookies = format!("pk_admin={session}; pk_csrf={csrf}");
    let body = document.to_string();
    let response = http(
        port,
        "POST",
        "/admin/preview",
        &[
            ("Origin", &origin),
            ("Cookie", &cookies),
            ("X-CSRF-Token", &csrf),
        ],
        Some(&body),
    )
    .map_err(ForwardError::Connect)?;

    if response.status == 200 {
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| ForwardError::Connect(format!("invalid preview response: {error}")))?;
        for warning in value["warnings"].as_array().into_iter().flatten() {
            let code = warning["code"].as_str().unwrap_or("?");
            let message = warning["message"].as_str().unwrap_or("");
            eprintln!("warning: {code} {message}");
        }
        return value
            .get("windowId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ForwardError::Connect("the service returned no windowId".to_owned()));
    }

    let failure: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let code = failure
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("E_CONNECT");
    let message = failure
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("the service refused the preview");
    let code = serde_json::from_value::<ErrorCode>(Value::String(code.to_owned()))
        .map_err(|_| ForwardError::Connect(format!("unknown error code: {code}")))?;
    Err(ForwardError::Remote {
        code,
        message: message.to_owned(),
    })
}

fn read_input(path: &Path) -> Result<Vec<u8>, Failure> {
    if path == Path::new("-") {
        let mut input = Vec::new();
        std::io::stdin()
            .read_to_end(&mut input)
            .map_err(|error| Failure::io(format!("cannot read stdin: {error}")))?;
        return Ok(input);
    }
    std::fs::read(path).map_err(|error| {
        Failure::io(format!(
            "cannot read the input document {}: {error}",
            path.display()
        ))
    })
}

// --- loopback control plane ------------------------------------------------
//
// `preview` is a client of a service it did not start. It borrows the
// administrator token file, logs in through the documented admin page to get
// a session and a CSRF token, then posts the document to `/admin/preview`,
// which opens the window in the owning process. HTTP is hand-written here to
// avoid pulling a second HTTP stack into the CLI.

pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl HttpResponse {
    pub(crate) fn cookie(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        for (header, value) in &self.headers {
            if !header.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            let first = value.split(';').next().unwrap_or(value);
            if let Some(rest) = first.trim().strip_prefix(&prefix) {
                return Some(rest.to_owned());
            }
        }
        None
    }
}
pub(crate) fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<HttpResponse, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| format!("cannot connect to 127.0.0.1:{port}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("cannot set the read timeout: {error}"))?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("Connection: close\r\n\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot send the request: {error}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| format!("cannot read the response: {error}"))?;
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "the HTTP response has no header block".to_owned())?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "the HTTP response has no status".to_owned())?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let body_bytes = &raw[split + 4..];
    let body = if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    }) {
        decode_chunked(body_bytes)?
    } else {
        body_bytes.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut rest = bytes;
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| "truncated chunked body".to_owned())?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..line_end])
                .map_err(|_| "invalid chunk size")?
                .trim(),
            16,
        )
        .map_err(|_| "invalid chunk size")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("truncated chunk".to_owned());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}
