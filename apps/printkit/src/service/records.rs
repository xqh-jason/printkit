//! Adapters from the host store to the records plugin service contract.

use std::sync::Arc;

use printkit_facade::UsageRecorder;
use printkit_plugin_api::{
    RecordPage, RecordQuery, RecordRow, RecordScope, RecordStats, RecordStore, RecordSummary,
    UsageRow, UsageSink,
};
use printkit_protocol::{
    RecordView, RecordsQuery, RecordsSummaryView, StatsBucketView, StatsGroupView,
};
use printkit_store::{NewPrintRecord, RecordFilter, StatsGroup, StatsQuery, Store};
use salvo::http::header::HeaderValue;
use salvo::prelude::*;
use serde_json::json;

use super::{AppState, admin};

/// Read-only `RecordStore` over the statistics database.
pub struct StoreRecords {
    store: Arc<Store>,
}

impl StoreRecords {
    /// Wrap the service's store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

fn filter(query: &RecordQuery) -> Result<RecordFilter, String> {
    if let Some(status) = query.status.as_deref()
        && !printkit_protocol::RECORD_STATUSES.contains(&status)
    {
        return Err(format!(
            "status must be one of {}",
            printkit_protocol::RECORD_STATUSES.join(", ")
        ));
    }
    Ok(RecordFilter {
        from: query.from.clone(),
        to: query.to.clone(),
        tags: query.tags.clone(),
        printer: query.printer.clone(),
        status: query.status.clone(),
        origin: match &query.scope {
            RecordScope::All => None,
            RecordScope::Origin(origin) => Some(origin.clone()),
        },
        cursor: query.cursor,
        limit: query.limit.clamp(1, 500),
    })
}

fn row(record: &printkit_store::PrintRecord) -> RecordRow {
    RecordRow {
        id: record.id,
        seq: u64::try_from(record.seq).unwrap_or(0),
        at: record.at.clone(),
        origin: record.origin.clone(),
        template: record.template.clone(),
        tags: record.tags.clone(),
        printer: record.printer.clone(),
        pages: record.pages,
        copies: record.copies,
        status: record.status.clone(),
        dur_ms: u64::try_from(record.dur_ms).unwrap_or(0),
        error_code: record.error_code.clone(),
        trace_id: record.trace_id.clone(),
    }
}

impl RecordStore for StoreRecords {
    fn list(&self, query: &RecordQuery) -> Result<RecordPage, String> {
        let filter = filter(query)?;
        let limit = usize::try_from(filter.limit).unwrap_or(0);
        let records = self
            .store
            .list_print_records(&filter)
            .map_err(|error| error.to_string())?;
        let next_cursor = (records.len() == limit)
            .then(|| records.last().map(|record| record.id))
            .flatten();
        Ok(RecordPage {
            records: records.iter().map(row).collect(),
            next_cursor,
        })
    }

    fn stats(&self, query: &RecordQuery, group: &str) -> Result<Vec<RecordStats>, String> {
        let group = match group {
            "tag" => StatsGroup::Tag,
            "template" => StatsGroup::Template,
            "printer" => StatsGroup::Printer,
            "day" => StatsGroup::Day,
            other => return Err(format!("unknown stats group: {other}")),
        };
        let buckets = self
            .store
            .stats(&StatsQuery {
                group,
                filter: filter(query)?,
            })
            .map_err(|error| error.to_string())?;
        Ok(buckets
            .iter()
            .map(|bucket| RecordStats {
                key: bucket.key.clone(),
                count: bucket.count,
                sheets: bucket.sheets,
                avg_dur_ms: bucket.avg_ms,
            })
            .collect())
    }

    fn summary(&self, query: &RecordQuery) -> Result<RecordSummary, String> {
        let summary = self
            .store
            .record_summary(&filter(query)?)
            .map_err(|error| error.to_string())?;
        let (slowest_printer, slowest_avg_dur_ms) = match summary.slowest_printer {
            Some((printer, avg)) => (Some(printer), avg),
            None => (None, 0.0),
        };
        Ok(RecordSummary {
            today_sheets: summary.today_sheets,
            today_done_sheets: summary.today_done_sheets,
            today_done: summary.today_done,
            today_failed: summary.today_failed,
            today_cancelled: summary.today_cancelled,
            total: summary.total,
            failed: summary.failed,
            cancelled: summary.cancelled,
            pending: summary.pending,
            avg_dur_ms: summary.avg_ms,
            slowest_printer,
            slowest_avg_dur_ms,
        })
    }

