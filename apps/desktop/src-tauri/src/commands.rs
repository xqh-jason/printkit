//! Tauri commands for the desktop pages. All loopback HTTP goes through the
//! control client in this process; the WebView never receives a token.
use serde::{Deserialize, Serialize};

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};

use crate::control::{self, ControlClient, ControlError};
use crate::lifecycle::{self, DesktopState};

pub(crate) fn control_client(app: &AppHandle) -> Result<Arc<ControlClient>, String> {
    let state = app.state::<DesktopState>();
    let config = state.config();
    let status = lifecycle::status(app);
    let port = control::port_for(&status, config.as_ref())
        .ok_or_else(|| "the printkit service is not running".to_owned())?;
    if state.cached_control_port() == Some(port) {
        if let Some(client) = state.control() {
            return Ok(client);
        }
    }
    let data_dir = config
        .as_ref()
        .map(|config| PathBuf::from(&config.data_dir))
        .ok_or_else(|| "the printkit configuration is unavailable".to_owned())?;
    let client =
        Arc::new(ControlClient::connect(data_dir, port).map_err(|error| error.to_string())?);
    state.set_control(port, Arc::clone(&client));
    Ok(client)
}

async fn with_client<T, F>(app: AppHandle, call: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(Arc<ControlClient>) -> Result<T, ControlError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let client = control_client(&app)?;
        call(client).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("control task failed: {error}"))?
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn query(pairs: &[(&str, String)]) -> String {
    let encoded: Vec<String> = pairs
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!("{}={}", key, encode(value)))
        .collect();
    if encoded.is_empty() {
        String::new()
    } else {
        format!("?{}", encoded.join("&"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCapability {
    /// The single managed window.
    Business,
}

pub fn window_allows(label: &str, capability: WindowCapability) -> bool {
    match capability {
        WindowCapability::Business => label == "business",
    }
}

pub(crate) fn require_window(
    window: &tauri::WebviewWindow,
    capability: WindowCapability,
) -> Result<(), String> {
    if window_allows(window.label(), capability) {
        Ok(())
    } else {
        Err(format!(
            "E_UNSUPPORTED: window '{}' may not run this command",
            window.label()
        ))
    }
}

/// Writes are only available for the service this shell owns. An external
/// service is attached read-only, matching the README contract.
fn require_owned(app: &AppHandle) -> Result<(), String> {
    if writes_available(lifecycle::status(app).state) {
        Ok(())
    } else {
        Err(
            "E_UNSUPPORTED: the external printkit service is attached read-only; start it from this desktop shell"
                .to_owned(),
        )
    }
}

fn writes_available(state: lifecycle::ServiceState) -> bool {
    state == lifecycle::ServiceState::RunningOwned
}

fn canonical_under(root: &std::path::Path, target: &std::path::Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve the data directory: {error}"))?;
    let target = std::fs::canonicalize(target)
        .map_err(|error| format!("cannot resolve the target path: {error}"))?;
    if target.starts_with(&root) {
        Ok(target)
    } else {
        Err("refusing to open a path outside the data directory".to_owned())
    }
}

fn run_explorer(args: &[std::ffi::OsString]) -> Result<(), String> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let program = "/usr/bin/open";
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("cannot open the path: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the file manager exited with {status}"))
    }
}

fn reveal_command(target: &std::path::Path) -> Vec<std::ffi::OsString> {
    #[cfg(target_os = "macos")]
    {
        vec!["-R".into(), target.into()]
    }
    #[cfg(windows)]
    {
        vec!["/select,".into(), target.as_os_str().to_owned()]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = target;
        vec![target.parent().unwrap_or(target).into()]
    }
}

fn open_dir_command(target: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec![target.into()]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordStatsBucket {
    pub key: String,
    pub count: i64,
    pub sheets: i64,
    pub avg_dur_ms: f64,
}

#[derive(Debug, Deserialize)]
struct RecordStatsReply {
    buckets: Vec<RecordStatsBucket>,
}

fn stats_group(group_by: &str) -> Result<&'static str, String> {
    match group_by {
        "day" => Ok("day"),
        "printer" => Ok("printer"),
        _ => Err("E_SCHEMA: groupBy must be day or printer".to_owned()),
    }
}

