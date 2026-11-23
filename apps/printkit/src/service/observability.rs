//! Observability: trace streaming and the business records/logs JSON endpoints.
//!
//! `trace.subscribe` replays from the retained JSONL files and then polls
//! them, so a reconnect with the last `seq` produces the same events a live
//! connection would have seen. The usage endpoints share one SQL builder with
//! the CLI, which is why the desktop records page, the WebSocket responses and
//! `printkit stats` cannot disagree.
//!
//! A `print` token is always forced to its bound origin; an administrator
//! session sees every origin. The two paths differ only in that filter.

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration as StdDuration;

use printkit_facade::{Scalar, Stage, TraceEvent, events_since};
use printkit_protocol::{
    ErrorCode, LogsExportParams, LogsExportResult, TraceEventView, TraceStage,
    TraceSubscribeParams, TraceTailParams, TraceTailResult, TraceValue,
};
use salvo::http::{HeaderValue, StatusCode};
use salvo::prelude::*;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

use super::{AppState, admin};

const LIVE_POLL_MS: u64 = 400;

/// A failure that maps directly onto a protocol error response.
pub struct ObservabilityError {
    pub code: ErrorCode,
    pub message: String,
}

impl ObservabilityError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn stage_view(stage: Stage) -> TraceStage {
    match stage {
        Stage::Recv => TraceStage::Recv,
        Stage::Auth => TraceStage::Auth,
        Stage::Validate => TraceStage::Validate,
        Stage::Queue => TraceStage::Queue,
        Stage::Render => TraceStage::Render,
        Stage::Submit => TraceStage::Submit,
        Stage::Result => TraceStage::Result,
    }
}

fn value_view(value: &Scalar) -> TraceValue {
    match value {
        Scalar::Bool(value) => TraceValue::Bool(*value),
        Scalar::Int(value) => TraceValue::Int(*value),
        Scalar::Float(value) => TraceValue::Float(*value),
        Scalar::Str(value) => TraceValue::Str(value.clone()),
    }
}

/// Convert one stored event into the published view.
pub fn trace_view(event: &TraceEvent) -> TraceEventView {
    TraceEventView {
        seq: event.seq,
        trace_id: event.trace_id.as_str().to_owned(),
        stage: stage_view(event.stage),
        at: event.at.format(&Rfc3339).unwrap_or_default(),
        dur_ms: event.dur_ms,
        ok: event.ok,
        code: event.code.clone(),
        detail: event.detail.clone(),
        origin: event.origin.clone(),
        printer: event.printer.clone(),
        pages: event.pages,
        queue_pos: event.queue_pos,
        fields: event
            .fields
            .iter()
            .map(|(key, value)| (key.as_str().to_owned(), value_view(value)))
            .collect(),
    }
}

/// A running subscription. `cursor` is the last sequence number already sent.
pub struct TracePlan {
    dir: std::path::PathBuf,
    stages: Vec<TraceStage>,
    cursor: u64,
}

/// Validate `trace.subscribe` and pin its starting point.
pub fn plan_subscription(
    state: &AppState,
    params: TraceSubscribeParams,
) -> Result<TracePlan, ObservabilityError> {
    let dir = printkit_facade::logs_dir(&state.data_dir);
    let earliest = printkit_facade::earliest_seq(&dir)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    let latest = printkit_facade::max_seq_in_dir(&dir).unwrap_or(0);
    let cursor = match params.since {
        Some(since) => {
            if earliest.is_some_and(|earliest| since < earliest.saturating_sub(1)) {
                return Err(ObservabilityError::new(
                    ErrorCode::NotFound,
                    "the requested sequence number is older than the retained logs",
                ));
            }
            since
        }
        None => latest,
    };
    Ok(TracePlan {
        dir,
        stages: params.stages,
        cursor,
    })
}

/// Queue one optional event. Returns `false` when the connection is closed;
/// a full queue drops the event and increments `dropped`.
pub(crate) fn enqueue_event(out: &Sender<String>, dropped: &AtomicU64, text: String) -> bool {
    match out.try_send(text) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

/// Stream retained events and then keep polling. The task ends when the
/// connection's receiver is dropped; a full queue drops the event and counts
/// it instead of growing memory.
pub fn start_stream(
    plan: TracePlan,
    gate: Arc<AtomicBool>,
    out: Sender<String>,
    dropped: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut cursor = plan.cursor;
        loop {
            if !gate.load(Ordering::Relaxed) {
                tokio::time::sleep(StdDuration::from_millis(LIVE_POLL_MS)).await;
                continue;
            }
            let dir = plan.dir.clone();
            let requested = cursor;
            let replay = tokio::task::spawn_blocking(move || events_since(&dir, requested)).await;
            let events = match replay {
                Ok(Ok(events)) => events,
                _ => Vec::new(),
            };
            for event in events {
                cursor = cursor.max(event.seq);
                if !plan.stages.is_empty() && !plan.stages.contains(&stage_view(event.stage)) {
                    continue;
                }
                let Some(text) = event_frame(&event) else {
                    continue;
                };
                if !enqueue_event(&out, &dropped, text) {
                    return;
                }
            }
            tokio::time::sleep(StdDuration::from_millis(LIVE_POLL_MS)).await;
        }
    })
}

