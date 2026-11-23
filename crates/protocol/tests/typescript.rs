//! Verifies that `packages/sdk-ts/src/types.ts` and `src/limits.ts` match this
//! build.
//!
//! The whole file is generated into a staging directory and compared byte for
//! byte with what is committed; `UPDATE_TS=1` rewrites it, any other run fails
//! with the first differing line. Everything the contract publishes is listed
//! here, and `packages/sdk-ts/scripts/verify.mjs` fails when a type in either
//! JSON Schema has no declaration in the generated file.
//!
//! `types.ts` leaves ts-rs through one projection step: document fields that
//! carry a serde default are optional on the wire, and the committed
//! `schema/document-v1.json` already lists them outside `required`. The
//! projection marks exactly those fields with `?` so the binding follows the
//! schema instead of the Rust struct's serialization shape.
#![cfg(feature = "typescript")]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;

use printkit_protocol::{
    BatchResult, Capabilities, CapabilityFamily, DocPrintBatchParams, DocPrintPdfBatchParams,
    DocPrintPdfParams, Empty, ErrorBody, ErrorCode, ExportPdfParams, ExportPdfResult, Hello,
    JobError, JobEvent, JobIdParams, JobRef, JobState, JobStatus, LogsExportParams,
    LogsExportResult, PaperKind, PaperSize, PdfBatchPart, PdfBatchResult, Platform,
    PreviewCloseParams, PreviewPagesParams, PreviewPagesResult, Printer, PrintersPapersParams,
    RawSendParams, RecordView, RecordsExportParams, RecordsExportResult, RecordsListParams,
    RecordsListResult, RecordsQuery, RecordsStatsParams, RecordsStatsResult, RecordsSummaryParams,
    RecordsSummaryView, Rejected, Request, Response, Scope, StatsBucketView, StatsGroupView,
    SysInfo, TraceEventView, TraceStage, TraceSubscribeParams, TraceTailParams, TraceTailResult,
    TraceValue, WarningBody, WarningCode, Welcome, WindowRef,
};
use ts_rs::TS;

#[test]
fn export_typescript_bindings() {
    // Export into a private staging directory and rename the result into place:
    // ts-rs truncates and merges its own file without a cross-process lock, so
    // two concurrent generators would interleave declarations. The rename is
    // atomic and keeps a stale half-written file from being committed.
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/sdk-ts/src");
    let staging = out.join(format!(".types-staging-{}", std::process::id()));
    std::fs::create_dir_all(&staging).unwrap();
    macro_rules! export {
        ($($ty:ty),* $(,)?) => {
            $(
                <$ty as TS>::export_all_to(&staging)
                    .unwrap_or_else(|error| panic!("cannot export {}: {error}", stringify!($ty)));
            )*
        };
    }
    // `Document` pulls in the whole document model: pages, items and units.
    export!(printkit_core::Document);
    export!(
        Hello,
        Welcome,
        Rejected,
        Request,
        Response,
        JobEvent,
        JobState,
        ErrorBody,
        WarningBody,
        ErrorCode,
        WarningCode,
        Scope,
        CapabilityFamily,
        SysInfo,
        Capabilities,
        Platform,
        Printer,
        PaperSize,
        PaperKind,
        JobStatus,
        JobError,
        PrintersPapersParams,
        JobRef,
        DocPrintBatchParams,
        BatchResult,
        DocPrintPdfBatchParams,
        PdfBatchPart,
        PdfBatchResult,
        DocPrintPdfParams,
        WindowRef,
        PreviewPagesParams,
        PreviewPagesResult,
        PreviewCloseParams,
        ExportPdfParams,
        ExportPdfResult,
        RawSendParams,
        JobIdParams,
        Empty,
        TraceStage,
        TraceValue,
        TraceEventView,
        TraceSubscribeParams,
        TraceTailParams,
        TraceTailResult,
        LogsExportParams,
        LogsExportResult,
        StatsGroupView,
        RecordsQuery,
        RecordsListParams,
        RecordView,
        RecordsListResult,
        RecordsStatsParams,
        StatsBucketView,
        RecordsStatsResult,
        RecordsSummaryParams,
        RecordsExportParams,
        RecordsExportResult,
        RecordsSummaryView,
    );
    let target = out.join("types.ts");
    let staged = staging.join("types.ts");
    let mut generated = read_text(&staged);
    project_document_optionality(&mut generated);
    sync_generated(&target, generated.as_bytes());
    export_limits(&out, &staging);
    std::fs::remove_dir_all(&staging).ok();
}

