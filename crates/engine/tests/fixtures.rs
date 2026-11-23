//! The page-spec and output fixtures the CLI acceptance run uses.
//!
//! Landscape, custom paper, copies and collate are document-level metadata, so
//! a regression here is caught without a WebView: the tests parse the same
//! files `printkit pdf` renders and, for the print path, check what reaches the
//! spooler.

use printkit_engine::{FetchPolicy, print_document_now};
use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
use printkit_trace::MemorySink;

const LANDSCAPE: &[u8] = include_bytes!("../../../tests/fixtures/landscape.json");
const CUSTOM: &[u8] = include_bytes!("../../../tests/fixtures/custom-paper.json");
const COPIES: &[u8] = include_bytes!("../../../tests/fixtures/copies.json");
const UNCOLLATED: &[u8] = include_bytes!("../../../tests/fixtures/collate-uncollated.json");

fn document(input: &[u8]) -> printkit_core::Document {
    printkit_core::parse(input).expect("the fixture parses")
}

#[test]
fn landscape_turns_a4_on_its_side() {
    let document = document(LANDSCAPE);
    assert_eq!(document.pages.len(), 1);
    let (width, height) = document.pages[0].spec.size_mm();
    assert_eq!((width, height), (297.0, 210.0));
}

#[test]
fn custom_paper_uses_its_own_millimetres() {
    let document = document(CUSTOM);
    assert_eq!(document.pages.len(), 1);
    let (width, height) = document.pages[0].spec.size_mm();
    assert_eq!((width, height), (241.0, 140.0));
}

#[test]
fn document_copies_reach_the_spooler() {
    let backend = FakePrinterBackend::new();
    let done = print_document_now(
        COPIES,
        &FakeRenderHost::new(),
        &backend,
        &MemorySink::default(),
        &FetchPolicy::default(),
        Some("fake".to_owned()),
        None,
        None,
    )
    .expect("the fixture prints");
    assert_eq!(done.copies, 3);
    let submitted = backend.submitted();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0].copies, 3);
    assert!(submitted[0].collate);
}

#[test]
fn uncollated_copies_reach_the_spooler() {
    let backend = FakePrinterBackend::new();
    let done = print_document_now(
        UNCOLLATED,
        &FakeRenderHost::new(),
        &backend,
        &MemorySink::default(),
        &FetchPolicy::default(),
        Some("fake".to_owned()),
        None,
        None,
    )
    .expect("the fixture prints");
    assert_eq!(done.copies, 3);
    let submitted = backend.submitted();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0].copies, 3);
    assert!(!submitted[0].collate);
}
