//! `printkit print`: render documents and/or send PDFs, then submit one job.
//!
//! Without `--batch` exactly one input becomes one spooler job; `-n` sets the
//! spooler copy count. A single PDF is submitted byte-for-byte. With `--batch`
//! every input (or every element of a JSON array) is merged into one PDF and
//! submitted as one job.
//!
//! A PDF input (explicit `--pdf`, a `.pdf` name, or `%PDF-` magic) skips the
//! renderer. Several inputs, and anything under `--batch`, are assembled into
//! one PDF; `--pdf` files come before `-i` inputs, each group in repetition
//! order.

use std::path::PathBuf;

use clap::Args;
use printkit_facade as facade;
use printkit_facade::limits::{MAX_COPIES, MIN_COPIES};
use serde_json::json;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct PrintArgs {
    /// Input document JSON file, or `-` for stdin. A `.pdf` file skips the
    /// renderer; repeat with `--batch` to merge it with other inputs.
    #[arg(
        short = 'i',
        long = "input",
        value_name = "FILE",
        required_unless_present = "pdf"
    )]
    pub input: Vec<PathBuf>,

    /// Existing PDF file to submit without rendering; may be repeated. A single
    /// `--pdf` without other inputs is sent byte-for-byte; in a batch it joins
    /// the merged job, before `-i` inputs.
    #[arg(long = "pdf", value_name = "FILE")]
    pub pdf: Vec<PathBuf>,

    /// Printer name; omitted means the system default.
    #[arg(short = 'p', long = "printer", value_name = "NAME")]
    pub printer: Option<String>,

    /// Copies of the whole job. For `--batch` this is the only copy count;
    /// every batch part must set `output.copies` to 1. A single document
    /// follows its `output.copies` when omitted, and a single PDF defaults to 1.
    #[arg(short = 'n', long = "copies", value_name = "COUNT")]
    pub copies: Option<u32>,

    /// Merge every input into one spooler job. A single PDF without `--batch`
    /// is sent byte-for-byte instead.
    #[arg(long)]
    pub batch: bool,
    /// Print without a `print_record` row (desktop test pages).
    #[arg(long)]
    pub no_record: bool,

    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: PrintArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    if let Some(copies) = args.copies
        && !(MIN_COPIES..=MAX_COPIES).contains(&copies)
    {
        return Err(Failure::usage(format!(
            "copies must be between {MIN_COPIES} and {MAX_COPIES}"
        )));
    }
    let fetch = facade::FetchPolicy {
        timeout_ms: config.fetch.timeout_ms.value,
        max_bytes: config.fetch.max_bytes.value,
        max_redirects: config.fetch.max_redirects.value,
        allow_private_networks: super::allow_private_fetch(),
    };
    let sink = super::sink_for(&config, super::plugin_active(&config, "observability"));
    #[cfg(debug_assertions)]
    super::test_hook_before_render();
    let host = facade::new_render_host();
    let backend = facade::new_printer_backend();

    let usage = if args.no_record {
        None
    } else {
        super::usage_recorder(&config)
    };
    let inputs = read_inputs(&args.input, &args.pdf, args.batch)?;
    if !args.batch && inputs.len() != 1 {
        return Err(Failure::usage(
            "several inputs need --batch, which merges them into one job",
        ));
    }
    if inputs
        .iter()
        .any(|input| matches!(input, InputPart::Document(_)))
    {
        let webview = facade::webview_info();
        if !webview.available {
            return Err(Failure::environment(format!(
                "{} renderer is not available",
                webview.engine
            )));
        }
    }

    let done = match (args.batch, inputs.as_slice()) {
        (false, [InputPart::Document(bytes)]) => facade::print_document_now(
            bytes,
            host.as_ref(),
            backend.as_ref(),
            sink.as_ref(),
            &fetch,
            args.printer,
            args.copies,
            usage,
        ),
        (false | true, [InputPart::Pdf(bytes)]) => facade::print_pdf_now(
            bytes::Bytes::copy_from_slice(bytes),
            backend.as_ref(),
            sink.as_ref(),
            args.printer,
            args.copies.unwrap_or(1),
            "printkit pdf".to_owned(),
            usage,
        ),
        (true, parts) => facade::print_batch_now(
            parts
                .iter()
                .map(|part| match part {
                    InputPart::Document(bytes) => facade::BatchPart::Document(bytes.clone()),
                    InputPart::Pdf(bytes) => {
                        facade::BatchPart::Pdf(bytes::Bytes::copy_from_slice(bytes))
                    }
                })
                .collect(),
            host.as_ref(),
            backend.as_ref(),
            sink.as_ref(),
            &fetch,
            args.printer,
            args.copies.unwrap_or(1),
            true,
            "printkit batch".to_owned(),
            usage,
        ),
        (false, _) => unreachable!("one non-batch input was checked above"),
    }
    .map_err(Failure::from_engine)?;

    for warning in &done.warnings {
        eprintln!("warning: {} {}", warning.code, warning.message);
    }

    if args.json {
        println!(
            "{}",
            json!({
                "traceId": done.trace_id.as_str(),
                "jobId": done.submitted.job_id,
                "printer": done.submitted.printer,
                "pages": done.pages,
                "copies": done.copies,
                "sheets": done.sheets(),
                "warnings": done
                    .warnings
                    .iter()
                    .map(|warning| json!({ "code": warning.code, "message": warning.message }))
                    .collect::<Vec<_>>(),
            })
        );
    } else if let Some(job_id) = done.submitted.job_id {
        println!(
            "submitted job {job_id} to {} ({} page x {} copies = {} sheets, trace {})",
            done.submitted.printer,
            done.pages,
            done.copies,
            done.sheets(),
            done.trace_id
        );
    } else {
        println!(
            "submitted to {} ({} page x {} copies = {} sheets, trace {}; no spooler job id)",
            done.submitted.printer,
            done.pages,
            done.copies,
            done.sheets(),
            done.trace_id
        );
    }
    Ok(())
}