fn read_text(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).unwrap())
        .unwrap_or_else(|error| panic!("{} is not UTF-8: {error}", path.display()))
}

/// Writes the generated file when `UPDATE_TS=1`; otherwise any difference is a
/// stale binding. The staging copy makes a half-written file impossible.
fn sync_generated(target: &Path, generated: &[u8]) {
    let current = std::fs::read(target).unwrap_or_default();
    if current == generated {
        return;
    }
    if std::env::var("UPDATE_TS").as_deref() != Ok("1") {
        panic!(
            "{} is stale ({}); regenerate with UPDATE_TS=1",
            target.display(),
            first_difference(&current, generated)
        );
    }
    std::fs::write(target, generated).unwrap();
}

/// Emits `src/limits.ts` from the Rust limit constants, so an SDK can preflight
/// a document or batch against the same numbers validation uses.
fn export_limits(out: &Path, staging: &Path) {
    let text = limits_typescript();
    let staged = staging.join("limits.ts");
    std::fs::write(&staged, &text).unwrap();
    sync_generated(&out.join("limits.ts"), text.as_bytes());
}

fn limits_typescript() -> String {
    use printkit_core::limits;

    let rows: [(&str, u64, &str); 29] = [
        (
            "maxPages",
            limits::MAX_PAGES as u64,
            "Maximum pages in one document.",
        ),
        (
            "maxItemsPerPage",
            limits::MAX_ITEMS_PER_PAGE as u64,
            "Maximum items on one page.",
        ),
        (
            "maxDocumentBytes",
            limits::MAX_DOCUMENT_BYTES as u64,
            "Maximum serialised document size; above it the service answers `E_TOO_LARGE`.",
        ),
        (
            "maxAdminBodyBytes",
            limits::MAX_ADMIN_BODY_BYTES as u64,
            "Maximum body of one administrator write (document plus JSON envelope).",
        ),
        (
            "maxResourceBytes",
            limits::MAX_RESOURCE_BYTES as u64,
            "Maximum decoded size of one embedded resource (image, PDF, font).",
        ),
        (
            "maxTextChars",
            limits::MAX_TEXT_CHARS as u64,
            "Maximum characters in one text item.",
        ),
        (
            "maxTags",
            limits::MAX_TAGS as u64,
            "Maximum entries in `meta.tags`.",
        ),
        (
            "maxTagChars",
            limits::MAX_TAG_CHARS as u64,
            "Maximum characters in one tag, counted in Unicode scalar values.",
        ),
        (
            "maxTableRows",
            limits::MAX_TABLE_ROWS as u64,
            "Maximum table rows.",
        ),
        (
            "maxTableColumns",
            limits::MAX_TABLE_COLUMNS as u64,
            "Maximum table columns.",
        ),
        (
            "maxFetchBytes",
            limits::MAX_FETCH_BYTES as u64,
            "Maximum bytes fetched for one `data.url`.",
        ),
        (
            "maxFetchUrls",
            limits::MAX_FETCH_URLS as u64,
            "Maximum distinct `data.url` bodies one document may fetch.",
        ),
        (
            "maxFontBytes",
            limits::MAX_FONT_BYTES as u64,
            "Maximum decoded size of an embedded font file.",
        ),
        (
            "maxBarcodeBytes",
            limits::MAX_BARCODE_BYTES as u64,
            "Maximum bytes encoded in one barcode value.",
        ),
        (
            "minCopies",
            u64::from(limits::MIN_COPIES),
            "Lowest accepted `copies`.",
        ),
        (
            "maxCopies",
            u64::from(limits::MAX_COPIES),
            "Highest accepted `copies`.",
        ),
        (
            "minDpi",
            u64::from(limits::MIN_DPI),
            "Lowest accepted preview DPI.",
        ),
        (
            "maxDpi",
            u64::from(limits::MAX_DPI),
            "Highest accepted preview DPI.",
        ),
        (
            "maxBatchDocs",
            limits::MAX_BATCH_DOCS as u64,
            "Maximum documents in one batch call.",
        ),
        (
            "maxBatchPages",
            limits::MAX_BATCH_PAGES as u64,
            "Maximum pages across one batch.",
        ),
        (
            "maxBatchPdfBytes",
            limits::MAX_BATCH_PDF_BYTES as u64,
            "Maximum combined PDF size of one batch.",
        ),
        (
            "maxWsMessageBytes",
            limits::MAX_WS_MESSAGE_BYTES as u64,
            "Maximum bytes of one WebSocket message.",
        ),
        (
            "maxWsPdfBytes",
            limits::MAX_WS_PDF_BYTES as u64,
            "Maximum decoded PDF bytes sent through the WebSocket channel.",
        ),
        (
            "maxWsBase64Bytes",
            limits::MAX_WS_BASE64_BYTES as u64,
            "Maximum base64 text carried by one WebSocket message.",
        ),
        (
            "maxRawBytes",
            limits::MAX_RAW_BYTES as u64,
            "Maximum raw bytes in one `raw.send`.",
        ),
        (
            "maxBatchSegmentBytes",
            limits::MAX_BATCH_SEGMENT_BYTES as u64,
            "Maximum size of one multipart segment of `POST /print/batch`.",
        ),
        (
            "maxBatchRequestBytes",
            limits::MAX_BATCH_REQUEST_BYTES as u64,
            "Maximum total size of `POST /print/batch`.",
        ),
        (
            "maxGridCells",
            u64::from(limits::MAX_GRID_CELLS),
            "Maximum cells in one `tianGrid` (`rows * cols`).",
        ),
        (
            "documentVersion",
            u64::from(printkit_core::DOCUMENT_VERSION),
            "The only document `version` this build accepts.",
        ),
    ];

    let mut text = String::from(
        "// This file was generated by crates/protocol/tests/typescript.rs from\n\
         // crates/core/src/limits.rs. Do not edit it manually; run\n\
         // `UPDATE_TS=1 cargo test -p printkit-protocol --features typescript`.\n\
         \n\
         /**\n\
          * Hard limits shared by validation, the published JSON Schema and the\n\
          * service. Preflight against these instead of waiting for `E_TOO_LARGE`.\n\
          */\n\
         export const LIMITS = Object.freeze({\n",
    );
    for (name, value, doc) in rows {
        text.push_str(&format!("  /** {doc} */\n  {name}: {value},\n"));
    }
    text.push_str(
        "} as const);\n\
         \n\
         /** One key of {@link LIMITS}. */\n\
         export type LimitName = keyof typeof LIMITS;\n",
    );
    text
}