fn stats_path(group_by: &str, from: Option<&str>, to: Option<&str>) -> Result<String, String> {
    let group = stats_group(group_by)?;
    Ok(format!(
        "/records/stats{}",
        query(&[
            ("groupBy", group.to_owned()),
            ("from", from.unwrap_or_default().to_owned()),
            ("to", to.unwrap_or_default().to_owned()),
        ])
    ))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn records_stats(
    window: tauri::WebviewWindow,
    app: AppHandle,
    group_by: String,
    from: Option<String>,
    to: Option<String>,
) -> Result<Vec<RecordStatsBucket>, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = stats_path(&group_by, from.as_deref(), to.as_deref())?;
    let value = with_client(app, move |client| client.get_json(&path)).await?;
    let reply: RecordStatsReply = serde_json::from_value(value)
        .map_err(|error| format!("E_SCHEMA: invalid records stats reply: {error}"))?;
    Ok(reply.buckets)
}

fn summary_path(
    from: Option<&str>,
    to: Option<&str>,
    tags: &[String],
    printer: Option<&str>,
    status: Option<&str>,
) -> String {
    format!(
        "/records/summary{}",
        query(&[
            ("from", from.unwrap_or_default().to_owned()),
            ("to", to.unwrap_or_default().to_owned()),
            ("tags", tags.join(",")),
            ("printer", printer.unwrap_or_default().to_owned()),
            ("status", status.unwrap_or_default().to_owned()),
        ])
    )
}

#[tauri::command(rename_all = "camelCase")]
pub async fn records_summary(
    window: tauri::WebviewWindow,
    app: AppHandle,
    from: Option<String>,
    to: Option<String>,
    tags: Option<Vec<String>>,
    printer: Option<String>,
    status: Option<String>,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = summary_path(
        from.as_deref(),
        to.as_deref(),
        tags.as_deref().unwrap_or_default(),
        printer.as_deref(),
        status.as_deref(),
    );
    with_client(app, move |client| client.get_json(&path)).await
}

#[tauri::command(rename_all = "camelCase")]
#[allow(clippy::too_many_arguments)]
pub async fn records_list(
    window: tauri::WebviewWindow,
    app: AppHandle,
    from: Option<String>,
    to: Option<String>,
    tags: Option<Vec<String>>,
    printer: Option<String>,
    status: Option<String>,
    cursor: Option<i64>,
    limit: Option<u32>,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = format!(
        "/records/list{}",
        query(&[
            ("from", from.unwrap_or_default()),
            ("to", to.unwrap_or_default()),
            ("tags", tags.unwrap_or_default().join(",")),
            ("printer", printer.unwrap_or_default()),
            ("status", status.unwrap_or_default()),
            (
                "cursor",
                cursor.map(|value| value.to_string()).unwrap_or_default()
            ),
            (
                "limit",
                limit.map(|value| value.to_string()).unwrap_or_default()
            ),
        ])
    );
    with_client(app, move |client| client.get_json(&path)).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn export_records(
    window: tauri::WebviewWindow,
    app: AppHandle,
    from: Option<String>,
    to: Option<String>,
    tags: Option<Vec<String>>,
    printer: Option<String>,
    status: Option<String>,
) -> Result<String, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = format!(
        "/records/export.csv{}",
        query(&[
            ("from", from.unwrap_or_default()),
            ("to", to.unwrap_or_default()),
            ("tags", tags.unwrap_or_default().join(",")),
            ("printer", printer.unwrap_or_default()),
            ("status", status.unwrap_or_default()),
        ])
    );
    with_client(app, move |client| {
        client
            .download(&path, "records", "csv")
            .map(|path| path.display().to_string())
    })
    .await
}

#[tauri::command(rename_all = "camelCase")]
#[allow(clippy::too_many_arguments)]
pub async fn logs_events(
    window: tauri::WebviewWindow,
    app: AppHandle,
    trace_id: Option<String>,
    stage: Option<String>,
    field: Option<String>,
    since: Option<u64>,
    minutes: Option<i64>,
    limit: Option<u64>,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = format!(
        "/admin/logs/events{}",
        query(&[
            ("traceId", trace_id.unwrap_or_default()),
            ("stage", stage.unwrap_or_default()),
            ("field", field.unwrap_or_default()),
            (
                "since",
                since.map(|value| value.to_string()).unwrap_or_default()
            ),
            (
                "minutes",
                minutes.map(|value| value.to_string()).unwrap_or_default()
            ),
            (
                "limit",
                limit.map(|value| value.to_string()).unwrap_or_default()
            ),
        ])
    );
    with_client(app, move |client| client.get_json(&path)).await
}

