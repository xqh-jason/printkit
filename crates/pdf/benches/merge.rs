//! Merge benchmarks: the renderer PDF with and without embedded pages.
//!
//! `passthrough` measures parse, font validation, text-map repair and
//! serialisation. `form` measures the same work plus cloning a page into a
//! Form XObject and painting it.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::hint::black_box;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use lopdf::{Dictionary, Document, Object, Stream};

fn sample_pdf(content: &[u8]) -> Bytes {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            String::from_utf8_lossy(content)
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /AAAAAA+Fake /FontDescriptor 6 0 R >>".to_owned(),
        "<< /Type /FontDescriptor /FontName /AAAAAA+Fake /Flags 4 \
         /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
         /CapHeight 700 /StemV 80 /FontFile 7 0 R >>"
            .to_owned(),
        "<< /Length 4 >>\nstream\nfake\nendstream".to_owned(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{object}\nendobj\n", index + 1);
    }
    let xref_start = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        let _ = writeln!(out, "{offset:010} 00000 n ");
    }
    let _ = write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF\n",
        objects.len() + 1
    );
    Bytes::from(out)
}

fn embedded_pdf() -> Bytes {
    let mut document = Document::with_version("1.4");
    let mut catalog = Dictionary::new();
    catalog.set("Type", "Catalog");
    catalog.set("Pages", Object::Reference((2, 0)));
    let mut pages = Dictionary::new();
    pages.set("Type", "Pages");
    pages.set("Kids", vec![Object::Reference((3, 0))]);
    pages.set("Count", 1);
    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set("Parent", Object::Reference((2, 0)));
    page.set(
        "MediaBox",
        vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(595.0),
            Object::Real(842.0),
        ],
    );
    page.set("Contents", Object::Reference((4, 0)));
    let mut dict = Dictionary::new();
    dict.set("Length", 42);
    let stream = Stream::new(
        dict,
        b"BT /F1 12 Tf 40 800 Td (embedded merge benchmark) Tj ET".to_vec(),
    );
    document.add_object(catalog);
    document.add_object(pages);
    document.add_object(page);
    document.add_object(stream);
    document.trailer.set("Root", Object::Reference((1, 0)));
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    Bytes::from(bytes)
}

fn merge_passthrough(criterion: &mut Criterion) {
    let main = sample_pdf(b"BT /F1 24 Tf 72 770 Td (passthrough) Tj ET");
    criterion.bench_function("bench_merge_passthrough", |b| {
        b.iter(|| {
            printkit_pdf::assemble_embedded(black_box(main.clone()), black_box(&[])).unwrap()
        });
    });
}

fn merge_form(criterion: &mut Criterion) {
    let main = sample_pdf(b"BT /F1 24 Tf 72 770 Td (form merge) Tj ET");
    let embedded = embedded_pdf();
    let pages = [printkit_pdf::EmbeddedPage {
        bytes: embedded.clone(),
        page: 1,
        target_page: 1,
        frame: printkit_pdf::PtRect::new(40.0, 40.0, 400.0, 400.0),
    }];
    criterion.bench_function("bench_merge_form", |b| {
        b.iter(|| {
            printkit_pdf::assemble_embedded(black_box(main.clone()), black_box(&pages)).unwrap()
        });
    });
}

criterion_group!(benches, merge_passthrough, merge_form);
criterion_main!(benches);
