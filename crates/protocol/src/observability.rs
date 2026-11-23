//! Observability methods: trace streaming, diagnostic export and usage
//! records.
//!
//! `trace.*` and `logs.export` are administrator views over the JSONL stream.
//! `records.*` is the usage view: a `print` token sees only its bound origin,
//! an `admin` token sees every origin. The parameter objects are shared by the
//! WebSocket methods and the HTTP pages, so the numbers cannot drift.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Statuses a usage record can carry, and the only values `status` filters
/// accept.
pub const RECORD_STATUSES: [&str; 4] = ["done", "failed", "cancelled", "pending"];

/// One stage of the fixed print timeline.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceStage {
    /// The request frame was received.
    Recv,
    /// Token and origin were authenticated.
    Auth,
    /// The document passed the schema and semantic layers.
    Validate,
    /// The job is queued and its one-based position is recorded.
    Queue,
    /// The renderer produced pixels or PDF bytes.
    Render,
    /// Bytes were handed to the spooler or the driver.
    Submit,
    /// The closing event; carries the outcome and total duration of the trace.
    Result,
}

impl TraceStage {
    /// Wire spelling, identical to the serde rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recv => "recv",
            Self::Auth => "auth",
            Self::Validate => "validate",
            Self::Queue => "queue",
            Self::Render => "render",
            Self::Submit => "submit",
            Self::Result => "result",
        }
    }
}

impl std::str::FromStr for TraceStage {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "recv" => Ok(Self::Recv),
            "auth" => Ok(Self::Auth),
            "validate" => Ok(Self::Validate),
            "queue" => Ok(Self::Queue),
            "render" => Ok(Self::Render),
            "submit" => Ok(Self::Submit),
            "result" => Ok(Self::Result),
            _ => Err(()),
        }
    }
}

/// A scalar value inside a trace event's field map.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TraceValue {
    /// A boolean field, wire JSON `true` or `false`.
    Bool(bool),
    /// An integer field, wire JSON number.
    Int(#[cfg_attr(feature = "typescript", ts(type = "number"))] i64),
    /// A floating-point field, wire JSON number.
    Float(f64),
    /// A string field.
    Str(String),
}

/// One trace event as the protocol exposes it.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceEventView {
    /// Global event sequence number; strictly increasing per process.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub seq: u64,
    /// Trace the event belongs to (`traceId`).
    pub trace_id: String,
    /// Timeline stage that produced the event.
    pub stage: TraceStage,
    /// RFC 3339 timestamp.
    pub at: String,
    /// Stage duration in milliseconds (`durMs`); absent when the stage has no
    /// measured end.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "number", optional))]
    pub dur_ms: Option<u64>,
    /// `true` when the stage succeeded.
    pub ok: bool,
    /// Code of a failure, for example `E_JOB_FAILED`; absent when `ok` is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub code: Option<String>,
    /// Free-form failure detail, for example the driver status.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub detail: Option<String>,
    /// Client origin the request came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub origin: Option<String>,
    /// Printer the event concerns.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub printer: Option<String>,
    /// Pages in the job, reported by the render and result stages.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub pages: Option<u32>,
    /// Queue position announced at the `queue` stage (`queuePos`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub queue_pos: Option<u32>,
    /// Additional scalar fields of the stage, keyed by their wire names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, TraceValue>,
}

/// `trace.subscribe` parameters. `since` is the last sequence number the
/// client saw; the subscription starts after it and replays retained files.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TraceSubscribeParams {
    /// Empty or absent streams every stage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<TraceStage>,
    /// Replay only events after this sequence number; omitted starts at the
    /// beginning of retention.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "number", optional))]
    pub since: Option<u64>,
}

/// `trace.tail` parameters.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TraceTailParams {
    /// Trace to replay (`traceId`).
    pub trace_id: String,
}

/// `trace.tail` result: the complete retained timeline of one trace.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceTailResult {
    /// Retained events ordered by `seq`.
    pub events: Vec<TraceEventView>,
}