fn event_frame(event: &TraceEvent) -> Option<String> {
    let mut value = serde_json::to_value(trace_view(event)).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.insert("type".to_owned(), json!("event"));
        object.insert("event".to_owned(), json!("trace"));
    }
    serde_json::to_string(&value).ok()
}

/// `trace.tail`: one trace id's retained timeline.
pub fn tail_events(
    dir: &std::path::Path,
    params: TraceTailParams,
) -> Result<TraceTailResult, ObservabilityError> {
    let events = printkit_facade::read_events(dir)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    Ok(TraceTailResult {
        events: events
            .iter()
            .filter(|event| event.trace_id.as_str() == params.trace_id)
            .map(trace_view)
            .collect(),
    })
}

/// Diagnostics bundle: retained JSONL in the requested range, the doctor
/// report and the version. Returns the data-directory-relative path.
pub fn export_bundle(
    data_dir: &std::path::Path,
    export_ttl_hours: u16,
    doctor_report: &Value,
    params: LogsExportParams,
) -> Result<LogsExportResult, ObservabilityError> {
    let (from, to) = export_range(params)?;
    let dir = printkit_facade::logs_dir(data_dir);
    let events = printkit_facade::read_events(&dir)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    let diagnostics = data_dir.join("diagnostics");
    std::fs::create_dir_all(&diagnostics)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    purge_exports(&diagnostics, export_ttl_hours);
    let name = format!(
        "printkit-diagnostics-{}.zip",
        OffsetDateTime::now_utc().unix_timestamp()
    );
    let path = diagnostics.join(&name);
    let file = std::fs::File::create(&path)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("events.jsonl", options)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    for event in events {
        if event.at < from || event.at >= to {
            continue;
        }
        let line = serde_json::to_string(&event)
            .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
        zip.write_all(line.as_bytes())
            .and_then(|()| zip.write_all(b"\n"))
            .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    }
    zip.start_file("doctor.json", options)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    let doctor = serde_json::to_vec_pretty(doctor_report)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    zip.write_all(&doctor)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    zip.start_file("version.txt", options)
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    zip.write_all(env!("CARGO_PKG_VERSION").as_bytes())
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    zip.finish()
        .map_err(|error| ObservabilityError::new(ErrorCode::JobFailed, error.to_string()))?;
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    Ok(LogsExportResult {
        path: format!("diagnostics/{name}"),
        bytes,
    })
}

fn export_range(
    params: LogsExportParams,
) -> Result<(OffsetDateTime, OffsetDateTime), ObservabilityError> {
    let now = OffsetDateTime::now_utc();
    let default_from = now.replace_time(time::Time::MIDNIGHT);
    let from = match params.from {
        Some(value) => parse_bound(&value, false)?,
        None => default_from,
    };
    let to = match params.to {
        Some(value) => parse_bound(&value, true)?,
        None => from.checked_add(Duration::days(1)).ok_or_else(|| {
            ObservabilityError::new(
                ErrorCode::Protocol,
                "date range is out of bounds".to_owned(),
            )
        })?,
    };
    Ok((from, to))
}

/// Accept RFC 3339 and `YYYY-MM-DD`. A date-only upper bound covers the whole
/// day, matching what a human means by "up to the 7th".
fn parse_bound(value: &str, end_of_day: bool) -> Result<OffsetDateTime, ObservabilityError> {
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Ok(parsed);
    }
    let date = Date::parse(
        value,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .map_err(|_| ObservabilityError::new(ErrorCode::Protocol, format!("invalid date: {value}")))?;
    let midnight = date.midnight().assume_utc();
    if end_of_day {
        midnight.checked_add(Duration::days(1)).ok_or_else(|| {
            ObservabilityError::new(ErrorCode::Protocol, "date is out of bounds".to_owned())
        })
    } else {
        Ok(midnight)
    }
}

