//! `POST /print/batch`: binary PDFs and documents in one spooler job.
//!
//! The body is `multipart/mixed`. The first part is `application/json`
//! metadata (printer, copies, collate); every later part is either an
//! `application/pdf` passed through without rendering (P0) or a Document
//! JSON rendered by the engine. All parts are merged into one PDF, so the
//! spooler receives one job.
//!
//! Authentication is the WebSocket one: a print token, an optional transport
//! `Origin`, and the same origin binding. The whole request is bounded before
//! anything is written to disk; an oversized body is rejected, never spooled.

use std::sync::Arc;

use printkit_facade as facade;
use printkit_facade::limits::{
    MAX_BATCH_PDF_BYTES, MAX_BATCH_REQUEST_BYTES, MAX_BATCH_SEGMENT_BYTES, MAX_COPIES, MIN_COPIES,
};
use printkit_protocol::{ErrorCode, Hello, Scope};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::auth::{Rejection, authenticate};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BatchMeta {
    #[serde(default)]
    printer: Option<String>,
    #[serde(default)]
    copies: Option<u32>,
    /// Whether copies are collated as complete sets; defaults to `true`.
    #[serde(default)]
    collate: Option<bool>,
}

struct Part {
    content_type: String,
    start: usize,
    end: usize,
}

