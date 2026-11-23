//! Batch benchmark: 100 documents of two pages each, merged into one spooler
//! job.
//!
//! The fake renderer removes WebView variance, so this number tracks the
//! engine's own work: validation, HTML layout, PDF merging and submission of
//! one job. WebView cost is measured end-to-end by `scripts/bench-batch.sh`.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use printkit_engine::{BatchPart, FetchPolicy, print_batch_now};
use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
use printkit_trace::NullSink;

const TWO_PAGES: &str = r#"{
  "version": 1,
  "pages": [
    {"spec": {"size": "A4"}, "items": [{"id": "a", "type": "text", "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 20.0}, "data": {"text": "batch page one"}}]},
    {"spec": {"size": "A4"}, "items": [{"id": "b", "type": "text", "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 20.0}, "data": {"text": "batch page two"}}]}
  ]
}"#;

fn bench_batch_100x2(criterion: &mut Criterion) {
    let parts: Vec<BatchPart> = (0..100)
        .map(|_| BatchPart::Document(TWO_PAGES.as_bytes().to_vec()))
        .collect();
    let host = FakeRenderHost::new();
    let backend = FakePrinterBackend::new();
    let sink = NullSink;
    criterion.bench_function("bench_batch_100x2", |bencher| {
        bencher.iter(|| {
            let parts = parts.clone();
            let done = print_batch_now(
                parts,
                &host,
                &backend,
                &sink,
                &FetchPolicy::default(),
                Some("bench".to_owned()),
                1,
                true,
                "bench".to_owned(),
                None,
            )
            .unwrap();
            black_box(done.sheets());
        });
    });
}

criterion_group!(benches, bench_batch_100x2);
criterion_main!(benches);
