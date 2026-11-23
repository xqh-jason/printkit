#![allow(clippy::panic, clippy::unwrap_used)]

//! The four illegal documents named by the contract, plus one snapshot per
//! semantic validation branch.

use std::collections::BTreeMap;

use printkit_core::{DocumentError, E_SCHEMA, E_UNSUPPORTED, validate_bytes};
use serde_json::{Value, json};

fn frame() -> Value {
    json!({"x": 1, "y": 1, "width": 10, "height": 5})
}

fn item(kind: &str, style: Value, data: Value) -> Value {
    json!({
        "id": "a",
        "type": kind,
        "frame": frame(),
        "style": style,
        "data": data,
    })
}

fn document(items: Value) -> String {
    json!({
        "version": 1,
        "pages": [{"spec": {"size": "A4"}, "items": items}],
    })
    .to_string()
}

fn one(item: Value) -> String {
    document(json!([item]))
}

fn text(data: Value) -> String {
    one(item("text", json!({}), data))
}

fn violations(input: &str) -> Vec<printkit_core::Violation> {
    match validate_bytes(input.as_bytes()) {
        Err(DocumentError::Semantic(violations)) => violations,
        Err(other) => panic!("expected semantic violations, got {other:?}"),
        Ok(_) => panic!("document was accepted"),
    }
}

fn pointer(input: &str) -> String {
    let error = validate_bytes(input.as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    error.pointer().unwrap_or_default().to_owned()
}

#[test]
fn validate_unknown_field_points_at_the_field() {
    // Root level: serde reports the key directly.
    let root = json!({
        "version": 1,
        "pages": [{"spec": {"size": "A4"}, "items": []}],
        "bogus": true,
    });
    assert_eq!(pointer(&root.to_string()), "/bogus");

    // Page spec: the tracked path already names the key.
    let spec = json!({
        "version": 1,
        "pages": [{"spec": {"size": "A4", "bogus": 1}, "items": []}],
    });
    assert_eq!(pointer(&spec.to_string()), "/pages/0/spec/bogus");

    // Item body: an internally tagged enum buffers its fields, so the raw JSON
    // is walked to name the key inside `data`.
    let item = json!({
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "a",
                "type": "text",
                "frame": frame(),
                "data": {"text": "printkit", "bogus": 1},
            }],
        }],
    });
    assert_eq!(pointer(&item.to_string()), "/pages/0/items/0/data/bogus");
}

#[test]
fn validate_rejects_more_than_max_pages() {
    let pages: Vec<Value> = (0..201)
        .map(|_| json!({"spec": {"size": "A4"}, "items": []}))
        .collect();
    let input = json!({"version": 1, "pages": pages}).to_string();
    assert_eq!(pointer(&input), "/pages");
    assert_eq!(
        violations(&input)[0].message,
        "a document holds at most 200 pages"
    );
}

#[test]
fn validate_rejects_non_null_paper_source() {
    let input = json!({
        "version": 1,
        "pages": [{"spec": {"size": "A4"}, "items": []}],
        "output": {"printer": {"paperSource": "tray-1"}},
    })
    .to_string();
    assert_eq!(pointer(&input), "/output/printer/paperSource");
    assert_eq!(
        violations(&input)[0].message,
        "paperSource must be null in version 1"
    );
}

#[test]
fn validate_rejects_out_of_range_frame() {
    let negative = json!({
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "a",
                "type": "text",
                "frame": {"x": -5, "y": 1, "width": 10, "height": 5},
                "data": {"text": "printkit"},
            }],
        }],
    })
    .to_string();
    assert_eq!(pointer(&negative), "/pages/0/items/0/frame/x");

    let zero_width = json!({
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "a",
                "type": "text",
                "frame": {"x": 1, "y": 1, "width": 0, "height": 5},
                "data": {"text": "printkit"},
            }],
        }],
    })
    .to_string();
    assert_eq!(pointer(&zero_width), "/pages/0/items/0/frame/width");

    let past_paper = json!({
        "version": 1,
        "pages": [{
            "spec": {"size": "A4"},
            "items": [{
                "id": "a",
                "type": "text",
                "frame": {"x": 205, "y": 1, "width": 10, "height": 5},
                "data": {"text": "printkit"},
            }],
        }],
    })
    .to_string();
    assert_eq!(pointer(&past_paper), "/pages/0/items/0/frame");
}

#[test]
fn validate_rejects_a_custom_paper_edge_past_the_limit() {
    let input = json!({
        "version": 1,
        "pages": [{
            "spec": {"size": "custom", "width": 50_000, "height": 200},
            "items": [],
        }],
    })
    .to_string();
    assert_eq!(pointer(&input), "/pages/0/spec/width");
}