/// `logs.export` parameters. Both bounds are RFC 3339; absent bounds select
/// the current day.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct LogsExportParams {
    /// Inclusive lower bound, RFC 3339 or `YYYY-MM-DD`; absent means UTC
    /// midnight today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Exclusive upper bound, RFC 3339 or `YYYY-MM-DD`; absent means one day
    /// after `from`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// `logs.export` result: a data-directory-relative archive path.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogsExportResult {
    /// Archive path relative to the data directory (`path`).
    pub path: String,
    /// Archive size in bytes.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub bytes: u64,
}

/// Aggregation dimension of `records.stats`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatsGroupView {
    /// Group by tag; a row with several tags counts once per tag.
    Tag,
    /// Group by template handle; rows without one land in `unnamed`.
    Template,
    /// Group by printer name; rows without one land in `default`.
    Printer,
    /// Group by local calendar day of `at`, keyed `YYYY-MM-DD`.
    Day,
}

impl StatsGroupView {
    /// Wire spelling, identical to the serde rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Template => "template",
            Self::Printer => "printer",
            Self::Day => "day",
        }
    }
}

impl std::str::FromStr for StatsGroupView {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tag" => Ok(Self::Tag),
            "template" => Ok(Self::Template),
            "printer" => Ok(Self::Printer),
            "day" => Ok(Self::Day),
            _ => Err(()),
        }
    }
}

/// Filters shared by listing, statistics and export.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RecordsQuery {
    /// Inclusive lower bound on `at`, RFC 3339 or `YYYY-MM-DD`; absent means no
    /// lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub from: Option<String>,
    /// Exclusive upper bound on `at`, RFC 3339 or `YYYY-MM-DD`; absent means no
    /// upper bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub to: Option<String>,
    /// Rows carrying any of these tags; empty matches every row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Exact printer name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub printer: Option<String>,
    /// Exact status: `done`, `failed`, `cancelled` or `pending`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub status: Option<RecordStatus>,
}

/// The closed set of record statuses a query may name.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    /// A successful print request.
    Done,
    /// A failed request.
    Failed,
    /// A cancelled request.
    Cancelled,
    /// Still running or waiting.
    Pending,
}

impl RecordStatus {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Pending => "pending",
        }
    }
}

impl std::str::FromStr for RecordStatus {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "pending" => Ok(Self::Pending),
            _ => Err(()),
        }
    }
}

/// Parameters of `records.list`.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RecordsListParams {
    /// Filters applied to the listing.
    #[serde(flatten)]
    #[cfg_attr(feature = "typescript", ts(flatten))]
    pub query: RecordsQuery,
    /// Last id of the previous page; rows with a smaller id come next.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "number", optional))]
    pub cursor: Option<i64>,
    /// Page size, 1 to 500; omitted means 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One usage row.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordView {
    /// Row id, also the value used as the next page `cursor`.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub id: i64,
    /// Trace sequence number of the closing event.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub seq: u64,
    /// RFC 3339 timestamp.
    pub at: String,
    /// Client origin the record was printed from.
    pub origin: String,
    /// Template id when the job came from a stored template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Tags recorded with the job; empty when it had none.
    pub tags: Vec<String>,
    /// Printer name the job was sent to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Pages in the submitted document, copies excluded.
    pub pages: u32,
    /// Copies requested for the job.
    pub copies: u32,
    /// Print outcome: `done`, `failed`, `cancelled` or `pending`.
    pub status: String,
    /// End-to-end duration in milliseconds (`durMs`).
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub dur_ms: u64,
    /// Code of a failed job (`errorCode`), for example `E_JOB_FAILED`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Trace to inspect with `trace.tail` (`traceId`).
    pub trace_id: String,
}

/// `records.list` result.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordsListResult {
    /// Rows in `id`-descending order.
    pub records: Vec<RecordView>,
    /// Cursor for the next page; absent when the page is the last one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "number", optional))]
    pub next_cursor: Option<i64>,
}