#[tauri::command]
pub async fn export_logs(window: tauri::WebviewWindow, app: AppHandle) -> Result<String, String> {
    require_window(&window, WindowCapability::Business)?;
    with_client(app, |client| {
        client
            .download("/admin/logs/export", "diagnostics", "zip")
            .map(|path| path.display().to_string())
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionView {
    pub days: u16,
    pub source: String,
    pub log_days: u16,
    pub log_source: String,
    pub records_days: u16,
    pub records_source: String,
    pub restart_required: bool,
}

fn retention_value(config: &Value, section: &str) -> Result<(u16, String), String> {
    let sourced = config
        .get(section)
        .and_then(|value| value.get("retention_days"))
        .ok_or_else(|| {
            format!("E_CONFIG: {section}.retention_days is missing from config --json")
        })?;
    let value = sourced
        .get("value")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("E_CONFIG: {section}.retention_days has no numeric value"))?;
    let days = u16::try_from(value)
        .map_err(|_| format!("E_CONFIG: {section}.retention_days is out of range"))?;
    let source = sourced
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    Ok((days, source))
}

fn retention_view(config: &Value) -> Result<RetentionView, String> {
    let (log_days, log_source) = retention_value(config, "log")?;
    let (records_days, records_source) = retention_value(config, "records")?;
    Ok(RetentionView {
        // One UI setting means both streams; if a legacy file has different
        // values, show the records value and include both components.
        days: records_days,
        source: records_source.clone(),
        log_days,
        log_source,
        records_days,
        records_source,
        restart_required: false,
    })
}

fn retention_range(days: u16) -> Result<(), String> {
    if (1..=3650).contains(&days) {
        Ok(())
    } else {
        Err("E_SCHEMA: retention days must be between 1 and 3650".to_owned())
    }
}

fn write_retention_days(path: &std::path::Path, days: u16) -> Result<(), String> {
    retention_range(days)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.permissions().readonly() {
            return Err(format!(
                "E_CONFIG: {} is read-only; make it writable in Settings",
                path.display()
            ));
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("E_CONFIG: cannot read {}: {error}", path.display())),
    };
    let mut document = if text.trim().is_empty() {
        toml::Value::Table(toml::Table::new())
    } else {
        toml::from_str::<toml::Value>(&text)
            .map_err(|error| format!("E_CONFIG: {} is not valid TOML: {error}", path.display()))?
    };
    let table = document
        .as_table_mut()
        .ok_or_else(|| format!("E_CONFIG: {} is not a TOML table", path.display()))?;
    match table.get_mut("retention") {
        Some(toml::Value::Table(retention)) => {
            retention.insert("days".to_owned(), toml::Value::Integer(i64::from(days)));
        }
        Some(_) => {
            return Err(format!(
                "E_CONFIG: {} has a non-table [retention] section",
                path.display()
            ));
        }
        None => {
            let mut retention = toml::Table::new();
            retention.insert("days".to_owned(), toml::Value::Integer(i64::from(days)));
            table.insert("retention".to_owned(), toml::Value::Table(retention));
        }
    }
    let serialized = toml::to_string_pretty(&document)
        .map_err(|error| format!("E_CONFIG: cannot encode {}: {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("E_CONFIG: cannot create {}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("E_CONFIG: cannot write {}: {error}", temporary.display()))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            let _ = file.set_permissions(metadata.permissions());
        }
        file.write_all(serialized.as_bytes())
            .map_err(|error| format!("E_CONFIG: cannot write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("E_CONFIG: cannot flush {}: {error}", temporary.display()))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("E_CONFIG: cannot replace {}: {error}", path.display()))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn get_retention_days(
    window: tauri::WebviewWindow,
    app: AppHandle,
) -> Result<RetentionView, String> {
    require_window(&window, WindowCapability::Business)?;
    let view = lifecycle::refresh_config(&app).await?;
    retention_view(&view.json)
}

#[tauri::command]
pub async fn set_retention_days(
    window: tauri::WebviewWindow,
    app: AppHandle,
    days: u16,
) -> Result<RetentionView, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    retention_range(days)?;
    let view = lifecycle::refresh_config(&app).await?;
    let path = std::path::PathBuf::from(&view.data_dir).join("config.toml");
    write_retention_days(&path, days)?;
    let refreshed = lifecycle::refresh_config(&app).await?;
    let mut next = retention_view(&refreshed.json)?;
    next.restart_required = !matches!(
        lifecycle::status(&app).state,
        lifecycle::ServiceState::Stopped
    );
    Ok(next)
}

#[tauri::command]
pub async fn list_printers(window: tauri::WebviewWindow, app: AppHandle) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let raw = lifecycle::run_sidecar(&app, &["printers", "--json"]).await?;
    serde_json::from_str(&raw).map_err(|error| format!("printers returned invalid JSON: {error}"))
}

#[tauri::command]
pub async fn list_papers(
    window: tauri::WebviewWindow,
    app: AppHandle,
    printer: String,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let args = vec![
        "papers".to_owned(),
        "--printer".to_owned(),
        printer,
        "--json".to_owned(),
    ];
    let raw = lifecycle::run_sidecar_owned(&app, args).await?;
    serde_json::from_str(&raw).map_err(|error| format!("papers returned invalid JSON: {error}"))
}

#[tauri::command]
pub async fn print_test(
    window: tauri::WebviewWindow,
    app: AppHandle,
    document: String,
    printer: Option<String>,
    copies: Option<u32>,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    print_document(&app, &document, printer, copies).await
}

fn write_temp_document(document: &str) -> Result<std::path::PathBuf, String> {
    use std::io::Write as _;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    for attempt in 0..16 {
        let path = std::env::temp_dir().join(format!(
            "printkit-desktop-{}-{stamp}-{attempt}.json",
            std::process::id()
        ));
        let mut file = match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("cannot create the test document: {error}"));
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(document.as_bytes())
            .map_err(|error| format!("cannot write the test document: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("cannot flush the test document: {error}"))?;
        return Ok(path);
    }
    Err("cannot allocate a unique test document path".to_owned())
}

async fn print_document(
    app: &AppHandle,
    document: &str,
    printer: Option<String>,
    copies: Option<u32>,
) -> Result<Value, String> {
    let path = write_temp_document(document)?;
    let data_dir = app
        .state::<DesktopState>()
        .config()
        .map(|config| config.data_dir)
        .ok_or_else(|| "the printkit configuration is unavailable".to_owned())?;
    let mut args = vec![
        "--data-dir".to_owned(),
        data_dir,
        "print".to_owned(),
        "-i".to_owned(),
        path.display().to_string(),
        "--json".to_owned(),
        "--no-record".to_owned(),
    ];
    if let Some(printer) = printer {
        args.push("--printer".to_owned());
        args.push(printer);
    }
    if let Some(copies) = copies {
        args.push("--copies".to_owned());
        args.push(copies.to_string());
    }
    let result = lifecycle::run_sidecar_owned(app, args).await;
    let _ = std::fs::remove_file(path);
    let raw = result?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("print returned invalid JSON: {error}"))?;
    let _ = app.emit("data-changed", ());
    Ok(value)
}

/// One PDF submission is bounded in memory before encoding.
///
/// Mirrors the service's single-PDF ceiling (`MAX_BATCH_PDF_BYTES`, 128 MiB):
/// `POST /print/batch` accepts one PDF part up to that size, and the service
/// still applies the 2000-page and total-request bounds.
const MAX_PDF_BYTES: usize = 128 * 1024 * 1024;
const PDF_MAGIC: &[u8] = b"%PDF-";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginsReply {
    pub origins: Vec<String>,
}

fn checked_request_id(request_id: Option<String>) -> Result<Option<String>, String> {
    match request_id {
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty()
                || value.len() > 128
                || !value.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(
                    "E_SCHEMA: requestId must be 1-128 printable ASCII characters".to_owned(),
                );
            }
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

fn validate_pdf(bytes: &[u8]) -> Result<(), String> {
    validate_pdf_with_limit(bytes, MAX_PDF_BYTES)
}

fn validate_pdf_with_limit(bytes: &[u8], limit: usize) -> Result<(), String> {
    if bytes.len() > limit {
        return Err(format!(
            "E_TOO_LARGE: the PDF is {} bytes; the limit is {limit}",
            bytes.len()
        ));
    }
    if !bytes.starts_with(PDF_MAGIC) {
        return Err("E_SCHEMA: the file does not start with %PDF-".to_owned());
    }
    Ok(())
}

fn multipart_pdf(
    bytes: &[u8],
    printer: Option<&str>,
    copies: Option<u32>,
) -> Result<(String, Vec<u8>), String> {
    let boundary = format!(
        "printkit-desktop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    let mut meta = serde_json::Map::new();
    if let Some(printer) = printer.filter(|value| !value.trim().is_empty()) {
        meta.insert("printer".to_owned(), Value::String(printer.to_owned()));
    }
    if let Some(copies) = copies {
        meta.insert("copies".to_owned(), Value::from(copies));
    }
    meta.insert("collate".to_owned(), Value::Bool(true));
    let meta = serde_json::to_vec(&Value::Object(meta))
        .map_err(|error| format!("E_SCHEMA: cannot encode print metadata: {error}"))?;
    let mut body = Vec::with_capacity(bytes.len() + meta.len() + 256);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&meta);
    body.extend_from_slice(
        format!("\r\n--{boundary}\r\nContent-Type: application/pdf\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/mixed; boundary={boundary}"), body))
}

async fn print_pdf_bytes_inner(
    app: AppHandle,
    bytes: Vec<u8>,
    printer: Option<String>,
    copies: Option<u32>,
    request_id: Option<String>,
) -> Result<Value, String> {
    validate_pdf(&bytes)?;
    let request_id = checked_request_id(request_id)?;
    let (content_type, body) = multipart_pdf(&bytes, printer.as_deref(), copies)?;
    let value = tauri::async_runtime::spawn_blocking(move || {
        let client = control_client(&app)?;
        client
            .post_local_multipart("/print/batch", &content_type, body, request_id.as_deref())
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("print task failed: {error}"))??;
    Ok(value)
}

/// Print PDF bytes chosen in the WebView. `bytes` is base64 because a Tauri
/// IPC argument is JSON, not a raw buffer.
#[tauri::command]
pub async fn print_pdf_bytes(
    window: tauri::WebviewWindow,
    app: AppHandle,
    bytes: String,
    file_name: String,
    printer: Option<String>,
    copies: Option<u32>,
    request_id: Option<String>,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(bytes.as_bytes())
        .map_err(|error| format!("E_SCHEMA: cannot decode {file_name}: {error}"))?;
    print_pdf_bytes_inner(app, decoded, printer, copies, request_id).await
}

fn origins_reply(value: Value) -> Result<OriginsReply, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("E_SCHEMA: invalid origins reply: {error}"))
}

