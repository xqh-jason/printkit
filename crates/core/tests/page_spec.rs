#![allow(clippy::panic, clippy::unwrap_used)]

//! Custom paper dimensions and orientation are the two places where a page spec
//! can contradict itself.

use printkit_core::{DocumentError, E_SCHEMA, Orientation, parse, validate_bytes};
use serde_json::json;

fn document(spec: serde_json::Value) -> String {
    json!({"version": 1, "pages": [{"spec": spec, "items": []}]}).to_string()
}

fn accepted(name: &str, spec: serde_json::Value) -> printkit_core::Validated {
    validate_bytes(document(spec).as_bytes()).unwrap_or_else(|error| panic!("{name}: {error:?}"))
}

#[test]
fn page_spec_custom_size_is_the_final_face() {
    let page = accepted(
        "custom",
        json!({"size": "custom", "width": 241, "height": 140}),
    );
    let spec = &page.document().pages[0].spec;
    assert_eq!(spec.size_mm(), (241.0, 140.0));
    assert_eq!(spec.effective_orientation(), Orientation::Landscape);

    let portrait = accepted(
        "custom portrait",
        json!({"size": "custom", "width": 140, "height": 241}),
    );
    assert_eq!(portrait.document().pages[0].spec.size_mm(), (140.0, 241.0));
}

#[test]
fn page_spec_landscape_rotates_named_paper() {
    let page = accepted(
        "A4 landscape",
        json!({"size": "A4", "orientation": "landscape"}),
    );
    assert_eq!(page.document().pages[0].spec.size_mm(), (297.0, 210.0));

    let portrait = accepted("A4 portrait", json!({"size": "A4"}));
    assert_eq!(portrait.document().pages[0].spec.size_mm(), (210.0, 297.0));
}

#[test]
fn page_spec_orientation_conflict_is_e_schema() {
    let error = validate_bytes(
        document(json!({
            "size": "custom",
            "width": 241,
            "height": 140,
            "orientation": "portrait",
        }))
        .as_bytes(),
    )
    .unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    match error {
        DocumentError::Semantic(violations) => {
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].pointer, "/pages/0/spec/orientation");
        }
        other => panic!("expected semantic violations, got {other:?}"),
    }
}

#[test]
fn page_spec_explicit_orientation_matching_dimensions_is_accepted() {
    let page = accepted(
        "custom landscape",
        json!({"size": "custom", "width": 241, "height": 140, "orientation": "landscape"}),
    );
    assert_eq!(page.document().pages[0].spec.size_mm(), (241.0, 140.0));
}

#[test]
fn page_spec_width_is_rejected_for_named_paper() {
    let error =
        validate_bytes(document(json!({"size": "A4", "width": 210})).as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    assert_eq!(error.pointer(), Some("/pages/0/spec/width"));
}

#[test]
fn page_spec_parses_as_multiple_pages() {
    let input = json!({
        "version": 1,
        "pages": [
            {"spec": {"size": "A5"}, "items": []},
            {"spec": {"size": "A4", "orientation": "landscape"}, "items": []},
        ],
    })
    .to_string();
    let document = parse(input.as_bytes()).unwrap();
    assert_eq!(document.pages.len(), 2);
    assert_eq!(document.pages[1].spec.size_mm(), (297.0, 210.0));
}