/// `records.stats` parameters.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RecordsStatsParams {
    /// Dimension to aggregate by (`groupBy`).
    pub group_by: StatsGroupView,
    /// Filters shared with listing and export.
    #[serde(flatten)]
    #[cfg_attr(feature = "typescript", ts(flatten))]
    pub query: RecordsQuery,
}

/// One statistics bucket. `sheets` is `SUM(pages * copies)`; `count` is rows
/// (or tag occurrences for `groupBy=tag`). They are separate bases.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatsBucketView {
    /// Bucket value: tag, template id, printer name or `YYYY-MM-DD` day.
    pub key: String,
    /// Rows in the bucket; for `tag` the number of tag occurrences.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub count: i64,
    /// Sheet count, `SUM(pages * copies)`.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub sheets: i64,
    /// Average end-to-end duration in milliseconds (`avgDurMs`).
    pub avg_dur_ms: f64,
}

/// Parameters of `records.summary`.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RecordsSummaryParams {
    /// Filters applied to the cards.
    #[serde(flatten)]
    #[cfg_attr(feature = "typescript", ts(flatten))]
    pub query: RecordsQuery,
}

/// `records.stats` result.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordsStatsResult {
    /// Buckets ordered by the grouping key.
    pub buckets: Vec<StatsBucketView>,
}

/// Parameters of `records.export`.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RecordsExportParams {
    /// Filters applied before export.
    #[serde(flatten)]
    #[cfg_attr(feature = "typescript", ts(flatten))]
    pub query: RecordsQuery,
}

/// `records.export` result: CSV text with a UTF-8 BOM.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordsExportResult {
    /// CSV text, one row per record, including the UTF-8 BOM.
    pub csv: String,
}

/// KPI cards above the records table; `today*` fields use the local calendar date.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordsSummaryView {
    /// Sheets recorded today by local calendar date across all statuses
    /// (`todaySheets`); kept for old clients.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub today_sheets: i64,
    /// Deprecated compatibility field: successful print requests are counted
    /// by `todayDone` (one per successful request). This retains
    /// `sum(pages * copies)` over today's done rows for old clients.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub today_done_sheets: i64,
    /// Count of today's `status = done` rows (`todayDone`).
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub today_done: i64,
    /// Count of today's `status = failed` rows (`todayFailed`).
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub today_failed: i64,
    /// Count of today's `status = cancelled` rows (`todayCancelled`).
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub today_cancelled: i64,
    /// Records in the query window, including failures.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub total: i64,
    /// Records whose status is `failed`.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub failed: i64,
    /// Records whose status is `cancelled`.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub cancelled: i64,
    /// Records whose status is `pending`.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub pending: i64,
    /// Average duration in milliseconds (`avgDurMs`) over the same rows.
    pub avg_dur_ms: f64,
    /// Printer with the highest average duration (`slowestPrinter`); absent
    /// when there are no records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slowest_printer: Option<String>,
    /// Average duration in milliseconds of `slowest_printer`; `0.0` when the
    /// field is absent.
    pub slowest_avg_dur_ms: f64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RECORD_STATUSES, RecordsQuery};

    #[test]
    fn record_statuses_are_the_documented_set() {
        assert_eq!(RECORD_STATUSES, ["done", "failed", "cancelled", "pending"]);
        assert!(!RECORD_STATUSES.contains(&"ok"));
    }

    #[test]
    fn the_query_status_is_a_closed_enum() {
        for status in RECORD_STATUSES {
            let query: RecordsQuery =
                serde_json::from_value(json!({ "status": status })).expect("a documented status");
            assert_eq!(query.status.map(|value| value.as_str()), Some(status));
        }
        assert!(serde_json::from_value::<RecordsQuery>(json!({ "status": "ok" })).is_err());
        assert!(serde_json::from_value::<RecordsQuery>(json!({ "status": "unknown" })).is_err());
    }
}
