//! JSON Schema generation for the two published contracts.
//!
//! Both documents are generated from the Rust types, so validation rules and
//! the published schema cannot drift: limits come from `printkit-core`, method
//! parameters and results from `printkit-protocol`. A third party writes a
//! client against these files without reading Rust.

#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use printkit_core::Document;
use printkit_protocol::{
    BatchResult, Capabilities, DocPrintBatchParams, DocPrintPdfBatchParams, DocPrintPdfParams,
    Empty, ErrorBody, ErrorCode, ExportPdfParams, ExportPdfResult, Hello, JobEvent, JobIdParams,
    JobRef, JobStatus, LogsExportParams, LogsExportResult, METHODS, PaperSize, PdfBatchPart,
    PdfBatchResult, PreviewCloseParams, PreviewPagesParams, PreviewPagesResult, Printer,
    PrintersPapersParams, RawSendParams, RecordView, RecordsExportParams, RecordsExportResult,
    RecordsListParams, RecordsListResult, RecordsQuery, RecordsStatsParams, RecordsStatsResult,
    RecordsSummaryParams, RecordsSummaryView, Rejected, Request, Response, StatsBucketView,
    StatsGroupView, SysInfo, TraceEventView, TraceStage, TraceSubscribeParams, TraceTailParams,
    TraceTailResult, TraceValue, WarningBody, WarningCode, Welcome, WindowRef,
};
use schemars::SchemaGenerator;
use serde_json::{Map, Value, json};

/// File name of the document contract.
pub const DOCUMENT_SCHEMA_FILE: &str = "document-v1.json";
/// File name of the protocol contract.
pub const PROTOCOL_SCHEMA_FILE: &str = "protocol-v1.json";

const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";
const DOCUMENT_ID: &str = "https://printkit.dev/schema/document-v1.json";
const PROTOCOL_ID: &str = "https://printkit.dev/schema/protocol-v1.json";

/// Failure while generating or writing a schema.
///
/// Each variant names the step that failed and the file involved: creating the
/// output directory or writing a contract file, serialising a contract, or
/// locating the published contracts beside a release binary.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// Creating the output directory failed, or one contract file could not
    /// be written into it.
    #[error("cannot write {path}: {source}")]
    Io {
        /// Directory or file path that could not be written.
        path: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A generated contract could not be serialised to JSON.
    #[error("cannot serialise {name}: {source}")]
    Serialise {
        /// File name of the contract that failed, either
        /// [`DOCUMENT_SCHEMA_FILE`] or [`PROTOCOL_SCHEMA_FILE`].
        name: &'static str,
        /// Underlying serde_json error.
        #[source]
        source: serde_json::Error,
    },
    /// A release binary found no `schema/` directory beside its executable
    /// containing both contracts, so it refused to serve the contracts instead
    /// of falling back to the build machine's checkout path.
    #[error(
        "no schema directory beside {executable}; unpack the release's schema \
         archive there, or write one with `printkit schema --out DIR`"
    )]
    MissingPublished {
        /// Path of the running executable, or `this executable` when it could
        /// not be determined. The `schema/` directory must be placed beside
        /// that path.
        executable: String,
    },
}

/// The schema directory at the workspace root.
pub fn repository_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .join("schema")
}

/// Set by `scripts/release.sh`. A release binary must not claim the build
/// machine's checkout: callers get an error until `schema/` is unpacked beside
/// the executable.
const RELEASE_BUILD: bool = option_env!("PRINTKIT_RELEASE").is_some();

/// The schema directory a released binary serves to callers.
///
/// A developer build falls back to the checkout resolved at compile time. A
/// release binary only accepts `schema/` beside its executable, because the
/// compile-time path belongs to the machine that built it.
pub fn published_dir() -> Result<PathBuf, SchemaError> {
    let executable = std::env::current_exe().ok();
    let checkout = repository_dir();
    resolve_published_dir(executable.as_deref(), checkout.is_dir(), RELEASE_BUILD)
}

fn resolve_published_dir(
    executable: Option<&Path>,
    checkout_exists: bool,
    release_build: bool,
) -> Result<PathBuf, SchemaError> {
    if let Some(directory) = executable.and_then(published_dir_for) {
        return Ok(directory);
    }
    if !release_build && checkout_exists {
        return Ok(repository_dir());
    }
    Err(SchemaError::MissingPublished {
        executable: executable
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "this executable".to_owned()),
    })
}

fn published_dir_for(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?.join("schema");
    let document = directory.join(DOCUMENT_SCHEMA_FILE);
    let protocol = directory.join(PROTOCOL_SCHEMA_FILE);
    (document.is_file() && protocol.is_file()).then_some(directory)
}