#[tauri::command]
pub async fn job_status(
    window: tauri::WebviewWindow,
    app: AppHandle,
    job_id: String,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    let path = format!("/admin/jobs/{}", encode(&job_id));
    with_client(app, move |client| client.get_json(&path)).await
}

#[tauri::command]
pub async fn job_cancel(
    window: tauri::WebviewWindow,
    app: AppHandle,
    job_id: String,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    let path = format!("/admin/jobs/{}/cancel", encode(&job_id));
    with_client(app, move |client| {
        client
            .post_bytes(&path)
            .map(|reply| serde_json::from_slice(&reply.body).unwrap_or(Value::Null))
    })
    .await
}

#[tauri::command]
pub async fn origins_list(
    window: tauri::WebviewWindow,
    app: AppHandle,
) -> Result<OriginsReply, String> {
    require_window(&window, WindowCapability::Business)?;
    let value = with_client(app, |client| client.get_json("/admin/origins")).await?;
    origins_reply(value)
}

#[tauri::command]
pub async fn origin_add(
    window: tauri::WebviewWindow,
    app: AppHandle,
    origin: String,
) -> Result<OriginsReply, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    let value = with_client(app, move |client| {
        client.post_json("/admin/origins", &json!({ "origin": origin }))
    })
    .await?;
    origins_reply(value)
}