/// Applies `schema/document-v1.json`'s `required` lists to the ts-rs output.
fn project_document_optionality(text: &mut String) {
    let schema_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/document-v1.json");
    let schema_text = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", schema_path.display()));
    let schema: serde_json::Value = serde_json::from_str(&schema_text)
        .unwrap_or_else(|error| panic!("cannot parse {}: {error}", schema_path.display()));
    apply_document_optionality(text, &schema);
    // ts-rs leaves a space after the comma that starts a multi-line object;
    // the binding is committed, so keep `git diff --check` clean.
    *text = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
}

fn apply_document_optionality(text: &mut String, schema: &serde_json::Value) {
    use std::collections::BTreeSet;

    let defs = schema
        .get("$defs")
        .and_then(serde_json::Value::as_object)
        .unwrap_or_else(|| panic!("the document schema has no $defs"));
    let mut edits = Vec::new();
    for (name, definition) in defs {
        let Some(definition) = definition.as_object() else {
            continue;
        };
        let is_object =
            definition.get("type").and_then(serde_json::Value::as_str) == Some("object");
        if !is_object && definition.get("oneOf").is_none() {
            continue;
        }
        let marker = format!("export type {name} = ");
        let Some(start) = text.find(&marker) else {
            continue;
        };
        let block_end = text[start + marker.len()..]
            .find("\nexport type ")
            .map_or(text.len(), |offset| start + marker.len() + offset);
        let block = &text[start..block_end];
        let properties = schema_properties(definition);
        for group in object_groups(block) {
            let group_text = &block[group.start..group.end];
            let required: BTreeSet<String> = required_names(definition, group_text);
            for field in group_fields(group_text) {
                if !field.has_question
                    && properties.contains(&field.name)
                    && !required.contains(&field.name)
                {
                    edits.push((start + group.start + field.name_end, field.name));
                }
            }
        }
    }

    edits.sort_by_key(|(position, _)| *position);
    edits.dedup_by_key(|(position, _)| *position);
    for (position, _) in edits.into_iter().rev() {
        text.insert(position, '?');
    }
}