/// Render both contracts as pretty JSON with a trailing newline.
pub fn render() -> Result<BTreeMap<&'static str, String>, SchemaError> {
    let mut rendered = BTreeMap::new();
    for (name, document) in [
        (DOCUMENT_SCHEMA_FILE, document_schema()),
        (PROTOCOL_SCHEMA_FILE, protocol_schema()?),
    ] {
        let mut text = serde_json::to_string_pretty(&document)
            .map_err(|source| SchemaError::Serialise { name, source })?;
        text.push('\n');
        rendered.insert(name, text);
    }
    Ok(rendered)
}

/// Write both contracts into `dir`, creating it when missing.
pub fn write_to(dir: &Path) -> Result<Vec<PathBuf>, SchemaError> {
    fs::create_dir_all(dir).map_err(|source| SchemaError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let mut written = Vec::new();
    for (name, text) in render()? {
        let path = dir.join(name);
        fs::write(&path, text).map_err(|source| SchemaError::Io {
            path: path.display().to_string(),
            source,
        })?;
        written.push(path);
    }
    Ok(written)
}

/// The document contract: one root object, every type under `$defs`.
pub fn document_schema() -> Value {
    let mut generator = SchemaGenerator::default();
    let _ = generator.subschema_for::<Document>();
    bundle(document_root(), generator.take_definitions(true))
}

/// The protocol contract: method table, envelopes and result types.
pub fn protocol_schema() -> Result<Value, SchemaError> {
    let mut generator = SchemaGenerator::default();
    for reference in [
        generator.subschema_for::<Hello>(),
        generator.subschema_for::<Welcome>(),
        generator.subschema_for::<Rejected>(),
        generator.subschema_for::<Request>(),
        generator.subschema_for::<Response>(),
        generator.subschema_for::<JobEvent>(),
        generator.subschema_for::<ErrorBody>(),
        generator.subschema_for::<WarningBody>(),
        generator.subschema_for::<ErrorCode>(),
        generator.subschema_for::<WarningCode>(),
        generator.subschema_for::<SysInfo>(),
        generator.subschema_for::<Capabilities>(),
        generator.subschema_for::<Printer>(),
        generator.subschema_for::<PaperSize>(),
        generator.subschema_for::<JobStatus>(),
        generator.subschema_for::<PrintersPapersParams>(),
        generator.subschema_for::<WindowRef>(),
        generator.subschema_for::<PreviewPagesParams>(),
        generator.subschema_for::<PreviewPagesResult>(),
        generator.subschema_for::<PreviewCloseParams>(),
        generator.subschema_for::<ExportPdfParams>(),
        generator.subschema_for::<ExportPdfResult>(),
        generator.subschema_for::<RawSendParams>(),
        generator.subschema_for::<JobIdParams>(),
        generator.subschema_for::<JobRef>(),
        generator.subschema_for::<Empty>(),
        generator.subschema_for::<DocPrintBatchParams>(),
        generator.subschema_for::<BatchResult>(),
        generator.subschema_for::<DocPrintPdfParams>(),
        generator.subschema_for::<DocPrintPdfBatchParams>(),
        generator.subschema_for::<PdfBatchPart>(),
        generator.subschema_for::<PdfBatchResult>(),
        generator.subschema_for::<Document>(),
        generator.subschema_for::<TraceStage>(),
        generator.subschema_for::<TraceValue>(),
        generator.subschema_for::<TraceEventView>(),
        generator.subschema_for::<TraceSubscribeParams>(),
        generator.subschema_for::<TraceTailParams>(),
        generator.subschema_for::<TraceTailResult>(),
        generator.subschema_for::<LogsExportParams>(),
        generator.subschema_for::<LogsExportResult>(),
        generator.subschema_for::<StatsGroupView>(),
        generator.subschema_for::<RecordsQuery>(),
        generator.subschema_for::<RecordsListParams>(),
        generator.subschema_for::<RecordView>(),
        generator.subschema_for::<RecordsListResult>(),
        generator.subschema_for::<RecordsStatsParams>(),
        generator.subschema_for::<StatsBucketView>(),
        generator.subschema_for::<RecordsStatsResult>(),
        generator.subschema_for::<RecordsExportParams>(),
        generator.subschema_for::<RecordsExportResult>(),
        generator.subschema_for::<RecordsSummaryParams>(),
        generator.subschema_for::<RecordsSummaryView>(),
    ] {
        // Calling `subschema_for` registers the definition; the returned
        // reference is not needed at the root.
        drop(reference);
    }
    let mut root = bundle(protocol_root(), generator.take_definitions(true));
    let methods = serde_json::to_value(METHODS).map_err(|source| SchemaError::Serialise {
        name: PROTOCOL_SCHEMA_FILE,
        source,
    })?;
    if let Some(object) = root.as_object_mut() {
        object.insert("methods".to_owned(), methods);
    }
    Ok(root)
}

fn document_root() -> Value {
    json!({
        "$schema": DRAFT,
        "$id": DOCUMENT_ID,
        "title": "printkit document v1",
        "description": "A print document: one or more pages of declarative items. Lengths are millimetres.",
        "$ref": "#/$defs/Document",
    })
}

fn protocol_root() -> Value {
    json!({
        "$schema": DRAFT,
        "$id": PROTOCOL_ID,
        "title": "printkit protocol v1",
        "description": "WebSocket and batch-upload protocol of printkit. Lengths are millimetres; timestamps are RFC 3339. Document bodies are validated against document-v1.json only; the Document types copied into $defs are there to make message bodies readable.",
        "protocol": printkit_protocol::PROTOCOL_VERSION,
        "errorCodes": ErrorCode::ALL.map(ErrorCode::as_str),
        "warningCodes": WarningCode::ALL.map(WarningCode::as_str),
    })
}

fn bundle(mut root: Value, definitions: Map<String, Value>) -> Value {
    if let Some(object) = root.as_object_mut() {
        object.insert("$defs".to_owned(), Value::Object(definitions));
    }
    root
}

/// Convenience for the CLI: generate into the repository's `schema/` directory.
pub fn write_repository() -> Result<Vec<PathBuf>, SchemaError> {
    write_to(&repository_dir())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use printkit_protocol::{ErrorCode, METHODS, WarningCode};

    use super::{
        DOCUMENT_SCHEMA_FILE, PROTOCOL_SCHEMA_FILE, document_schema, protocol_schema,
        published_dir_for, resolve_published_dir,
    };

    fn definition<'a>(schema: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        &schema["$defs"][name]
    }

    fn variants(schema: &serde_json::Value, name: &str) -> Vec<String> {
        schema["$defs"][name]["oneOf"]
            .as_array()
            .expect("a unit-only enum is a oneOf list")
            .iter()
            .map(|variant| variant["const"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    #[test]
    fn error_codes_cover_the_published_tables() {
        let schema = protocol_schema().unwrap();
        assert_eq!(
            variants(&schema, "ErrorCode"),
            ErrorCode::ALL.map(ErrorCode::as_str)
        );
        assert_eq!(
            variants(&schema, "WarningCode"),
            WarningCode::ALL.map(WarningCode::as_str)
        );
        assert_eq!(variants(&schema, "ErrorCode").len(), 18);
        assert_eq!(variants(&schema, "WarningCode").len(), 9);
    }

    #[test]
    fn every_method_reference_resolves() {
        let schema = protocol_schema().unwrap();
        for descriptor in METHODS {
            for reference in [descriptor.params, Some(descriptor.result)] {
                let Some(reference) = reference else {
                    continue;
                };
                let name = reference.trim_end_matches("[]");
                assert!(
                    definition(&schema, name).is_object(),
                    "{} references {name}, which is not in $defs",
                    descriptor.method
                );
            }
        }
    }

    #[test]
    fn document_limits_match_the_shared_constants() {
        let schema = document_schema();
        let pages = &definition(&schema, "Document")["properties"]["pages"];
        assert_eq!(pages["minItems"], 1);
        assert_eq!(pages["maxItems"], printkit_core::MAX_PAGES as u64);
        let items = &definition(&schema, "Page")["properties"]["items"];
        assert_eq!(items["maxItems"], printkit_core::MAX_ITEMS_PER_PAGE as u64);
        let tags = &definition(&schema, "Meta")["properties"]["tags"]["anyOf"][1];
        assert_eq!(tags["maxItems"], printkit_core::MAX_TAGS as u64);
        assert_eq!(
            tags["items"]["maxLength"],
            printkit_core::MAX_TAG_CHARS as u64
        );
    }

    #[test]
    fn published_dir_requires_both_contracts_beside_the_executable() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("schema");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(DOCUMENT_SCHEMA_FILE), "{}").unwrap();
        let executable = root.path().join("printkit");
        assert_eq!(published_dir_for(&executable), None);
        fs::write(directory.join(PROTOCOL_SCHEMA_FILE), "{}").unwrap();
        assert_eq!(published_dir_for(&executable), Some(directory));
    }

    #[test]
    fn a_release_binary_never_falls_back_to_the_build_checkout() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("printkit");
        let error = resolve_published_dir(Some(&executable), true, true)
            .expect_err("a release build must not print the checkout path");
        assert!(error.to_string().contains("schema"));
        assert_eq!(
            resolve_published_dir(Some(&executable), true, false).unwrap(),
            super::repository_dir()
        );
        assert!(resolve_published_dir(None, false, false).is_err());
    }
}
