#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! End-to-end tracing: one `trace_id` strings together every stage of a job.
//!
//! This is a leaf crate on purpose: `engine`, `platform-*` and the CLI all emit
//! events, so it must not depend on any of them. Types carry no IO; the JSONL
//! sink is the only writer in this crate.

mod event;
mod id;
mod read;
mod sink;

pub use event::{FieldKey, Scalar, Stage, TraceEvent, Tracer};
pub use id::TraceId;
pub use read::{earliest_seq, events_since, read_events};
pub use sink::{
    DEFAULT_RETENTION_DAYS, JsonlSink, LogLevel, MemorySink, NullSink, RotateOnOpen, Sink,
    max_seq_in_dir,
};