#[test]
fn validate_covers_every_semantic_branch() {
    let pages: Vec<Value> = (0..201)
        .map(|_| json!({"spec": {"size": "A4"}, "items": []}))
        .collect();
    let items: Vec<Value> = (0..501)
        .map(|index| {
            json!({
                "id": format!("i{index}"),
                "type": "text",
                "frame": frame(),
                "data": {"text": "x"},
            })
        })
        .collect();
    let rows: Vec<Value> = (0..10_001).map(|_| json!({"a": "1"})).collect();
    let columns: Vec<Value> = (0..33)
        .map(|index| json!({"key": format!("k{index}"), "title": "t", "width": 10}))
        .collect();
    let tags: Vec<Value> = (0..9).map(|index| json!(format!("t{index}"))).collect();
    let large_background = format!(
        "data:image/png;base64,{}",
        "A".repeat(8 * 1024 * 1024 / 3 * 4 + 4)
    );

    let mut report: BTreeMap<&str, Vec<printkit_core::Violation>> = BTreeMap::new();
    let mut record = |name: &'static str, input: String| {
        let found = violations(&input);
        assert!(
            found.iter().all(|violation| violation.code == E_SCHEMA),
            "{name} reported a code other than {E_SCHEMA}"
        );
        report.insert(name, found);
    };

    record("version", json!({"version": 2, "pages": []}).to_string());
    record(
        "pages-empty",
        json!({"version": 1, "pages": []}).to_string(),
    );
    record(
        "pages-too-many",
        json!({"version": 1, "pages": pages}).to_string(),
    );
    record(
        "items-too-many",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": items}],
        })
        .to_string(),
    );
    record(
        "custom-width-missing",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "custom", "height": 140}, "items": []}],
        })
        .to_string(),
    );
    record(
        "custom-width-not-positive",
        json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "custom", "width": 0, "height": 140},
                "items": [],
            }],
        })
        .to_string(),
    );
    record(
        "custom-height-missing",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "custom", "width": 241}, "items": []}],
        })
        .to_string(),
    );
    record(
        "custom-orientation-conflict",
        json!({
            "version": 1,
            "pages": [{
                "spec": {
                    "size": "custom",
                    "width": 241,
                    "height": 140,
                    "orientation": "portrait",
                },
                "items": [],
            }],
        })
        .to_string(),
    );
    record(
        "named-size-with-width",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4", "width": 210}, "items": []}],
        })
        .to_string(),
    );
    record(
        "margin-negative",
        json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "A4", "margins": {"top": -1}},
                "items": [],
            }],
        })
        .to_string(),
    );
    record(
        "background-not-data-uri",
        json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "A4", "background": "https://example.com/bg.png"},
                "items": [],
            }],
        })
        .to_string(),
    );
    record(
        "background-too-large",
        json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "A4", "background": large_background},
                "items": [],
            }],
        })
        .to_string(),
    );
    record(
        "copies-out-of-range",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": []}],
            "output": {"copies": 1000},
        })
        .to_string(),
    );
    record(
        "paper-source-set",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": []}],
            "output": {"printer": {"paperSource": "tray-1"}},
        })
        .to_string(),
    );
    record(
        "too-many-tags",
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": []}],
            "meta": {"tags": tags},
        })
        .to_string(),
    );
    record("tag-too-long", {
        json!({
            "version": 1,
            "pages": [{"spec": {"size": "A4"}, "items": []}],
            "meta": {"tags": ["x".repeat(25)]},
        })
        .to_string()
    });
    record(
        "item-id-empty",
        one(json!({
            "id": " ",
            "type": "text",
            "frame": frame(),
            "data": {"text": "x"},
        })),
    );
    record(
        "frame-width-zero",
        one(json!({
            "id": "a",
            "type": "text",
            "frame": {"x": 1, "y": 1, "width": 0, "height": 5},
            "data": {"text": "x"},
        })),
    );
    record("text-empty", text(json!({"text": ""})));
    record("text-too-long", text(json!({"text": "x".repeat(10_001)})));
    record(
        "text-font-invalid",
        text(json!({"text": "x", "fontFile": "https://example.com/f.ttf"})),
    );
    record(
        "text-style-font-size",
        one(item("text", json!({"fontSize": 0}), json!({"text": "x"}))),
    );
    record(
        "text-style-letter-spacing",
        one(item(
            "text",
            json!({"letterSpacing": -1}),
            json!({"text": "x"}),
        )),
    );
    record(
        "text-style-line-height",
        one(item("text", json!({"lineHeight": 0}), json!({"text": "x"}))),
    );
    record(
        "text-style-color",
        one(item("text", json!({"color": "red"}), json!({"text": "x"}))),
    );
    record("html-neither", one(item("html", json!({}), json!({}))));
    record(
        "html-both",
        one(item(
            "html",
            json!({}),
            json!({"html": "<p>x</p>", "url": "https://example.com"}),
        )),
    );
    record(
        "html-url-scheme",
        one(item("html", json!({}), json!({"url": "ftp://example.com"}))),
    );
    record(
        "image-src-invalid",
        one(item("image", json!({}), json!({"src": "photo.png"}))),
    );
    record(
        "pdf-page-zero",
        one(item(
            "pdf",
            json!({}),
            json!({"src": "data:application/pdf;base64,AA==", "page": 0}),
        )),
    );
    record(
        "barcode-value-empty",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "code128", "value": ""}),
        )),
    );
    record(
        "barcode-ean13-length",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "ean13", "value": "123"}),
        )),
    );
    record(
        "barcode-ean13-digits",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "ean13", "value": "12345678901a"}),
        )),
    );
    record(
        "barcode-ean13-check-digit",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "ean13", "value": "1234567890123"}),
        )),
    );
    record(
        "barcode-code39-charset",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "code39", "value": "abc"}),
        )),
    );
    record(
        "barcode-code128-charset",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "code128", "value": "caf\u{e9}"}),
        )),
    );
    record(
        "barcode-2d-too-long",
        one(item(
            "barcode",
            json!({}),
            json!({"kind": "qr", "value": "x".repeat(2_049)}),
        )),
    );
    record(
        "barcode-bar-width",
        one(item(
            "barcode",
            json!({"barWidth": 0}),
            json!({"kind": "code128", "value": "PK-1"}),
        )),
    );
    record(
        "barcode-quiet-zone",
        one(item(
            "barcode",
            json!({"quietZone": -1}),
            json!({"kind": "code128", "value": "PK-1"}),
        )),
    );
    record(
        "barcode-text-size",
        one(item(
            "barcode",
            json!({"textSize": 0}),
            json!({"kind": "code128", "value": "PK-1"}),
        )),
    );
    record(
        "barcode-color",
        one(item(
            "barcode",
            json!({"color": "#fff"}),
            json!({"kind": "code128", "value": "PK-1"}),
        )),
    );
    record(
        "shape-polygon-missing-points",
        one(item("shape", json!({}), json!({"kind": "polygon"}))),
    );
    record(
        "shape-polygon-too-short",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "polygon", "points": [0, 0, 1, 1]}),
        )),
    );
    record(
        "shape-polygon-odd",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "polygon", "points": [0, 0, 1, 1, 2]}),
        )),
    );
    record(
        "shape-points-not-polygon",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "rect", "points": [0, 0, 1, 1, 2, 2]}),
        )),
    );
    record(
        "shape-tiangrid-missing-cell",
        one(item("shape", json!({}), json!({"kind": "tianGrid"}))),
    );
    record(
        "shape-tiangrid-empty-cell",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "tianGrid", "cell": {"rows": 0, "cols": 0}}),
        )),
    );
    record(
        "shape-cell-not-tiangrid",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "rect", "cell": {"rows": 2, "cols": 2}}),
        )),
    );
    record(
        "shape-lines-not-textrule",
        one(item(
            "shape",
            json!({}),
            json!({"kind": "rect", "lines": {"top": true}}),
        )),
    );
    record(
        "shape-stroke-width",
        one(item(
            "shape",
            json!({"strokeWidth": -1}),
            json!({"kind": "rect"}),
        )),
    );
    record(
        "shape-dash-empty",
        one(item("shape", json!({"dash": []}), json!({"kind": "rect"}))),
    );
    record(
        "shape-dash-negative",
        one(item(
            "shape",
            json!({"dash": [-1]}),
            json!({"kind": "rect"}),
        )),
    );
    record(
        "shape-stroke-color",
        one(item(
            "shape",
            json!({"stroke": "black"}),
            json!({"kind": "rect"}),
        )),
    );
    record(
        "shape-fill-color",
        one(item(
            "shape",
            json!({"fill": "black"}),
            json!({"kind": "rect"}),
        )),
    );
    record(
        "table-columns-empty",
        one(item("table", json!({}), json!({"columns": [], "rows": []}))),
    );
    record(
        "table-columns-too-many",
        one(item(
            "table",
            json!({}),
            json!({"columns": columns, "rows": []}),
        )),
    );
    record(
        "table-column-key-empty",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [{"key": " ", "title": "t", "width": 10}],
                "rows": [],
            }),
        )),
    );
    record(
        "table-column-key-duplicate",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [
                    {"key": "a", "title": "t", "width": 10},
                    {"key": "a", "title": "u", "width": "auto"},
                ],
                "rows": [],
            }),
        )),
    );
    record(
        "table-column-width-negative",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": -1}],
                "rows": [],
            }),
        )),
    );
    record(
        "table-rows-too-many",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": rows,
            }),
        )),
    );
    record(
        "table-row-unknown-key",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [{"b": "1"}],
            }),
        )),
    );
    record(
        "table-footer-unknown-key",
        one(item(
            "table",
            json!({}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [],
                "footer": {"label": "total", "values": {"b": "1"}},
            }),
        )),
    );
    record(
        "table-font-size",
        one(item(
            "table",
            json!({"fontSize": 0}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [],
            }),
        )),
    );
    record(
        "table-row-height",
        one(item(
            "table",
            json!({"rowHeight": 0}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [],
            }),
        )),
    );
    record(
        "table-border-width",
        one(item(
            "table",
            json!({"borderWidth": -1}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [],
            }),
        )),
    );
    record(
        "table-border-color",
        one(item(
            "table",
            json!({"borderColor": "black"}),
            json!({
                "columns": [{"key": "a", "title": "t", "width": 10}],
                "rows": [],
            }),
        )),
    );

    insta::assert_json_snapshot!(report);
}