    fn csv(&self, query: &RecordQuery) -> Result<Vec<u8>, String> {
        let csv = self
            .store
            .records_csv(&filter(query)?)
            .map_err(|error| error.to_string())?;
        Ok(csv.into_bytes())
    }
}

/// `UsageSink` adapter over the engine's single queued writer.
pub struct StoreUsage {
    recorder: UsageRecorder,
}

impl StoreUsage {
    /// Wrap the service-wide writer.
    pub fn new(recorder: UsageRecorder) -> Self {
        Self { recorder }
    }
}

impl UsageSink for StoreUsage {
    fn record(&self, row: UsageRow) {
        let record = NewPrintRecord {
            trace_id: &row.trace_id,
            seq: row.seq,
            at: &row.at,
            origin: &row.origin,
            template: row.template.as_deref(),
            tags: &row.tags,
            printer: row.printer.as_deref(),
            pages: row.pages,
            copies: row.copies,
            status: &row.status,
            dur_ms: row.dur_ms,
            error_code: row.error_code.as_deref(),
        };
        self.recorder.record(&record);
    }

    fn flush(&self) {
        self.recorder.close_and_drain();
    }
}

fn records_reader(state: &AppState, req: &Request) -> bool {
    admin::admin_session(state, req)
}

fn query_filter(req: &Request) -> Result<RecordsQuery, String> {
    Ok(RecordsQuery {
        from: req.query::<String>("from"),
        to: req.query::<String>("to"),
        tags: req
            .query::<String>("tags")
            .map(|tags| {
                tags.split(',')
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        printer: req.query::<String>("printer"),
        status: req
            .query::<String>("status")
            .map(|value| {
                value.parse().map_err(|()| {
                    format!(
                        "status must be one of {}",
                        printkit_protocol::RECORD_STATUSES.join(", ")
                    )
                })
            })
            .transpose()?,
    })
}

fn plugin_query(req: &Request) -> Result<RecordQuery, String> {
    let query = query_filter(req)?;
    Ok(RecordQuery {
        scope: RecordScope::All,
        from: query.from,
        to: query.to,
        tags: query.tags,
        printer: query.printer,
        status: query.status.map(|status| status.as_str().to_owned()),
        cursor: req.query::<i64>("cursor"),
        limit: req.query::<u32>("limit").unwrap_or(100).clamp(1, 500),
    })
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
        StatusCode::BAD_REQUEST => "E_SCHEMA",
        StatusCode::NOT_FOUND => "E_NOT_FOUND",
        _ => "E_JOB_FAILED",
    };
    res.status_code(status);
    res.render(Json(
        json!({ "ok": false, "error": { "code": code, "message": message } }),
    ));
}

fn unsupported(res: &mut Response) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "ok": false,
        "error": {
            "code": "E_UNSUPPORTED",
            "message": "plugin records is disabled",
            "detail": { "plugin": "records" }
        }
    })));
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

fn bucket_view(bucket: &RecordStats) -> StatsBucketView {
    StatsBucketView {
        key: bucket.key.clone(),
        count: bucket.count,
        sheets: bucket.sheets,
        avg_dur_ms: bucket.avg_dur_ms,
    }
}

#[handler]
pub async fn list_http(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if !records_reader(&state, req) {
        return plain_status(res, StatusCode::UNAUTHORIZED, "records session required");
    }
    let Some(domain) = state.plugins.records_domain() else {
        return unsupported(res);
    };
    let query = match plugin_query(req) {
        Ok(query) => query,
        Err(message) => return plain_status(res, StatusCode::BAD_REQUEST, &message),
    };
    let limit = query.limit;
    match tokio::task::spawn_blocking(move || domain.list(&query)).await {
        Ok(Ok(page)) => {
            let next_cursor = (page.records.len() == usize::try_from(limit).unwrap_or(0))
                .then(|| page.records.last().map(|row| row.id))
                .flatten();
            ok_json(
                res,
                &json!({
                    "records": page.records.into_iter().map(row_view).collect::<Vec<_>>(),
                    "nextCursor": next_cursor,
                }),
            );
        }
        _ => plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot read records",
        ),
    }
}