#[handler]
pub async fn receive(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return fail(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::JobFailed,
            "service state is missing".to_owned(),
        );
    };

    let Some(boundary) = boundary(
        req.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
    ) else {
        return fail(
            res,
            StatusCode::BAD_REQUEST,
            ErrorCode::Schema,
            "content-type must be multipart/mixed with a boundary".to_owned(),
        );
    };
    let token = bearer(req);
    let origin = req
        .headers()
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let hello = Hello {
        r#type: "hello".to_owned(),
        protocol: 1,
        sdk: "http-batch".to_owned(),
        token,
        origin: None,
    };
    let session = match authenticate(&state, &hello, origin.as_deref()).await {
        Ok(session) => session,
        Err(rejection) => {
            return reject(res, &rejection);
        }
    };
    if state.draining.load(std::sync::atomic::Ordering::SeqCst) {
        return fail(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unsupported,
            "the service is shutting down".to_owned(),
        );
    }
    if !session.has(Scope::Print) {
        return fail(
            res,
            StatusCode::UNAUTHORIZED,
            ErrorCode::Auth,
            "the token does not grant the print scope".to_owned(),
        );
    }
    let _batch_permit = match state.batch_slots.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return fail(
                res,
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::JobFailed,
                "the batch limiter is unavailable".to_owned(),
            );
        }
    };

    let idempotency_key = req
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(|value| {
            super::idempotency_key("http", "print/batch", session.origin.as_deref(), value)
        });
    let key_lock = idempotency_key.as_ref().map(|key| {
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
    let _idempotency_guard = match key_lock {
        Some(lock) => Some(lock.lock_owned().await),
        None => None,
    };
    if let Some(key) = idempotency_key.as_deref()
        && let Ok(mut cache) = state.idempotency.lock()
        && let Some(entry) = cache.get(key)
    {
        let body = json!({
            "ok": true,
            "jobId": entry.job_id,
            "docCount": entry.doc_count,
            "pageCount": entry.page_count,
            "idempotent": true,
        });
        res.status_code(StatusCode::OK);
        res.render(Json(body));
        return;
    }
    let body = match req.payload_with_max_size(MAX_BATCH_REQUEST_BYTES).await {
        Ok(body) => body.clone(),
        Err(error) => {
            return fail(
                res,
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::TooLarge,
                format!("the request exceeds {MAX_BATCH_REQUEST_BYTES} bytes: {error}"),
            );
        }
    };
    let parts = match parse_multipart(&body, &boundary) {
        Ok(parts) => parts,
        Err(message) => {
            return fail(res, StatusCode::BAD_REQUEST, ErrorCode::Schema, message);
        }
    };
    if parts.is_empty() {
        return fail(
            res,
            StatusCode::BAD_REQUEST,
            ErrorCode::Schema,
            "the multipart body has no parts".to_owned(),
        );
    }
    // One PDF (metadata + exactly one PDF part) may use the pass-through
    // ceiling; a mixed batch keeps the smaller per-segment bound.
    let single_pdf = parts.len() == 2 && parts[1].content_type == "application/pdf";
    for part in &parts {
        let limit = segment_limit(&part.content_type, single_pdf);
        if part.end - part.start > limit {
            return fail(
                res,
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::TooLarge,
                format!("one {} part exceeds {limit} bytes", part.content_type),
            );
        }
    }
    let meta_body = &body[parts[0].start..parts[0].end];
    let meta: BatchMeta = match serde_json::from_slice(meta_body) {
        Ok(meta) => meta,
        Err(error) => {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::Schema,
                format!("the first part must be application/json metadata: {error}"),
            );
        }
    };
    let copies = meta.copies.unwrap_or(1);
    if !(MIN_COPIES..=MAX_COPIES).contains(&copies) {
        return fail(
            res,
            StatusCode::BAD_REQUEST,
            ErrorCode::Schema,
            format!("copies must be between {MIN_COPIES} and {MAX_COPIES}"),
        );
    }
    let mut batch_parts = Vec::with_capacity(parts.len().saturating_sub(1));
    for part in &parts[1..] {
        if part.content_type == "application/pdf" {
            batch_parts.push(facade::BatchPart::Pdf(body.slice(part.start..part.end)));
        } else if part.content_type == "application/json" {
            batch_parts.push(facade::BatchPart::Document(
                body[part.start..part.end].to_vec(),
            ));
        } else {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::Schema,
                format!(
                    "unsupported part type {:?}; use application/json or application/pdf",
                    part.content_type
                ),
            );
        }
    }
    if batch_parts.is_empty() {
        return fail(
            res,
            StatusCode::BAD_REQUEST,
            ErrorCode::Schema,
            "the batch has no documents".to_owned(),
        );
    }

    let doc_count = u32::try_from(batch_parts.len()).unwrap_or(u32::MAX);
    // One PDF needs no rebuild: submit the bytes through the same
    // passthrough path as `doc.printPdf`/CLI `--batch`. That preserves
    // AcroForm, Outlines, Metadata and the trailer Info instead of merging a
    // single file into a new object graph.
    let single_pdf = match batch_parts.as_slice() {
        [facade::BatchPart::Pdf(bytes)] => Some(bytes.clone()),
        _ => None,
    };
    let hub = Arc::clone(&state.job_events);
    let notify_sink = Arc::clone(&state.sink);
    let origin_key = session.origin.clone().unwrap_or_default();
    let notify_origin = origin_key.clone();
    let notify: facade::JobUpdateHook = Box::new(move |job_id, update| {
        super::ws::send_job_event(&hub, notify_sink.as_ref(), &notify_origin, job_id, &update)
    });
    let usage = state.plugins.usage_recorder(&origin_key);
    let worker = Arc::clone(&state);
    let title = "printkit batch".to_owned();
    let result = tokio::task::spawn_blocking(move || {
        if let Some(pdf) = single_pdf {
            facade::begin_print_pdf(
                facade::PrintPdf {
                    pdf,
                    printer: meta.printer,
                    copies,
                    collate: meta.collate.unwrap_or(true),
                    title,
                },
                &worker.pool,
                Arc::clone(&worker.print_backend),
                &worker.print_jobs,
                worker.sink.clone(),
                notify,
                Some(origin_key),
                usage,
            )
        } else {
            facade::begin_print_batch(
                facade::PrintBatch {
                    parts: batch_parts,
                    printer: meta.printer,
                    copies,
                    collate: meta.collate.unwrap_or(true),
                    title,
                },
                &worker.pool,
                Arc::clone(&worker.print_backend),
                Arc::clone(&worker.render_host),
                &worker.print_jobs,
                worker.sink.clone(),
                worker.fetch,
                notify,
                Some(origin_key),
                usage,
            )
        }
    })
    .await;
    let reserved = match result {
        Ok(Ok(reserved)) => reserved,
        Ok(Err(error)) => {
            let status = status_for(error.code());
            return fail_engine(
                res,
                status,
                code_for(error.code()),
                error.to_string(),
                error.detail(),
            );
        }
        Err(error) => {
            return fail(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::JobFailed,
                format!("the batch validation task failed: {error}"),
            );
        }
    };
    state
        .print_jobs
        .bind_origin(&reserved.job_id, session.origin.as_deref());
    let job_id = reserved.job_id.as_str().to_owned();
    super::ws::remember_pages(&state, &job_id, reserved.pages);
    if let Some(key) = idempotency_key
        && let Ok(mut cache) = state.idempotency.lock()
    {
        cache.insert(
            key,
            super::IdempotencyEntry {
                job_id: job_id.clone(),
                doc_count,
                page_count: reserved.pages,
            },
        );
    }
    let body = json!({
        "ok": true,
        "jobId": job_id,
        "docCount": doc_count,
        "pageCount": reserved.pages,
    });
    res.status_code(StatusCode::OK);
    res.render(Json(body));
    reserved.activate();
}

fn boundary(content_type: Option<&str>) -> Option<String> {
    let value = content_type?;
    if !value.trim_start().starts_with("multipart/") {
        return None;
    }
    let boundary = value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))?
        .trim_matches('"');
    if boundary.is_empty() || boundary.len() > 70 {
        return None;
    }
    Some(boundary.to_owned())
}