#[derive(Debug)]
enum InputPart {
    Document(Vec<u8>),
    Pdf(Vec<u8>),
}

/// Classify one input: explicit `--pdf`, a `.pdf` name, or PDF magic skips the
/// renderer; everything else is a document (expanded by `--batch`).
///
/// Order is deterministic: every `--pdf` file first, then every `-i` input,
/// each list in its repetition order. A PDF that is missing, has no `%PDF-`
/// header or does not parse fails as `E_SCHEMA` before any job is reserved.
fn read_inputs(
    documents: &[PathBuf],
    pdfs: &[PathBuf],
    batch: bool,
) -> Result<Vec<InputPart>, Failure> {
    let mut inputs = Vec::new();
    for path in pdfs {
        inputs.push(InputPart::Pdf(read_pdf(path)?));
    }
    for path in documents {
        let bytes = read_file(path)?;
        if looks_like_pdf(path, &bytes) {
            validate_pdf(path, &bytes)?;
            inputs.push(InputPart::Pdf(bytes));
            continue;
        }
        if !batch {
            inputs.push(InputPart::Document(bytes));
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            Failure::io(format!("cannot read {} as JSON: {error}", path.display()))
        })?;
        match value {
            serde_json::Value::Array(documents) => {
                for document in documents {
                    inputs.push(InputPart::Document(serde_json::to_vec(&document).map_err(
                        |error| Failure::io(format!("cannot encode a document: {error}")),
                    )?));
                }
            }
            other => inputs.push(InputPart::Document(serde_json::to_vec(&other).map_err(
                |error| Failure::io(format!("cannot encode a document: {error}")),
            )?)),
        }
    }
    if inputs.is_empty() {
        return Err(Failure::usage("no documents or PDFs to print"));
    }
    Ok(inputs)
}

fn looks_like_pdf(path: &std::path::Path, bytes: &[u8]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        || bytes.starts_with(b"%PDF-")
}

/// Read a PDF input and reject it as `E_SCHEMA` unless it is a readable PDF.
fn read_pdf(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    let bytes = read_file(path)?;
    validate_pdf(path, &bytes)?;
    Ok(bytes)
}

fn validate_pdf(path: &std::path::Path, bytes: &[u8]) -> Result<(), Failure> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(Failure::schema(format!(
            "{} is not a PDF: it does not start with %PDF-",
            path.display()
        )));
    }
    let pages = printkit_pdf::page_count(bytes).map_err(|error| pdf_input_failure(path, error))?;
    if pages == 0 {
        let message = format!("{} contains no pages", path.display());
        return Err(Failure {
            code: "E_SCHEMA",
            detail: Some(serde_json::json!({
                "pointer": "/pdf/pages",
                "message": message,
            })),
            message,
            exit_code: 2,
        });
    }
    Ok(())
}

/// Invalid user-supplied PDF bytes are input errors (`E_SCHEMA`), not job
/// failures. Encrypted and structurally unsupported PDFs keep their own codes.
fn pdf_input_failure(path: &std::path::Path, error: printkit_pdf::PdfError) -> Failure {
    if matches!(
        &error,
        printkit_pdf::PdfError::Encrypted
            | printkit_pdf::PdfError::UnsupportedObject { .. }
            | printkit_pdf::PdfError::TooDeep { .. }
    ) {
        Failure::from_engine(facade::EngineError::Pdf(error))
    } else {
        Failure::schema(format!("cannot read {} as a PDF: {error}", path.display()))
    }
}