/// Every property named anywhere by the definition (all `oneOf` variants).
fn schema_properties(
    definition: &serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeSet<String> {
    let mut properties = std::collections::BTreeSet::new();
    if let Some(object) = definition
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        properties.extend(object.keys().cloned());
    }
    if let Some(variants) = definition
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
    {
        for variant in variants {
            if let Some(object) = variant
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                properties.extend(object.keys().cloned());
            }
        }
    }
    properties
}

/// Required names of the variant a generated object literal describes. A
/// `oneOf` group carries its discriminator as `"type": "<const>"`.
fn required_names(
    definition: &serde_json::Map<String, serde_json::Value>,
    group: &str,
) -> std::collections::BTreeSet<String> {
    if let Some(variants) = definition
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
    {
        if let Some(kind) = group_discriminator(group) {
            for variant in variants {
                let matches = variant
                    .pointer("/properties/type/const")
                    .and_then(serde_json::Value::as_str)
                    == Some(kind.as_str());
                if matches {
                    return variant
                        .as_object()
                        .map_or_else(std::collections::BTreeSet::new, required_set);
                }
            }
        }
        let mut intersection: Option<std::collections::BTreeSet<String>> = None;
        for variant in variants {
            let Some(variant) = variant.as_object() else {
                continue;
            };
            let required = required_set(variant);
            intersection = Some(match intersection {
                Some(current) => current.intersection(&required).cloned().collect(),
                None => required,
            });
        }
        return intersection.unwrap_or_default();
    }
    required_set(definition)
}