/// Split a multipart body into headers and payloads. Delimiters are matched
/// only at the start of a line, so PDF bytes cannot terminate a part early.
/// Bytes one multipart segment may carry.
///
/// A single-PDF batch shares the CLI fast-path ceiling (`MAX_BATCH_PDF_BYTES`);
/// every other part stays at `MAX_BATCH_SEGMENT_BYTES`. The total request is
/// still bounded by `MAX_BATCH_REQUEST_BYTES`.
fn segment_limit(content_type: &str, single_pdf: bool) -> usize {
    if single_pdf && content_type == "application/pdf" {
        MAX_BATCH_PDF_BYTES
    } else {
        MAX_BATCH_SEGMENT_BYTES
    }
}

fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<Part>, String> {
    if boundary.is_empty() {
        return Err("the multipart boundary is empty".to_owned());
    }
    let first = format!("--{boundary}\r\n");
    let start = find(body, first.as_bytes())
        .ok_or_else(|| "the multipart opening boundary is missing".to_owned())?
        + first.len();
    let marker = format!("\r\n--{boundary}");
    let mut parts = Vec::new();
    let mut position = start;
    loop {
        let relative = find(&body[position..], marker.as_bytes())
            .ok_or_else(|| "the multipart closing boundary is missing".to_owned())?;
        let end = position + relative;
        parts.push(split_part(&body[position..end], position)?);
        let after = end + marker.len();
        if body.get(after..after + 2) == Some(b"--") {
            break;
        }
        position = after
            .checked_add(2)
            .filter(|next| *next <= body.len())
            .ok_or_else(|| "the multipart separator is truncated".to_owned())?;
    }
    Ok(parts)
}

fn split_part(segment: &[u8], base: usize) -> Result<Part, String> {
    let split = find(segment, b"\r\n\r\n")
        .ok_or_else(|| "a multipart part has no header separator".to_owned())?;
    let headers = std::str::from_utf8(&segment[..split])
        .map_err(|_| "multipart headers are not UTF-8".to_owned())?;
    let body_start = base + split + 4;
    let body_end = base + segment.len();
    let content_type = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    Ok(Part {
        content_type: content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
        start: body_start,
        end: body_end,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn bearer(req: &Request) -> Option<String> {
    if let Some(value) = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        && let Some(token) = value.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_owned());
    }
    req.headers()
        .get("x-printkit-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn reject(res: &mut Response, rejection: &Rejection) {
    let body = json!({
        "ok": false,
        "error": {
            "code": rejection.code.as_str(),
            "message": rejection.message,
            "detail": rejection.detail(),
        }
    });
    res.status_code(status_for(rejection.code.as_str()));
    res.render(Json(body));
}

fn fail(res: &mut Response, status: StatusCode, code: ErrorCode, message: String) {
    let body = json!({
        "ok": false,
        "error": { "code": code.as_str(), "message": message },
    });
    res.status_code(status);
    res.render(Json(body));
}

/// Error body for an engine rejection, including `detail` such as a pointer.
fn fail_engine(
    res: &mut Response,
    status: StatusCode,
    code: ErrorCode,
    message: String,
    detail: Option<serde_json::Value>,
) {
    let mut error = json!({
        "code": code.as_str(),
        "message": message,
    });
    if let Some(detail) = detail {
        error["detail"] = detail;
    }
    res.status_code(status);
    res.render(Json(json!({ "ok": false, "error": error })));
}

fn status_for(code: &str) -> StatusCode {
    match code {
        "E_AUTH" | "E_ORIGIN" => StatusCode::UNAUTHORIZED,
        "E_TOO_LARGE" => StatusCode::PAYLOAD_TOO_LARGE,
        "E_SCHEMA" => StatusCode::BAD_REQUEST,
        "E_PRINTER_NOT_FOUND" => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn code_for(code: &str) -> ErrorCode {
    ErrorCode::ALL
        .iter()
        .copied()
        .find(|known| known.as_str() == code)
        .unwrap_or(ErrorCode::JobFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_pdf_uses_the_passthrough_byte_ceiling() {
        assert_eq!(segment_limit("application/pdf", true), MAX_BATCH_PDF_BYTES);
        assert_eq!(
            segment_limit("application/pdf", false),
            MAX_BATCH_SEGMENT_BYTES
        );
        assert_eq!(
            segment_limit("application/json", true),
            MAX_BATCH_SEGMENT_BYTES
        );
    }

    #[test]
    fn boundary_is_required_and_bounded() {
        assert!(boundary(Some("multipart/mixed; boundary=abc")).is_some());
        let long = format!("boundary={}", "x".repeat(71));
        assert!(boundary(Some(&format!("multipart/mixed; {long}"))).is_none());
    }

    #[test]
    fn multipart_parts_report_offsets_into_the_original_body() {
        let body = b"--b\r\nContent-Type: application/json\r\n\r\n{}\r\n--b\r\nContent-Type: application/pdf\r\n\r\nPDF\r\n--b--";
        let parts = parse_multipart(body, "b").unwrap();
        assert_eq!(&body[parts[0].start..parts[0].end], b"{}");
        assert_eq!(&body[parts[1].start..parts[1].end], b"PDF");
    }
}
