//! Trace events and the builder that stamps sequence numbers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::id::TraceId;
use crate::sink::Sink;

/// Fixed stage enum, matching the seven stages of a print job.
///
/// A closed enum (not a free string) is what lets the CLI filter by stage and
/// the admin page draw one fixed timeline. Adding a stage is a contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The request bytes were received; their length and digest are recorded.
    Recv,
    /// Client credentials were checked.
    Auth,
    /// The request and its document passed validation.
    Validate,
    /// The job is queued and its one-based position is recorded.
    Queue,
    /// HTML is rendered into a PDF.
    Render,
    /// The job is handed to the printer or the OS spooler.
    Submit,
    /// The job's final outcome is recorded.
    Result,
}

impl Stage {
    /// The `snake_case` wire name used in JSONL and CLI stage filters.
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

impl std::str::FromStr for Stage {
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

/// Keys allowed inside [`TraceEvent::fields`].
///
/// Document text never enters a trace event; only metadata such as a SHA-256
/// digest or a byte count is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKey {
    /// Time spent serialising the template into HTML, in milliseconds.
    HtmlMs,
    /// Time spent converting the rendered HTML into a PDF, in milliseconds.
    PdfMs,
    /// Time spent merging embedded PDF pages into the final document, in
    /// milliseconds.
    MergeMs,
    /// Time spent fetching remote assets referenced by the document, in
    /// milliseconds.
    FetchMs,
    /// Lowercase hex SHA-256 digest of the input request body.
    DocumentSha256,
    /// Byte length of the document as seen at the stage: the request body on
    /// `recv`, the produced PDF later.
    Bytes,
    /// Trace events dropped by the sink since the last written event.
    Dropped,
    /// Field values rejected by the whitelist since the last written event.
    DroppedFields,
    /// Version string of the webview engine used for rendering.
    WebviewVersion,
    /// Identifier the OS spooler assigned to the submitted job.
    SpoolerJob,
}

impl FieldKey {
    /// The `snake_case` wire name; the `FromStr` implementation accepts it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HtmlMs => "html_ms",
            Self::PdfMs => "pdf_ms",
            Self::MergeMs => "merge_ms",
            Self::FetchMs => "fetch_ms",
            Self::DocumentSha256 => "document_sha256",
            Self::Bytes => "bytes",
            Self::Dropped => "dropped",
            Self::DroppedFields => "dropped_fields",
            Self::WebviewVersion => "webview_version",
            Self::SpoolerJob => "spooler_job",
        }
    }
}

impl std::str::FromStr for FieldKey {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "html_ms" => Ok(Self::HtmlMs),
            "pdf_ms" => Ok(Self::PdfMs),
            "merge_ms" => Ok(Self::MergeMs),
            "fetch_ms" => Ok(Self::FetchMs),
            "document_sha256" => Ok(Self::DocumentSha256),
            "bytes" => Ok(Self::Bytes),
            "dropped" => Ok(Self::Dropped),
            "dropped_fields" => Ok(Self::DroppedFields),
            "webview_version" => Ok(Self::WebviewVersion),
            "spooler_job" => Ok(Self::SpoolerJob),
            _ => Err(()),
        }
    }
}

/// A scalar field value. Strings are capped at [`Scalar::MAX_STR_CHARS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    /// A JSON boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A 64-bit floating point number.
    Float(f64),
    /// A string. Use [`Scalar::string`] to apply the length cap.
    Str(String),
}

impl Scalar {
    /// Maximum characters kept for a string value, including the ellipsis.
    pub const MAX_STR_CHARS: usize = 200;

    /// A string value, truncated at the write entrance.
    pub fn string(value: impl Into<String>) -> Self {
        Self::Str(truncate(value.into(), Self::MAX_STR_CHARS))
    }
}

/// Truncate to `max` characters, marking the cut with an ellipsis.
///
/// Trace writes are the entrance for anything that might be exported as a
/// diagnostic bundle, so the truncation happens here rather than at call sites.
pub(crate) fn truncate(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    let mut truncated: String = value.chars().take(max.saturating_sub(3)).collect();
    truncated.push_str("...");
    truncated
}

