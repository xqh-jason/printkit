#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! Wire types of the printkit protocol.
//!
//! Pure data: no IO, no service logic. [`ErrorCode`] and [`WarningCode`] are the
//! vocabulary every other layer reports, and [`METHODS`] is the frozen method
//! table. Input types reject unknown fields so a misspelled parameter fails
//! loudly; result types accept them so a newer engine stays compatible.

pub mod code;
pub mod envelope;
pub mod methods;
pub mod observability;
pub mod types;

pub use code::{ErrorCode, WarningCode};
pub use envelope::{
    CapabilityFamily, ErrorBody, Hello, JobEvent, JobState, PROTOCOL_VERSION, Rejected, Request,
    Response, Scope, WarningBody, Welcome,
};
pub use methods::{
    BatchResult, DocPrintBatchParams, DocPrintPdfBatchParams, DocPrintPdfParams, Empty,
    ExportPdfParams, ExportPdfResult, JobIdParams, JobRef, METHODS, MethodDescriptor, PdfBatchPart,
    PdfBatchResult, PreviewCloseParams, PreviewPagesParams, PreviewPagesResult,
    PrintersPapersParams, RawSendParams, WindowRef,
};
pub use observability::{
    LogsExportParams, LogsExportResult, RECORD_STATUSES, RecordStatus, RecordView,
    RecordsExportParams, RecordsExportResult, RecordsListParams, RecordsListResult, RecordsQuery,
    RecordsStatsParams, RecordsStatsResult, RecordsSummaryParams, RecordsSummaryView,
    StatsBucketView, StatsGroupView, TraceEventView, TraceStage, TraceSubscribeParams,
    TraceTailParams, TraceTailResult, TraceValue,
};
pub use types::{
    Capabilities, JobError, JobStatus, PaperKind, PaperSize, Platform, Printer, SysInfo,
};
