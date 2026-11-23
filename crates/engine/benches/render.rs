//! Render benchmarks for the two document shapes the service meets most:
//! a text-only page and a page full of vector/table HTML.
//!
//! Only HTML serialisation is measured. The WebView render is covered by the
//! PDF path and by the batch benchmark, not here.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use printkit_core::Document;

const TEXT: &str = r#"{
  "version": 1,
  "pages": [{
    "spec": {"size": "A4"},
    "items": [{
      "id": "body",
      "type": "text",
      "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 120.0},
      "style": {"fontSize": 3.7, "lineHeight": 1.4},
      "data": {"text": "printkit render benchmark"}
    }]
  }]
}"#;

const VECTOR: &str = r#"{
  "version": 1,
  "pages": [{
    "spec": {"size": "A4"},
    "items": [
      {"id": "rect", "type": "shape", "frame": {"x": 20.0, "y": 20.0, "width": 80.0, "height": 30.0}, "data": {"kind": "tianGrid", "cell": {"rows": 4, "cols": 6}}},
      {"id": "qr", "type": "barcode", "frame": {"x": 110.0, "y": 20.0, "width": 30.0, "height": 30.0}, "data": {"kind": "qr", "value": "https://example.invalid/printkit"}},
      {"id": "line", "type": "shape", "frame": {"x": 20.0, "y": 60.0, "width": 170.0, "height": 4.0}, "data": {"kind": "textRule"}},
      {"id": "table", "type": "table", "frame": {"x": 20.0, "y": 80.0, "width": 170.0, "height": 150.0},
       "data": {
         "columns": [
           {"key": "no", "title": "No", "width": 20.0},
           {"key": "name", "title": "Name", "width": 70.0},
           {"key": "amount", "title": "Amount", "width": "auto", "align": "right"}
         ],
         "rows": [
           {"no": "001", "name": "Alpha", "amount": "10.00"},
           {"no": "002", "name": "Beta", "amount": "20.00"},
           {"no": "003", "name": "Gamma", "amount": "30.00"},
           {"no": "004", "name": "Delta", "amount": "40.00"},
           {"no": "005", "name": "Epsilon", "amount": "50.00"},
           {"no": "006", "name": "Zeta", "amount": "60.00"},
           {"no": "007", "name": "Eta", "amount": "70.00"},
           {"no": "008", "name": "Theta", "amount": "80.00"},
           {"no": "009", "name": "Iota", "amount": "90.00"},
           {"no": "010", "name": "Kappa", "amount": "100.00"}
         ],
         "footer": {"label": "Total", "values": {"amount": "550.00"}}
       }
      }
    ]
  }]
}"#;

fn render_text(criterion: &mut Criterion) {
    let document: Document = serde_json::from_str(TEXT).expect("text benchmark document");
    let validated = document.validated().expect("text benchmark validates");
    criterion.bench_function("bench_render_text", |b| {
        b.iter(|| {
            printkit_render::to_html(black_box(&validated)).expect("benchmark render succeeds")
        });
    });
}

fn render_html(criterion: &mut Criterion) {
    let document: Document = serde_json::from_str(VECTOR).expect("vector benchmark document");
    let validated = document.validated().expect("vector benchmark validates");
    criterion.bench_function("bench_render_html", |b| {
        b.iter(|| {
            printkit_render::to_html(black_box(&validated)).expect("benchmark render succeeds")
        });
    });
}

criterion_group!(benches, render_text, render_html);
criterion_main!(benches);