/// One JSONL record.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    /// Process-wide monotonic sequence number.
    ///
    /// Numbers from one process strictly increase, and a restart seeds the
    /// counter above the highest value already on disk, so consumers can sort a
    /// stream by `seq`. Gaps mean the sink dropped events; the next written
    /// event reports the count as [`FieldKey::Dropped`].
    pub seq: u64,
    /// Identifier shared by every event of one job.
    pub trace_id: TraceId,
    /// Stage that produced the event.
    pub stage: Stage,
    /// UTC time, encoded as an RFC 3339 string in JSONL.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Duration of the stage in milliseconds; omitted from JSONL when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    /// Whether the stage succeeded; always present in JSONL.
    pub ok: bool,
    /// Machine-readable failure code set by `EventBuilder::failed`; omitted when
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Free-form human-readable detail, truncated to [`Scalar::MAX_STR_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Client origin reported by the transport, for example a remote address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Printer the job was routed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Number of pages rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// One-based position while the job waits in a render queue; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_pos: Option<u32>,
    /// Whitelisted metadata keyed by [`FieldKey`] wire names; omitted when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<FieldKey, Scalar>,
}

/// Sequence numbers are `(floor << 22) | pid`: the floor is a process-wide
/// counter and the low bits carry the process id. Two processes running at the
/// same time therefore never pick the same number, while the events of one
/// process stay strictly increasing. A restart seeds the floor above the
/// highest number already on disk, so the stream keeps growing across runs.
const PID_BITS: u32 = 22;
const PID_MASK: u64 = (1 << PID_BITS) - 1;

static NEXT_FLOOR: AtomicU64 = AtomicU64::new(1);

impl TraceEvent {
    fn next_seq() -> u64 {
        let floor = NEXT_FLOOR.fetch_add(1, Ordering::Relaxed);
        let pid = u64::from(std::process::id()) & PID_MASK;
        (floor << PID_BITS) | pid
    }

    /// Raise the sequence floor above `seq`, for example from the highest event
    /// already on disk.
    pub fn seed_seq(seq: u64) {
        let floor = (seq >> PID_BITS) + 1;
        NEXT_FLOOR.fetch_max(floor, Ordering::Relaxed);
    }
}

/// Emits events for one job: holds the sink and the job's trace id.
pub struct Tracer<'a> {
    sink: &'a dyn Sink,
    trace_id: TraceId,
}

impl<'a> Tracer<'a> {
    /// Wraps the sink and the job's trace id. No event is written yet.
    pub fn new(sink: &'a dyn Sink, trace_id: TraceId) -> Self {
        Self { sink, trace_id }
    }

    /// The identifier every event from this tracer carries.
    pub fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    /// Starts an event for `stage`; it is written only when `EventBuilder::emit` runs.
    pub fn event(&self, stage: Stage) -> EventBuilder<'a> {
        EventBuilder {
            sink: self.sink,
            event: TraceEvent {
                seq: 0,
                trace_id: self.trace_id.clone(),
                stage,
                at: OffsetDateTime::now_utc(),
                dur_ms: None,
                ok: true,
                code: None,
                detail: None,
                origin: None,
                printer: None,
                pages: None,
                queue_pos: None,
                fields: BTreeMap::new(),
            },
        }
    }
}

/// Stamps and emits exactly one event.
pub struct EventBuilder<'a> {
    sink: &'a dyn Sink,
    event: TraceEvent,
}

impl EventBuilder<'_> {
    /// Records the measured stage duration; values beyond `u64::MAX` milliseconds
    /// saturate.
    pub fn dur_ms(mut self, duration: std::time::Duration) -> Self {
        self.event.dur_ms = Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        self
    }

    /// Marks the event as failed and stores `code`; `ok` becomes `false`.
    pub fn failed(mut self, code: &str) -> Self {
        self.event.ok = false;
        self.event.code = Some(code.to_owned());
        self
    }

    /// Records free-form detail, truncated to [`Scalar::MAX_STR_CHARS`].
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.event.detail = Some(truncate(detail.into(), Scalar::MAX_STR_CHARS));
        self
    }

    /// Origin reported by the client, when the transport carries one.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.event.origin = Some(origin.into());
        self
    }

    /// Records how many pages were rendered.
    pub fn pages(mut self, pages: u32) -> Self {
        self.event.pages = Some(pages);
        self
    }

    /// One-based position while the job waits in a render queue.
    pub fn queue_pos(mut self, position: u32) -> Self {
        self.event.queue_pos = Some(position);
        self
    }

    /// Inserts a whitelisted field, replacing any value already stored under `key`.
    pub fn field(mut self, key: FieldKey, value: Scalar) -> Self {
        self.event.fields.insert(key, value);
        self
    }

    /// Insert a field by name. A name outside [`FieldKey`] is dropped as a
    /// whole and counted, never stored: the whitelist is the first redaction
    /// rule, so an unknown key cannot smuggle a document body into the log.
    pub fn field_named(self, name: &str, value: Scalar) -> Self {
        match name.parse::<FieldKey>() {
            Ok(key) => self.field(key, value),
            Err(()) => {
                self.sink.note_dropped_field();
                self
            }
        }
    }

    /// Stamp and emit the event, returning the sequence number it received.
    ///
    /// Callers that write a usage row keep the returned number: it is what
    /// joins the statistics table back to the JSONL stream.
    pub fn emit(mut self) -> u64 {
        self.event.seq = TraceEvent::next_seq();
        self.sink.emit(&self.event);
        self.event.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::MemorySink;

    #[test]
    fn builder_stamps_increasing_sequences() {
        let sink = MemorySink::default();
        let tracer = Tracer::new(&sink, TraceId::new());
        for stage in [Stage::Recv, Stage::Validate] {
            tracer.event(stage).emit();
        }
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].stage, Stage::Recv);
        assert!(events[0].seq < events[1].seq);
        assert_eq!(events[0].trace_id, events[1].trace_id);
    }

    #[test]
    fn long_strings_are_truncated_at_the_write_entrance() {
        let value = Scalar::string("x".repeat(1000));
        let Scalar::Str(value) = value else {
            panic!("expected a string scalar");
        };
        assert_eq!(value.chars().count(), Scalar::MAX_STR_CHARS);
        assert!(value.ends_with("..."));
    }

    #[test]
    fn field_keys_have_stable_json_names() {
        let event = TraceEvent {
            seq: 1,
            trace_id: TraceId::new(),
            stage: Stage::Render,
            at: OffsetDateTime::now_utc(),
            dur_ms: Some(12),
            ok: true,
            code: None,
            detail: None,
            origin: None,
            printer: None,
            pages: Some(1),
            queue_pos: None,
            fields: BTreeMap::from([
                (FieldKey::HtmlMs, Scalar::Int(7)),
                (FieldKey::PdfMs, Scalar::Int(5)),
            ]),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["fields"]["html_ms"], 7);
        assert_eq!(json["fields"]["pdf_ms"], 5);
        assert_eq!(json["stage"], "render");
        assert!(json.get("queue_pos").is_none());
    }
}