#[tauri::command]
pub async fn origin_remove(
    window: tauri::WebviewWindow,
    app: AppHandle,
    origin: String,
) -> Result<OriginsReply, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    let body = serde_json::json!({ "origin": origin });
    let value = with_client(app, move |client| {
        client.delete_json_body("/admin/origins", &body)
    })
    .await?;
    origins_reply(value)
}

#[tauri::command]
pub fn reveal_path(
    window: tauri::WebviewWindow,
    app: AppHandle,
    path: String,
) -> Result<(), String> {
    require_window(&window, WindowCapability::Business)?;
    let state = app.state::<DesktopState>();
    let config = state
        .config()
        .ok_or_else(|| "the printkit configuration is unavailable".to_owned())?;
    let target = canonical_under(&PathBuf::from(config.data_dir), &PathBuf::from(path))?;
    run_explorer(&reveal_command(&target))
}

#[tauri::command]
pub fn open_data_dir(window: tauri::WebviewWindow, app: AppHandle) -> Result<(), String> {
    require_window(&window, WindowCapability::Business)?;
    let state = app.state::<DesktopState>();
    let config = state
        .config()
        .ok_or_else(|| "the printkit configuration is unavailable".to_owned())?;
    let root = PathBuf::from(config.data_dir);
    let target = std::fs::canonicalize(&root)
        .map_err(|error| format!("cannot resolve the data directory: {error}"))?;
    run_explorer(&open_dir_command(&target))
}