#[test]
fn resources_must_decode_and_match_their_media_type() {
    let cases = [
        ("data:text/plain;base64,aGVsbG8=", "media types"),
        ("data:image/png;base64,", "empty"),
        ("data:image/png;base64,!!!!", "base64"),
    ];
    for (src, expected) in cases {
        let input = one(item("image", json!({}), json!({ "src": src })));
        let DocumentError::Semantic(violations) =
            validate_bytes(input.as_bytes()).expect_err("resource should be rejected")
        else {
            panic!("expected a semantic violation for {src}");
        };
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains(expected) && v.pointer.ends_with("/data/src")),
            "{src}: {:?}",
            violations
        );
    }
}

#[test]
fn trailing_bytes_after_the_document_are_rejected() {
    let input = format!("{} trailing", document(json!([])));
    let error = printkit_core::parse(input.as_bytes()).expect_err("trailing bytes must fail");
    assert!(error.message.contains("trailing"), "{:?}", error);
}

#[test]
fn tian_grid_cell_count_is_capped() {
    let input = one(item(
        "shape",
        json!({}),
        json!({"kind": "tianGrid", "cell": {"rows": 1000, "cols": 20}}),
    ));
    let error = validate_bytes(input.as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    assert_eq!(
        error.pointer(),
        Some("/pages/0/items/0/data/cell"),
        "{error:?}"
    );
}

#[test]
fn animated_webp_is_unsupported() {
    use base64::Engine as _;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"VP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.push(0x02);
    bytes.extend_from_slice(&[0_u8; 10]);
    let src = format!(
        "data:image/webp;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let input = one(item("image", json!({}), json!({"src": src})));
    let error = validate_bytes(input.as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_UNSUPPORTED);
    assert_eq!(error.pointer(), Some("/pages/0/items/0/data/src"));
}

#[test]
fn duplicate_item_ids_on_one_page_are_rejected_after_the_first() {
    let input = document(json!([
        item("text", json!({}), json!({"text": "first"})),
        item("text", json!({}), json!({"text": "second"})),
        item("text", json!({}), json!({"text": "third"})),
    ]));
    let error = validate_bytes(input.as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    let violations = error.violations();
    let pointers: Vec<&str> = violations
        .iter()
        .map(|violation| violation.pointer.as_str())
        .collect();
    assert_eq!(pointers, ["/pages/0/items/1/id", "/pages/0/items/2/id"]);
    assert!(violations[0].message.contains("unique across the document"));
    assert!(violations[0].message.contains("/pages/0/items/0"));
}

#[test]
fn duplicate_item_ids_across_pages_are_rejected() {
    let input = json!({
        "version": 1,
        "pages": [
            {"spec": {"size": "A4"}, "items": [item("text", json!({}), json!({"text": "one"}))]},
            {"spec": {"size": "A4"}, "items": [item("text", json!({}), json!({"text": "two"}))]},
        ],
    })
    .to_string();
    let error = validate_bytes(input.as_bytes()).unwrap_err();
    assert_eq!(error.code(), E_SCHEMA);
    assert_eq!(error.pointer(), Some("/pages/1/items/0/id"));
    assert!(error.violations()[0].message.contains("/pages/0/items/0"));
}

#[test]
fn distinct_item_ids_across_pages_are_accepted() {
    let input = json!({
        "version": 1,
        "pages": [
            {"spec": {"size": "A4"}, "items": [
                {"id": "title", "type": "text", "frame": frame(), "style": {},
                 "data": {"text": "one"}},
            ]},
            {"spec": {"size": "A4"}, "items": [
                {"id": "body", "type": "text", "frame": frame(), "style": {},
                 "data": {"text": "two"}},
            ]},
        ],
    })
    .to_string();
    assert!(validate_bytes(input.as_bytes()).is_ok());
}