#[handler]
pub async fn stats_http(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if !records_reader(&state, req) {
        return plain_status(res, StatusCode::UNAUTHORIZED, "records session required");
    }
    let Some(group) = req
        .query::<String>("groupBy")
        .and_then(|value| value.parse::<StatsGroupView>().ok())
    else {
        return plain_status(
            res,
            StatusCode::BAD_REQUEST,
            "groupBy must be tag, template, printer or day",
        );
    };
    let Some(domain) = state.plugins.records_domain() else {
        return unsupported(res);
    };
    let query = match plugin_query(req) {
        Ok(query) => query,
        Err(message) => return plain_status(res, StatusCode::BAD_REQUEST, &message),
    };
    let group = match group {
        StatsGroupView::Tag => "tag",
        StatsGroupView::Template => "template",
        StatsGroupView::Printer => "printer",
        StatsGroupView::Day => "day",
    };
    match tokio::task::spawn_blocking(move || domain.stats(&query, group)).await {
        Ok(Ok(buckets)) => {
            let views: Vec<StatsBucketView> = buckets.iter().map(bucket_view).collect();
            ok_json(res, &json!({ "buckets": views }));
        }
        _ => plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot aggregate records",
        ),
    }
}

#[handler]
pub async fn summary_http(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if !records_reader(&state, req) {
        return plain_status(res, StatusCode::UNAUTHORIZED, "records session required");
    }
    let Some(domain) = state.plugins.records_domain() else {
        return unsupported(res);
    };
    let query = match plugin_query(req) {
        Ok(query) => query,
        Err(message) => return plain_status(res, StatusCode::BAD_REQUEST, &message),
    };
    match tokio::task::spawn_blocking(move || domain.summary(&query)).await {
        Ok(Ok(summary)) => {
            let (slowest_printer, slowest_avg_dur_ms) = match summary.slowest_printer {
                Some(printer) => (Some(printer), summary.slowest_avg_dur_ms),
                None => (None, 0.0),
            };
            ok_json(
                res,
                &RecordsSummaryView {
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
                },
            );
        }
        _ => plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot summarize records",
        ),
    }
}

#[handler]
pub async fn csv_http(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "service state is missing",
        );
    };
    if !records_reader(&state, req) {
        return plain_status(res, StatusCode::UNAUTHORIZED, "records session required");
    }
    let Some(domain) = state.plugins.records_domain() else {
        return unsupported(res);
    };
    let query = match plugin_query(req) {
        Ok(query) => query,
        Err(message) => return plain_status(res, StatusCode::BAD_REQUEST, &message),
    };
    match tokio::task::spawn_blocking(move || domain.csv(&query)).await {
        Ok(Ok(csv)) => {
            res.headers_mut().insert(
                "content-type",
                HeaderValue::from_static("text/csv; charset=utf-8"),
            );
            res.headers_mut().insert(
                "content-disposition",
                HeaderValue::from_static("attachment; filename=\"print-records.csv\""),
            );
            let _ = res.write_body(csv);
        }
        _ => plain_status(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot export records",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(status: Option<&str>) -> RecordQuery {
        RecordQuery {
            scope: RecordScope::All,
            from: None,
            to: None,
            tags: Vec::new(),
            printer: None,
            status: status.map(str::to_owned),
            cursor: None,
            limit: 100,
        }
    }

    #[test]
    fn record_status_filter_accepts_only_the_documented_enum() {
        for status in printkit_protocol::RECORD_STATUSES {
            assert!(
                filter(&query(Some(status))).is_ok(),
                "{status} must be accepted"
            );
        }
        assert!(filter(&query(Some("ok"))).is_err(), "ok is not a status");
        assert!(filter(&query(None)).is_ok());
    }
}
