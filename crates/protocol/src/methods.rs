//! Method parameters, results and the frozen method table.
//!
//! The table lives in `protocol-v1.json` so a third party can generate a client
//! without reading this file. `params` and `result` name schema definitions;
//! `[]` marks an array and `Empty` marks the empty result object.

use printkit_core::Document;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::envelope::Scope;

/// `printers.papers` parameters.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct PrintersPapersParams {
    /// Printer name to list paper sizes for.
    pub name: String,
}

/// Handle of one job.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    /// Job handle returned by a submit method (`jobId`).
    pub job_id: String,
}

/// `doc.printBatch` parameters.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct DocPrintBatchParams {
    /// Documents to merge and print as one spooler job; 1 to 100 entries.
    #[schemars(length(min = 1, max = printkit_core::MAX_BATCH_DOCS))]
    pub docs: Vec<Document>,
    /// Printer name; omitted means the system default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Copies of the merged job at the spooler level, 1 to 999; omitted means
    /// one. Every batch part must set `output.copies` to 1; a per-document
    /// count is refused with `E_SCHEMA` so copies are never multiplied twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = printkit_core::MIN_COPIES, max = printkit_core::MAX_COPIES))]
    pub copies: Option<u32>,
    /// Client-chosen idempotency key (10-minute window).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_id: Option<String>,
    /// Collate copies as complete sets; omitted means `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collate: Option<bool>,
}

/// One finished PDF in `doc.printPdfBatch`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PdfBatchPart {
    /// Complete PDF, base64-encoded.
    pub bytes: String,
    /// Copies for this part; only 1 is accepted, the batch copies are the
    /// spooler count.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub copies: Option<u32>,
}

/// `doc.printPdfBatch` parameters: finished PDFs, one spooler job.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct DocPrintPdfBatchParams {
    /// Finished PDFs, in print order; 1 to 100 entries.
    #[schemars(length(min = 1, max = printkit_core::MAX_BATCH_DOCS))]
    pub pdfs: Vec<PdfBatchPart>,
    /// Printer name; omitted means the system default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Copies of the merged job at the spooler level, 1 to 999; omitted means
    /// one. Each part must use copies = 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = printkit_core::MIN_COPIES, max = printkit_core::MAX_COPIES))]
    pub copies: Option<u32>,
    /// Collate copies as complete sets; omitted means `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collate: Option<bool>,
    /// Client-chosen idempotency key, honoured for 10 minutes (`requestId`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_id: Option<String>,
}

/// `doc.printPdfBatch` result.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PdfBatchResult {
    /// Job handle (`jobId`).
    pub job_id: String,
    /// Pages in the submitted PDF, copies excluded (`pages`).
    pub pages: u32,
}

/// `doc.printBatch` result.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    /// Batch job handle (`jobId`).
    pub job_id: String,
    /// Number of documents merged into the job (`docCount`).
    pub doc_count: u32,
    /// Merged page count after the batch is combined into one PDF.
    pub page_count: u32,
}

/// `doc.printPdf` parameters. `bytes` is base64 of a complete PDF.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct DocPrintPdfParams {
    /// Complete PDF, base64-encoded, including its header and trailer.
    pub bytes: String,
    /// Printer name; omitted means the system default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Copies of the whole PDF, 1 to 999; omitted means 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = printkit_core::MIN_COPIES, max = printkit_core::MAX_COPIES))]
    pub copies: Option<u32>,
    /// Client-chosen idempotency key, honoured for 10 minutes (`requestId`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_id: Option<String>,
}

/// Handle of one preview window.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WindowRef {
    /// Preview window handle returned by `doc.preview` (`windowId`).
    pub window_id: String,
}

/// `preview.pages` parameters.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PreviewPagesParams {
    /// Preview window handle (`windowId`).
    pub window_id: String,
    /// Zero-based page index inside the previewed document.
    pub page_index: u32,
    /// Raster resolution, 72 to 300 DPI.
    #[schemars(range(min = printkit_core::MIN_DPI, max = printkit_core::MAX_DPI))]
    pub dpi: u32,
}

/// `preview.pages` result: a PNG as base64.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewPagesResult {
    /// Base64 PNG of the requested page at the requested `dpi`.
    pub png: String,
}

/// `preview.close` parameters.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PreviewCloseParams {
    /// Preview window handle to close (`windowId`).
    pub window_id: String,
}

/// `doc.exportPdf` parameters. `path` is relative and must stay inside the data
/// directory's `exports/` prefix after canonicalisation.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ExportPdfParams {
    /// Document to render into the PDF.
    pub document: Document,
    /// Target path relative to the data directory's `exports/`; `.` and `..`
    /// are rejected.
    pub path: String,
}

/// `doc.exportPdf` result.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportPdfResult {
    /// The relative path as requested (`path`).
    pub path: String,
    /// Canonical absolute path of the written file on the service machine.
    pub absolute_path: String,
    /// Pages in the written PDF.
    pub pages: u32,
    /// File size in bytes.
    #[cfg_attr(feature = "typescript", ts(type = "number"))]
    pub bytes: u64,
    /// Capability URL for a browser to open the export. It is signed for this
    /// exact canonical file and expires at `expiresAt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub url: Option<String>,
    /// Unix seconds after which `url` no longer works.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional, type = "number"))]
    pub expires_at: Option<u64>,
}

/// `raw.send` parameters. `bytes` is base64 of the raw job.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RawSendParams {
    /// Printer name that accepts raw bytes.
    pub printer: String,
    /// Raw job, base64-encoded.
    pub bytes: String,
    /// Client-chosen idempotency key (`requestId`).
    #[serde(skip_serializing_if = "Option::is_none", alias = "request_id")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_id: Option<String>,
}