/// `(window, id)` matrix for recovery buttons. Only the business page may
/// enable one of the two built-ins it can show.
pub fn owned_enable_allowed(label: &str, id: &str) -> bool {
    matches!((label, id), ("business", "observability" | "records"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSetRequest {
    pub id: Option<String>,
    pub preset: Option<String>,
    pub enabled: Option<bool>,
    pub confirm_data_count: Option<bool>,
    pub config: Option<Value>,
}

impl PluginSetRequest {
    fn body(&self) -> Result<Value, String> {
        let mut body = serde_json::Map::new();
        if let Some(id) = self.id.as_ref() {
            body.insert("id".to_owned(), Value::String(id.clone()));
        }
        if let Some(preset) = self.preset.as_ref() {
            body.insert("preset".to_owned(), Value::String(preset.clone()));
        }
        if let Some(enabled) = self.enabled {
            body.insert("enabled".to_owned(), Value::Bool(enabled));
        }
        if let Some(confirm) = self.confirm_data_count {
            body.insert("confirmDataCount".to_owned(), Value::Bool(confirm));
        }
        if let Some(config) = self.config.as_ref() {
            body.insert("config".to_owned(), config.clone());
        }
        if body.is_empty() {
            return Err("E_SCHEMA: the plugin request is empty".to_owned());
        }
        Ok(Value::Object(body))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLeaseRequest {
    pub id: String,
    pub op: String,
    pub lease_id: String,
}

fn valid_lease(request: &PluginLeaseRequest) -> Result<(), String> {
    if !matches!(request.op.as_str(), "acquire" | "heartbeat" | "release") {
        return Err(
            "E_SCHEMA: the lease operation is not acquire, heartbeat or release".to_owned(),
        );
    }
    if request.lease_id.is_empty()
        || request.lease_id.len() > 64
        || !request
            .lease_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("E_SCHEMA: the lease id must be 1-64 ASCII letters, digits or '-'".to_owned());
    }
    Ok(())
}

#[tauri::command]
pub async fn plugin_list(window: tauri::WebviewWindow, app: AppHandle) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    lifecycle::refresh_plugins(&app, true).await;
    let view = app.state::<DesktopState>().plugins_view();
    serde_json::to_value(view).map_err(|error| format!("E_SCHEMA: {error}"))
}

#[tauri::command]
pub async fn plugin_set(
    window: tauri::WebviewWindow,
    app: AppHandle,
    request: PluginSetRequest,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    require_owned(&app)?;
    let body = request.body()?;
    let reply = with_client(app.clone(), move |client| {
        client.post_json("/admin/plugins", &body)
    })
    .await?;
    lifecycle::apply_plugins(&app, &reply)?;
    Ok(reply)
}

/// Recovery only: enable one owned builtin from a window that actually shows
/// the disabled page. Disable/config stay on `plugin_set` (business + owned).
#[tauri::command]
pub async fn plugin_enable_owned(
    window: tauri::WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<Value, String> {
    if !owned_enable_allowed(window.label(), &id) {
        return Err(format!(
            "E_UNSUPPORTED: window '{}' cannot enable plugin '{id}'",
            window.label()
        ));
    }
    require_owned(&app)?;
    let body = json!({ "id": id, "enabled": true });
    let reply = with_client(app.clone(), move |client| {
        client.post_json("/admin/plugins", &body)
    })
    .await?;
    lifecycle::apply_plugins(&app, &reply)?;
    Ok(reply)
}

/// A trace viewer holds a short server-side lease so disabling observability
/// while Jobs/Logs is live returns 409 `inUse`. External services are allowed:
/// this is a read-only lease, not a configuration write.
#[tauri::command]
pub async fn plugin_lease(
    window: tauri::WebviewWindow,
    app: AppHandle,
    request: PluginLeaseRequest,
) -> Result<Value, String> {
    require_window(&window, WindowCapability::Business)?;
    valid_lease(&request)?;
    let body = json!({
        "id": request.id,
        "op": request.op,
        "leaseId": request.lease_id,
    });
    with_client(app, move |client| {
        client.post_json("/admin/plugin-lease", &body)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_get_the_smallest_capability_they_need() {
        assert!(window_allows("business", WindowCapability::Business));
        for label in ["dev", "records", "printers", "unexpected"] {
            assert!(!window_allows(label, WindowCapability::Business), "{label}");
        }
    }

    #[test]
    fn owned_recovery_is_limited_to_the_window_that_shows_the_page() {
        for id in ["observability", "records"] {
            assert!(owned_enable_allowed("business", id), "business/{id}");
        }
        for (label, id) in [
            ("printers", "records"),
            ("dev", "records"),
            ("records", "observability"),
            ("unknown-window", "records"),
            ("business", "unknown"),
            ("business", "templates"),
        ] {
            assert!(!owned_enable_allowed(label, id), "{label}/{id}");
        }
    }

    #[test]
    fn records_stats_maps_only_day_and_printer_groups() {
        assert_eq!(
            stats_path("day", None, None).unwrap(),
            "/records/stats?groupBy=day"
        );
        assert_eq!(
            stats_path("printer", Some("2026-01-01"), Some("2026-02-01")).unwrap(),
            "/records/stats?groupBy=printer&from=2026-01-01&to=2026-02-01"
        );
        for group in ["tag", "template", "", "DAY"] {
            assert!(stats_path(group, None, None).is_err(), "{group}");
        }
    }

    #[test]
    fn records_summary_forwards_optional_window_bounds() {
        assert_eq!(
            summary_path(None, None, &[], None, None),
            "/records/summary"
        );
        assert_eq!(
            summary_path(Some("2026-01-01"), Some("2026-12-31"), &[], None, None),
            "/records/summary?from=2026-01-01&to=2026-12-31"
        );
        assert_eq!(
            summary_path(
                None,
                None,
                &["store".to_owned(), "quote".to_owned()],
                Some("lab"),
                Some("done")
            ),
            "/records/summary?tags=store%2Cquote&printer=lab&status=done"
        );
    }

    #[test]
    fn records_stats_reply_is_the_service_camel_case_bucket_array() {
        let reply: RecordStatsReply = serde_json::from_value(json!({
            "buckets": [
                { "key": "2026-01-01", "count": 2, "sheets": 4, "avgDurMs": 12.5 }
            ]
        }))
        .unwrap();
        assert_eq!(reply.buckets.len(), 1);
        assert_eq!(reply.buckets[0].key, "2026-01-01");
        assert_eq!(reply.buckets[0].sheets, 4);
        assert_eq!(reply.buckets[0].avg_dur_ms, 12.5);
    }

    fn retention_path(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "printkit-retention-{}-{stamp}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn retention_writer_preserves_unrelated_config_keys() {
        let path = retention_path("config.toml");
        std::fs::write(
            &path,
            "[server]\nport = 19081\n[log]\nlevel = \"debug\"\nretention_days = 30\n[records]\nmax_rows = 1234\n",
        )
        .unwrap();
        write_retention_days(&path, 180).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["server"]["port"].as_integer(), Some(19081));
        assert_eq!(parsed["log"]["level"].as_str(), Some("debug"));
        assert_eq!(parsed["log"]["retention_days"].as_integer(), Some(30));
        assert_eq!(parsed["records"]["max_rows"].as_integer(), Some(1234));
        assert_eq!(parsed["retention"]["days"].as_integer(), Some(180));
        assert!(!retention_path("config.toml.tmp").exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retention_writer_refuses_corrupt_toml_without_touching_it() {
        let path = retention_path("broken.toml");
        let original = "[retention]\ndays = [";
        std::fs::write(&path, original).unwrap();
        let error = write_retention_days(&path, 30).unwrap_err();
        assert!(error.starts_with("E_CONFIG:"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn retention_writer_refuses_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = retention_path("readonly.toml");
        std::fs::write(&path, "[log]\nlevel = \"info\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let error = write_retention_days(&path, 30).unwrap_err();
        assert!(error.starts_with("E_CONFIG:"), "{error}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retention_range_and_view_contract_are_stable() {
        assert!(retention_range(1).is_ok());
        assert!(retention_range(3650).is_ok());
        assert!(retention_range(0).is_err());
        assert!(retention_range(3651).is_err());
        let view = retention_view(&json!({
            "log": {"retention_days": {"value": 90, "source": "default"}},
            "records": {"retention_days": {"value": 90, "source": "default"}}
        }))
        .unwrap();
        assert_eq!(view.days, 90);
        assert_eq!(view.source, "default");
        assert_eq!(view.log_days, 90);
        assert_eq!(view.records_days, 90);
        assert!(!view.restart_required);
    }

    #[test]
    fn retention_write_requires_business_and_owned() {
        assert!(window_allows("business", WindowCapability::Business));
        assert!(!window_allows("dev", WindowCapability::Business));
        assert!(!window_allows("records", WindowCapability::Business));
        assert!(writes_available(lifecycle::ServiceState::RunningOwned));
        assert!(!writes_available(lifecycle::ServiceState::RunningExternal));
        assert!(!writes_available(lifecycle::ServiceState::Stopped));
    }

    #[test]
    fn pdf_bytes_are_type_and_size_checked() {
        assert!(validate_pdf(b"%PDF-1.7\n...").is_ok());
        assert!(validate_pdf(b"not a pdf").is_err());
        assert!(validate_pdf_with_limit(b"%PDF-1.7", 4).is_err());
        assert!(validate_pdf_with_limit(b"%PDF-", 5).is_ok());
    }

    #[test]
    fn pdf_request_ids_are_bounded_printable_ascii() {
        assert_eq!(checked_request_id(None).unwrap(), None);
        assert_eq!(
            checked_request_id(Some(" pdf:label:1 ".to_owned())).unwrap(),
            Some("pdf:label:1".to_owned())
        );
        assert!(checked_request_id(Some(String::new())).is_err());
        assert!(checked_request_id(Some("x".repeat(129))).is_err());
        assert!(checked_request_id(Some("bad\nkey".to_owned())).is_err());
    }

    #[test]
    fn pdf_multipart_carries_metadata_and_bytes() {
        let (content_type, body) =
            multipart_pdf(b"%PDF-1.7 body", Some("Office"), Some(2)).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(content_type.starts_with("multipart/mixed; boundary="));
        assert!(text.contains("Content-Type: application/json"));
        assert!(text.contains("\"printer\":\"Office\""));
        assert!(text.contains("\"copies\":2"));
        assert!(text.contains("Content-Type: application/pdf"));
        assert!(text.contains("%PDF-1.7 body"));
    }

    #[test]
    fn print_document_temp_files_are_unique_and_created_new() {
        let first = write_temp_document("{\"first\":true}").unwrap();
        let second = write_temp_document("{\"second\":true}").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "{\"first\":true}");
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            "{\"second\":true}"
        );
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }

    #[test]
    fn lease_operations_and_ids_are_validated_before_http() {
        let valid = PluginLeaseRequest {
            id: "observability".to_owned(),
            op: "acquire".to_owned(),
            lease_id: "desk-7f3a".to_owned(),
        };
        assert!(valid_lease(&valid).is_ok());
        for op in ["acquire", "heartbeat", "release"] {
            assert!(
                valid_lease(&PluginLeaseRequest {
                    op: op.to_owned(),
                    ..valid.clone()
                })
                .is_ok()
            );
        }
        assert!(
            valid_lease(&PluginLeaseRequest {
                op: "take".to_owned(),
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            valid_lease(&PluginLeaseRequest {
                lease_id: String::new(),
                ..valid.clone()
            })
            .is_err()
        );
        assert!(
            valid_lease(&PluginLeaseRequest {
                lease_id: "a/b".to_owned(),
                ..valid
            })
            .is_err()
        );
    }

    #[test]
    fn plugin_bodies_use_the_service_camel_case_fields() {
        let request = PluginSetRequest {
            id: Some("observability".to_owned()),
            preset: None,
            enabled: Some(false),
            confirm_data_count: Some(true),
            config: None,
        };
        let body = request.body().unwrap();
        assert_eq!(body["id"], "observability");
        assert_eq!(body["enabled"], false);
        assert_eq!(body["confirmDataCount"], true);
        assert!(body.get("preset").is_none());
        // SetBody denies unknown fields; never emit confirmInUse.
        assert!(body.get("confirmInUse").is_none());
        let allowed = ["id", "preset", "enabled", "confirmDataCount", "config"];
        assert!(
            body.as_object()
                .unwrap()
                .keys()
                .all(|key| allowed.contains(&key.as_str()))
        );

        let empty = PluginSetRequest {
            id: None,
            preset: None,
            enabled: None,
            confirm_data_count: None,
            config: None,
        };
        assert!(empty.body().is_err());
    }
    #[test]
    fn external_services_are_read_only() {
        assert!(writes_available(lifecycle::ServiceState::RunningOwned));
        assert!(!writes_available(lifecycle::ServiceState::RunningExternal));
        assert!(!writes_available(lifecycle::ServiceState::Stopped));
    }

    #[test]
    fn reveal_stays_inside_the_data_directory() {
        let base = std::env::temp_dir().join(format!(
            "printkit-commands-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("data");
        let inside = root.join("logs/events.jsonl");
        let outside = base.join("outside.txt");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, b"x").unwrap();
        std::fs::write(&outside, b"x").unwrap();

        assert!(canonical_under(&root, &inside).is_ok());
        assert!(canonical_under(&root, &root.join("../outside.txt")).is_err());
        assert!(canonical_under(&root, &outside).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