pub(crate) fn purge_exports(dir: &std::path::Path, ttl_hours: u16) {
    let cutoff = std::time::SystemTime::now()
        - StdDuration::from_secs(u64::from(ttl_hours).saturating_mul(3600));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.modified().is_ok_and(|modified| modified < cutoff) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// --- HTTP endpoints ---------------------------------------------------------

#[handler]
pub async fn logs_events(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if !admin::admin_session(&state, req) {
        return plain_status(
            res,
            StatusCode::UNAUTHORIZED,
            "administrator session required",
        );
    }
    let trace_id = req.query::<String>("traceId");
    let stage = req
        .query::<String>("stage")
        .and_then(|value| value.parse::<TraceStage>().ok());
    let field = req.query::<String>("field");
    let since = req.query::<u64>("since").unwrap_or(0);
    let minutes = req.query::<i64>("minutes");
    let limit = req.query::<u64>("limit").unwrap_or(500).clamp(1, 2000);
    let cutoff = match minutes {
        Some(value) if !(0..=525_600).contains(&value) => {
            return plain_status(
                res,
                StatusCode::BAD_REQUEST,
                "minutes must be between 0 and 525600",
            );
        }
        Some(value) => Some(
            OffsetDateTime::now_utc()
                .checked_sub(Duration::minutes(value))
                .unwrap_or(OffsetDateTime::now_utc()),
        ),
        None => None,
    };
    let dir = printkit_facade::logs_dir(&state.data_dir);
    let events = match events_since(&dir, since) {
        Ok(events) => events,
        Err(error) => {
            return plain_status(res, StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
    };
    let mut views: Vec<TraceEventView> = events
        .iter()
        .filter(|event| {
            trace_id
                .as_deref()
                .is_none_or(|id| event.trace_id.as_str() == id)
        })
        .filter(|event| stage.is_none_or(|stage| stage_view(event.stage) == stage))
        .filter(|event| {
            field
                .as_deref()
                .is_none_or(|field| event.fields.keys().any(|key| key.as_str() == field))
        })
        .filter(|event| cutoff.is_none_or(|cutoff| event.at >= cutoff))
        .map(trace_view)
        .collect();
    let len = views.len();
    if len > usize::try_from(limit).unwrap_or(usize::MAX) {
        views.drain(..len - usize::try_from(limit).unwrap_or(len));
    }
    ok_json(res, &views);
}

#[handler]
pub async fn logs_export(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if let Err(status) = admin::check_write(&state, req, true) {
        return admin::deny(res, status);
    }
    let params = LogsExportParams {
        from: req.query::<String>("from"),
        to: req.query::<String>("to"),
    };
    match export_bundle(
        &state.data_dir,
        state.export_ttl_hours,
        &state.doctor_report,
        params,
    ) {
        Ok(result) => {
            let file = state.data_dir.join(&result.path);
            match std::fs::read(&file) {
                Ok(bytes) => {
                    res.status_code(StatusCode::OK);
                    res.headers_mut()
                        .insert("content-type", HeaderValue::from_static("application/zip"));
                    res.headers_mut().insert(
                        "content-disposition",
                        HeaderValue::from_str(&format!(
                            "attachment; filename=\"{}\"",
                            result.path.rsplit('/').next().unwrap_or("diagnostics.zip")
                        ))
                        .unwrap_or(HeaderValue::from_static("attachment")),
                    );
                    let chunks: Vec<Vec<u8>> = bytes
                        .chunks(64 * 1024)
                        .map(|chunk| chunk.to_vec())
                        .collect();
                    res.headers_mut()
                        .insert("transfer-encoding", HeaderValue::from_static("chunked"));
                    res.headers_mut().remove("content-length");
                    let mut sender = res.channel();
                    tokio::spawn(async move {
                        for chunk in chunks {
                            if sender.send_data(chunk).await.is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(error) => {
                    plain_status(res, StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
                }
            }
        }
        Err(error) => plain_status(res, StatusCode::BAD_REQUEST, &error.message),
    }
}

fn ok_json<T: serde::Serialize>(res: &mut Response, value: &T) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    let _ = res.write_body(body.into_bytes());
}

fn plain_status(res: &mut Response, status: StatusCode, message: &str) {
    let code = match status {
        StatusCode::UNAUTHORIZED => "E_AUTH",
        StatusCode::FORBIDDEN => "E_ORIGIN",
        StatusCode::BAD_REQUEST => "E_SCHEMA",
        StatusCode::NOT_FOUND => "E_NOT_FOUND",
        StatusCode::TOO_MANY_REQUESTS => "E_AUTH",
        _ => "E_JOB_FAILED",
    };
    res.status_code(status);
    res.render(Json(
        json!({ "ok": false, "error": { "code": code, "message": message } }),
    ));
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::channel;

    use super::*;

    #[test]
    fn a_full_event_queue_drops_and_counts_instead_of_blocking() {
        let (tx, mut rx) = channel::<String>(1);
        let dropped = AtomicU64::new(0);
        assert!(enqueue_event(&tx, &dropped, "first".to_owned()));
        assert!(enqueue_event(&tx, &dropped, "second".to_owned()));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_recv().as_deref(), Ok("first"));
        drop(rx);
        assert!(!enqueue_event(&tx, &dropped, "third".to_owned()));
    }

    #[test]
    fn dates_outside_the_time_range_are_refused_instead_of_panicking() {
        assert!(parse_bound("2026-11-11", false).is_ok());
        assert!(parse_bound("9999-12-31", true).is_err());
    }
}