/// Parameters of `job.status` and `job.cancel`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct JobIdParams {
    /// Job handle (`jobId`).
    pub job_id: String,
}

/// Result of methods that return nothing.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Empty {}

/// One row of the frozen method table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodDescriptor {
    /// Wire method name, for example `doc.print`.
    pub method: &'static str,
    /// Schema name of the parameter object, or `None` for no parameters.
    pub params: Option<&'static str>,
    /// Schema name of the result, with `[]` for arrays.
    pub result: &'static str,
    /// Required scope, or `None` for the unauthenticated handshake.
    pub scope: Option<Scope>,
    /// `true` when the method may be absent from a build; such a call fails
    /// with `E_UNSUPPORTED` instead of `E_PROTOCOL`.
    pub optional: bool,

    /// Builtin plugin that owns the method, or `None` for the core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<&'static str>,
}

const PRINT: Option<Scope> = Some(Scope::Print);

/// Every method of protocol version 1, in table order.
pub const METHODS: &[MethodDescriptor] = &[
    MethodDescriptor {
        method: "hello",
        params: Some("Hello"),
        result: "Welcome",
        scope: None,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "sys.info",
        params: None,
        result: "SysInfo",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "printers.list",
        params: None,
        result: "Printer[]",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "printers.papers",
        params: Some("PrintersPapersParams"),
        result: "PaperSize[]",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.print",
        params: Some("Document"),
        result: "JobRef",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.printBatch",
        params: Some("DocPrintBatchParams"),
        result: "BatchResult",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.printPdf",
        params: Some("DocPrintPdfParams"),
        result: "JobRef",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.printPdfBatch",
        params: Some("DocPrintPdfBatchParams"),
        result: "PdfBatchResult",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.preview",
        params: Some("Document"),
        result: "WindowRef",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "preview.pages",
        params: Some("PreviewPagesParams"),
        result: "PreviewPagesResult",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "preview.close",
        params: Some("PreviewCloseParams"),
        result: "Empty",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "doc.exportPdf",
        params: Some("ExportPdfParams"),
        result: "ExportPdfResult",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "raw.send",
        params: Some("RawSendParams"),
        result: "JobRef",
        scope: Some(Scope::Raw),
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "job.status",
        params: Some("JobIdParams"),
        result: "JobStatus",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "job.cancel",
        params: Some("JobIdParams"),
        result: "Empty",
        scope: PRINT,
        optional: false,
        plugin: None,
    },
    MethodDescriptor {
        method: "trace.subscribe",
        params: Some("TraceSubscribeParams"),
        result: "Empty",
        scope: Some(Scope::Admin),
        optional: true,
        plugin: Some("observability"),
    },
    MethodDescriptor {
        method: "trace.tail",
        params: Some("TraceTailParams"),
        result: "TraceTailResult",
        scope: Some(Scope::Admin),
        optional: true,
        plugin: Some("observability"),
    },
    MethodDescriptor {
        method: "logs.export",
        params: Some("LogsExportParams"),
        result: "LogsExportResult",
        scope: Some(Scope::Admin),
        optional: true,
        plugin: Some("observability"),
    },
    MethodDescriptor {
        method: "records.list",
        params: Some("RecordsListParams"),
        result: "RecordsListResult",
        scope: PRINT,
        optional: true,
        plugin: Some("records"),
    },
    MethodDescriptor {
        method: "records.stats",
        params: Some("RecordsStatsParams"),
        result: "RecordsStatsResult",
        scope: PRINT,
        optional: true,
        plugin: Some("records"),
    },
    MethodDescriptor {
        method: "records.summary",
        params: Some("RecordsSummaryParams"),
        result: "RecordsSummaryView",
        scope: PRINT,
        optional: true,
        plugin: Some("records"),
    },
    MethodDescriptor {
        method: "records.export",
        params: Some("RecordsExportParams"),
        result: "RecordsExportResult",
        scope: PRINT,
        optional: true,
        plugin: Some("records"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observability_methods_have_the_documented_scopes() {
        for (method, scope) in [
            ("trace.subscribe", Scope::Admin),
            ("trace.tail", Scope::Admin),
            ("logs.export", Scope::Admin),
            ("records.list", Scope::Print),
            ("records.stats", Scope::Print),
            ("records.summary", Scope::Print),
            ("records.export", Scope::Print),
        ] {
            let descriptor = METHODS
                .iter()
                .find(|descriptor| descriptor.method == method)
                .unwrap_or_else(|| panic!("{method} is missing"));
            assert_eq!(descriptor.scope, Some(scope), "{method}");
        }
    }

    #[test]
    fn method_names_are_unique() {
        let mut names: Vec<&str> = METHODS.iter().map(|descriptor| descriptor.method).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn raw_send_accepts_camel_case_and_the_snake_case_alias() {
        let camel_json = serde_json::json!({"printer": "p", "bytes": "", "requestId": "k"});
        let camel: RawSendParams = serde_json::from_value(camel_json).unwrap();
        assert_eq!(camel.request_id.as_deref(), Some("k"));
        assert_eq!(
            serde_json::to_value(&camel).unwrap(),
            serde_json::json!({"printer": "p", "bytes": "", "requestId": "k"})
        );

        let snake_json = serde_json::json!({"printer": "p", "bytes": "", "request_id": "k"});
        let snake: RawSendParams = serde_json::from_value(snake_json).unwrap();
        assert_eq!(snake.request_id.as_deref(), Some("k"));

        assert!(
            serde_json::from_value::<RawSendParams>(
                serde_json::json!({"printer": "p", "bytes": "", "unknown": 1})
            )
            .is_err(),
            "unknown fields must still be rejected"
        );
    }
}