fn read_file(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    if path == std::path::Path::new("-") {
        use std::io::Read as _;
        let mut buffer = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buffer)
            .map_err(|error| Failure::schema(format!("cannot read stdin: {error}")))?;
        return Ok(buffer);
    }
    std::fs::read(path)
        .map_err(|error| Failure::schema(format!("cannot read {}: {error}", path.display())))
}

#[cfg(all(test, feature = "fake-render", target_os = "macos"))]
mod tests {
    use super::*;

    fn enable_records(dir: &std::path::Path) {
        std::fs::write(
            dir.join("plugins.toml"),
            "version = 1\nsettled = 2\npreset = \"core\"\n[[plugins]]\nid = \"records\"\nenabled = true\n",
        )
        .unwrap();
    }

    #[test]
    fn pdf_names_and_magic_are_pass_through() {
        assert!(looks_like_pdf(std::path::Path::new("job.PDF"), b""));
        assert!(looks_like_pdf(
            std::path::Path::new("job.bin"),
            b"%PDF-1.7\n"
        ));
        assert!(!looks_like_pdf(
            std::path::Path::new("job.json"),
            br#"{"version":1}"#
        ));
        assert!(!looks_like_pdf(std::path::Path::new("job"), b"%PDX-1.7"));
    }

    fn zero_page_pdf() -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.4");
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", lopdf::Object::Array(Vec::new()));
        pages.set("Count", 0i64);
        let pages_id = document.add_object(lopdf::Object::Dictionary(pages));
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference(pages_id));
        let catalog_id = document.add_object(lopdf::Object::Dictionary(catalog));
        document
            .trailer
            .set("Root", lopdf::Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn cli_print_pdf_zero_page_points_at_pdf_pages() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("empty.pdf");
        std::fs::write(&pdf, zero_page_pdf()).unwrap();
        let failure = run(
            PrintArgs {
                input: Vec::new(),
                pdf: vec![pdf],
                printer: None,
                copies: Some(1),
                batch: false,
                no_record: true,
                json: true,
            },
            CliOverrides {
                data_dir: Some(dir.path().to_path_buf()),
                log_level: Some("info".to_owned()),
                port: None,
            },
        )
        .expect_err("a zero-page PDF must fail");
        assert_eq!(failure.code, "E_SCHEMA");
        assert_eq!(
            failure
                .detail
                .as_ref()
                .and_then(|detail| detail.get("pointer")),
            Some(&serde_json::json!("/pdf/pages"))
        );
    }

    #[test]
    fn cli_print_passes_an_existing_pdf_through() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("existing.pdf");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/baseline/table-multipage.pdf"
            ),
            &pdf,
        )
        .unwrap();
        for (input, explicit, batch) in [
            (Vec::new(), vec![pdf.clone()], false),
            (vec![pdf.clone()], Vec::new(), false),
            (Vec::new(), vec![pdf.clone()], true),
        ] {
            run(
                PrintArgs {
                    input,
                    pdf: explicit,
                    printer: None,
                    copies: Some(1),
                    batch,
                    no_record: true,
                    json: true,
                },
                CliOverrides {
                    data_dir: Some(dir.path().to_path_buf()),
                    log_level: Some("info".to_owned()),
                    port: None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn cli_print_skips_usage_when_records_are_off() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("document.json");
        std::fs::write(
            &input,
            br#"{
                "version": 1,
                "pages": [{
                    "spec": {"size": "A4"},
                    "items": [{
                        "id": "title",
                        "type": "text",
                        "frame": {"x": 20, "y": 20, "width": 100, "height": 10},
                        "data": {"text": "printkit"}
                    }]
                }],
                "meta": {"templateId": "cli-test", "tags": ["acceptance"]}
            }"#,
        )
        .unwrap();
        run(
            PrintArgs {
                input: vec![input],
                pdf: Vec::new(),
                printer: None,
                copies: None,
                batch: false,
                no_record: false,
                json: true,
            },
            CliOverrides {
                data_dir: Some(dir.path().to_path_buf()),
                log_level: Some("info".to_owned()),
                port: None,
            },
        )
        .unwrap();
        let store = printkit_store::Store::open(dir.path()).unwrap();
        let rows = store
            .list_print_records(&printkit_store::RecordFilter {
                limit: 10,
                ..printkit_store::RecordFilter::default()
            })
            .unwrap();
        assert!(
            rows.is_empty(),
            "records is off in the default core preset, so no row may be written"
        );
    }

    #[test]
    fn cli_print_writes_one_usage_row_when_records_are_on() {
        let dir = tempfile::tempdir().unwrap();
        enable_records(dir.path());
        let input = dir.path().join("document.json");
        std::fs::write(
            &input,
            br#"{
                "version": 1,
                "pages": [{
                    "spec": {"size": "A4"},
                    "items": [{
                        "id": "title",
                        "type": "text",
                        "frame": {"x": 20, "y": 20, "width": 100, "height": 10},
                        "data": {"text": "printkit"}
                    }]
                }],
                "meta": {"templateId": "cli-test", "tags": ["acceptance"]}
            }"#,
        )
        .unwrap();
        run(
            PrintArgs {
                input: vec![input],
                pdf: Vec::new(),
                printer: None,
                copies: None,
                batch: false,
                no_record: false,
                json: true,
            },
            CliOverrides {
                data_dir: Some(dir.path().to_path_buf()),
                log_level: Some("info".to_owned()),
                port: None,
            },
        )
        .unwrap();
        let store = printkit_store::Store::open(dir.path()).unwrap();
        let rows = store
            .list_print_records(&printkit_store::RecordFilter {
                limit: 10,
                ..printkit_store::RecordFilter::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        let record = &rows[0];
        assert_eq!(record.origin, "cli");
        assert_eq!(record.template.as_deref(), Some("cli-test"));
        assert_eq!(record.tags, vec!["acceptance".to_owned()]);
        assert_eq!(record.status, "done");
    }

    #[test]
    fn cli_print_no_record_skips_usage() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("document.json");
        std::fs::write(
            &input,
            br#"{
                "version": 1,
                "pages": [{
                    "spec": {"size": "A4"},
                    "items": [{
                        "id": "title",
                        "type": "text",
                        "frame": {"x": 20, "y": 20, "width": 100, "height": 10},
                        "data": {"text": "printkit"}
                    }]
                }],
                "meta": {"templateId": "cli-test", "tags": ["acceptance"]}
            }"#,
        )
        .unwrap();
        run(
            PrintArgs {
                input: vec![input],
                pdf: Vec::new(),
                printer: None,
                copies: None,
                batch: false,
                no_record: true,
                json: true,
            },
            CliOverrides {
                data_dir: Some(dir.path().to_path_buf()),
                log_level: Some("info".to_owned()),
                port: None,
            },
        )
        .unwrap();
        let store = printkit_store::Store::open(dir.path()).unwrap();
        let rows = store
            .list_print_records(&printkit_store::RecordFilter {
                limit: 10,
                ..printkit_store::RecordFilter::default()
            })
            .unwrap();
        assert!(
            rows.is_empty(),
            "a desktop test print must not touch records"
        );
    }

    #[test]
    fn mixed_pdf_and_input_keep_a_stable_order() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("existing.pdf");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/baseline/table-multipage.pdf"
            ),
            &pdf,
        )
        .unwrap();
        let second_pdf = dir.path().join("second.pdf");
        std::fs::copy(&pdf, &second_pdf).unwrap();
        let document = dir.path().join("document.json");
        std::fs::write(&document, br#"{"version":1,"pages":[]}"#).unwrap();

        // --pdf files come first, then -i inputs, each in repetition order.
        let inputs = read_inputs(&[document, pdf.clone()], &[second_pdf, pdf], false).unwrap();
        assert!(matches!(&inputs[0], InputPart::Pdf(_)));
        assert!(matches!(&inputs[1], InputPart::Pdf(_)));
        assert!(matches!(&inputs[2], InputPart::Document(_)));
        assert!(matches!(&inputs[3], InputPart::Pdf(_)));
    }

    #[test]
    fn missing_or_invalid_pdfs_are_schema_failures() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("missing.pdf");
        let error = read_inputs(&[], &[missing], false).unwrap_err();
        assert_eq!(error.code, "E_SCHEMA");
        assert_eq!(error.exit_code, 2);

        let not_pdf = dir.path().join("not-a-pdf.pdf");
        std::fs::write(&not_pdf, b"not a PDF").unwrap();
        let error = read_inputs(&[], &[not_pdf], false).unwrap_err();
        assert_eq!(error.code, "E_SCHEMA");

        let broken = dir.path().join("broken.pdf");
        std::fs::write(&broken, b"%PDF-1.4\nthis is not an object graph\n").unwrap();
        let error = read_inputs(&[], &[broken], false).unwrap_err();
        assert_eq!(error.code, "E_SCHEMA");

        // A `.pdf` name given to -i takes the same validation.
        let named = dir.path().join("named.pdf");
        std::fs::write(&named, b"still not a PDF").unwrap();
        let error = read_inputs(&[named], &[], false).unwrap_err();
        assert_eq!(error.code, "E_SCHEMA");
    }
}