fn required_set(
    definition: &serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeSet<String> {
    definition
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Top-level `{ ... }` groups of one declaration; a `oneOf` type has several.
fn object_groups(block: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = block.as_bytes();
    let mut groups = Vec::new();
    let mut depth = 0_usize;
    let mut opened = None;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index);
            }
            quote @ (b'"' | b'\'' | b'`') => {
                index = skip_string(bytes, index, quote);
            }
            b'{' => {
                depth += 1;
                if depth == 1 {
                    opened = Some(index);
                }
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(open) = opened.take()
                {
                    groups.push(open..index + 1);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    groups
}

struct GeneratedField {
    name: String,
    /// Byte offset just after the name (and its closing quote).
    name_end: usize,
    has_question: bool,
    value_start: usize,
}

/// Fields at the first nesting level of an object literal. Commas inside
/// comments, strings or nested types are not field separators.
fn group_fields(group: &str) -> Vec<GeneratedField> {
    let bytes = group.as_bytes();
    let mut candidates = Vec::new();
    let mut depth = 0_usize;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index);
            }
            quote @ (b'"' | b'\'' | b'`') => {
                index = skip_string(bytes, index, quote);
            }
            b'{' => {
                depth += 1;
                if depth == 1 {
                    candidates.push(index + 1);
                }
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b',' if depth == 1 => {
                candidates.push(index + 1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    let mut fields = Vec::new();
    for candidate in candidates {
        let start = skip_trivia(bytes, candidate);
        if let Some(field) = parse_field(bytes, start) {
            fields.push(field);
        }
    }
    fields
}

fn parse_field(bytes: &[u8], start: usize) -> Option<GeneratedField> {
    let (name, name_end) = if bytes.get(start) == Some(&b'"') {
        let close = start + 1 + bytes[start + 1..].iter().position(|byte| *byte == b'"')?;
        (
            std::str::from_utf8(&bytes[start + 1..close])
                .ok()?
                .to_owned(),
            close + 1,
        )
    } else {
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'$')
        {
            end += 1;
        }
        if end == start {
            return None;
        }
        (
            std::str::from_utf8(&bytes[start..end]).ok()?.to_owned(),
            end,
        )
    };
    let mut cursor = name_end;
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    let has_question = bytes.get(cursor) == Some(&b'?');
    if has_question {
        cursor += 1;
    }
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b':') {
        return None;
    }
    Some(GeneratedField {
        name,
        name_end,
        has_question,
        value_start: cursor + 1,
    })
}

/// The `"type"` discriminator of a generated `oneOf` object literal.
fn group_discriminator(group: &str) -> Option<String> {
    let field = group_fields(group)
        .into_iter()
        .find(|field| field.name == "type")?;
    let bytes = group.as_bytes();
    let mut index = field.value_start;
    while index < bytes.len() && bytes[index] == b' ' {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    let close = index + 1 + bytes[index + 1..].iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&bytes[index + 1..close])
        .ok()
        .map(str::to_owned)
}

fn skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        match bytes.get(index) {
            Some(b'/') if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(bytes, index);
            }
            Some(b'/') if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index);
            }
            _ => return index,
        }
    }
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset + 1)
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    bytes[start + 2..]
        .windows(2)
        .position(|window| window == b"*/")
        .map_or(bytes.len(), |offset| start + 2 + offset + 2)
}

fn skip_string(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

#[test]
fn projection_marks_defaulted_document_fields_optional() {
    let mut text = String::from(
        "export type Doc = { a: number, b: string, };\n\
         export type Union = { \"type\": \"x\", id: string, style: TextStyle, } | \
         { \"type\": \"y\", id: string, style: TextStyle, };\n",
    );
    let schema = serde_json::json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": { "a": {}, "b": {} },
                "required": ["a"]
            },
            "Union": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": { "type": {}, "id": {}, "style": {} },
                        "required": ["type", "id"]
                    },
                    {
                        "type": "object",
                        "properties": { "type": {}, "id": {}, "style": {} },
                        "required": ["type", "id"]
                    }
                ]
            }
        }
    });
    apply_document_optionality(&mut text, &schema);
    assert!(text.contains("a: number, b?: string"));
    assert!(text.contains("\"type\": \"x\", id: string, style?: TextStyle"));
    assert!(text.contains("\"type\": \"y\", id: string, style?: TextStyle"));

    let once = text.clone();
    apply_document_optionality(&mut text, &schema);
    assert_eq!(text, once, "the projection must be idempotent");
}

/// Name the first differing byte so a stale binding is a one-line diagnosis.
fn first_difference(current: &[u8], generated: &[u8]) -> String {
    let byte = current
        .iter()
        .zip(generated)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| current.len().min(generated.len()));
    let line = current[..byte.min(current.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    format!(
        "first difference at byte {byte}, line {line}:\n  current:   {}\n  generated: {}",
        line_at(current, line),
        line_at(generated, line)
    )
}

fn line_at(bytes: &[u8], number: usize) -> String {
    bytes
        .split(|byte| *byte == b'\n')
        .nth(number.saturating_sub(1))
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .unwrap_or_default()
}
