//! The embedded-PDF fixtures, exercised through the export path.
//!
//! These files are the samples the CLI acceptance run uses; keeping them in a
//! test means a regression in the merge or the preflight fails `cargo test`
//! instead of waiting for a manual run.

use lopdf::Object;
use printkit_engine::{EngineError, ExportResult, FetchPolicy, export_pdf};
use printkit_platform::test_support::FakeRenderHost;
use printkit_trace::MemorySink;

const EMBEDDED: &[u8] = include_bytes!("../../../tests/fixtures/embedded.json");
const ENCRYPTED: &[u8] = include_bytes!("../../../tests/fixtures/embedded-encrypted.json");
const UNSUPPORTED: &[u8] = include_bytes!("../../../tests/fixtures/embedded-unsupported.json");
const PAGE_OVER: &[u8] = include_bytes!("../../../tests/fixtures/embedded-page-over.json");

fn export(input: &[u8]) -> (Result<ExportResult, EngineError>, usize) {
    let sink = MemorySink::default();
    let host = FakeRenderHost::new();
    let result =
        futures::executor::block_on(export_pdf(input, &host, &sink, &FetchPolicy::default()));
    (result, host.calls())
}

#[test]
fn the_sample_merges_the_embedded_page_as_a_form_xobject() {
    let (result, calls) = export(EMBEDDED);
    let result = result.expect("the sample exports");
    assert_eq!(calls, 1);
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.code == "W_PDF_OVERLAY_TOP"),
        "{:?}",
        result.warnings
    );

    let merged = lopdf::Document::load_mem(&result.pdf).expect("the output parses");
    let page_id = merged.get_pages()[&1];
    let page = merged.get_dictionary(page_id).expect("page");
    let resources = page
        .get(b"Resources")
        .and_then(Object::as_dict)
        .expect("page resources");
    let xobjects = resources
        .get(b"XObject")
        .and_then(Object::as_dict)
        .expect("xobject resources");
    let form = xobjects
        .iter()
        .next()
        .and_then(|(_, value)| value.as_reference().ok())
        .and_then(|id| merged.get_object(id).ok())
        .and_then(|object| object.as_stream().ok())
        .map(|stream| stream.dict.clone())
        .expect("form xobject");
    let subtype = form
        .get(b"Subtype")
        .expect("subtype")
        .as_name()
        .expect("name");
    assert_eq!(subtype, b"Form");
    assert!(
        form.get(b"Resources")
            .and_then(Object::as_dict)
            .expect("form resources")
            .get(b"Font")
            .is_ok()
    );
}

#[test]
fn the_encrypted_sample_fails_before_rendering() {
    let (result, calls) = export(ENCRYPTED);
    let error = result.expect_err("an encrypted payload is rejected");
    assert_eq!(error.code(), "E_PDF_ENCRYPTED");
    assert_eq!(calls, 0);
}

#[test]
fn the_unsupported_sample_names_the_missing_object() {
    let (result, calls) = export(UNSUPPORTED);
    let error = result.expect_err("a dangling resource is rejected");
    assert_eq!(error.code(), "E_UNSUPPORTED");
    assert_eq!(calls, 0);
    let detail = error.detail().expect("detail");
    assert_eq!(detail["violations"][0]["object"], "99 0 R");
}

#[test]
fn the_page_over_sample_fails_before_rendering() {
    let (result, calls) = export(PAGE_OVER);
    let error = result.expect_err("a page past the end is rejected");
    assert_eq!(error.code(), "E_SCHEMA");
    assert_eq!(calls, 0);
    assert_eq!(error.pointer(), Some("/pages/0/items/1/data/page"));
}
