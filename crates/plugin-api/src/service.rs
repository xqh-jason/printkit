//! Service contracts a plugin may consume through the host.
//!
//! A plugin never opens the database or the protocol session itself. The host
//! resolves these traits from [`Requires::services`](crate::Requires) and passes
//! owned trait objects to the plugin.

use serde::{Deserialize, Serialize};

/// Who may see records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordScope {
    /// Administrator scope: every origin.
    All,
    /// One bound print origin.
    Origin(String),
}

/// Filters and pagination for record queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordQuery {
    /// Permission boundary; the adapter constructs it, the plugin forwards it.
    pub scope: RecordScope,
    /// Inclusive lower bound on `at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Exclusive upper bound on `at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Rows carrying any of these tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Exact printer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Exact status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Last id of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<i64>,
    /// Page size, 1 to 500.
    pub limit: u32,
}

/// One usage row returned to a records plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordRow {
    /// SQLite rowid.
    pub id: i64,
    /// Trace sequence number.
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub at: String,
    /// Bound print origin.
    pub origin: String,
    /// Optional template label.
    pub template: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
    /// Printer, when known.
    pub printer: Option<String>,
    /// Pages per copy.
    pub pages: u32,
    /// Copies requested.
    pub copies: u32,
    /// `ok`, `failed` or `cancelled`.
    pub status: String,
    /// Duration in milliseconds.
    pub dur_ms: u64,
    /// Error code when the job failed.
    pub error_code: Option<String>,
    /// Trace id.
    pub trace_id: String,
}

/// One page of rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordPage {
    /// Rows in descending id order.
    pub records: Vec<RecordRow>,
    /// Cursor to request the next page, when this page was full.
    pub next_cursor: Option<i64>,
}

/// One statistics bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordStats {
    /// Bucket value.
    pub key: String,
    /// Rows or tag occurrences.
    pub count: i64,
    /// `SUM(pages * copies)`.
    pub sheets: i64,
    /// Average duration in milliseconds.
    pub avg_dur_ms: f64,
}

/// KPI cards for one query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSummary {
    /// Sheets recorded today across all statuses (legacy semantics).
    pub today_sheets: i64,
    /// Deprecated compatibility field: `today_done` is the KPI.
    pub today_done_sheets: i64,
    /// Count of today's done rows.
    pub today_done: i64,
    /// Count of today's failed rows.
    pub today_failed: i64,
    /// Count of today's cancelled rows.
    pub today_cancelled: i64,
    /// Matching rows.
    pub total: i64,
    /// Failed rows.
    pub failed: i64,
    /// Cancelled rows.
    pub cancelled: i64,
    /// Pending rows.
    pub pending: i64,
    /// Average duration in milliseconds.
    pub avg_dur_ms: f64,
    /// Slowest printer, when any row exists.
    pub slowest_printer: Option<String>,
    /// Average duration of the slowest printer.
    pub slowest_avg_dur_ms: f64,
}

/// Read-only record access supplied by the host.
pub trait RecordStore: Send + Sync {
    /// List one page under `query`.
    fn list(&self, query: &RecordQuery) -> Result<RecordPage, String>;
    /// Aggregate one dimension under `query`.
    fn stats(&self, query: &RecordQuery, group: &str) -> Result<Vec<RecordStats>, String>;
    /// KPI cards under `query`.
    fn summary(&self, query: &RecordQuery) -> Result<RecordSummary, String>;
    /// CSV bytes with a UTF-8 BOM under `query`.
    fn csv(&self, query: &RecordQuery) -> Result<Vec<u8>, String>;
}

/// One row accepted by the single usage writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    /// Trace id.
    pub trace_id: String,
    /// Trace sequence number.
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub at: String,
    /// Origin recorded on the row.
    pub origin: String,
    /// Optional template label.
    pub template: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
    /// Printer, when known.
    pub printer: Option<String>,
    /// Pages per copy.
    pub pages: u32,
    /// Copies.
    pub copies: u32,
    /// `ok`, `failed` or `cancelled`.
    pub status: String,
    /// Duration in milliseconds.
    pub dur_ms: u64,
    /// Error code, when failed.
    pub error_code: Option<String>,
}

/// Non-blocking usage writer adapter.
///
/// The host owns the only writer; this trait only enqueues rows and asks for a
/// drain. It must never fail a print.
pub trait UsageSink: Send + Sync {
    /// Enqueue one row; implementations must not block.
    fn record(&self, row: UsageRow);
    /// Stop accepting rows and drain what is already queued.
    fn flush(&self);
}
