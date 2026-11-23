# printkit performance baseline

Machine: MacBook Air (M2, 8 cores, 24 GB), macOS 27.2, rustc 1.99.0.
Profile: `bench` (release). Date: 2026-11-07; the batch and PDF passthrough
rows were re-measured on 2026-11-08 against the reviewed tree.

## HTML serialisation (criterion)

Command:

```
cargo bench -p printkit-engine --bench render -- --warm-up-time 1 --measurement-time 3
```

| bench | median | interval |
|---|---|---|
| `bench_render_text` (one A4 page, one text item) | 588.20 ns | 587.45 - 588.79 ns |
| `bench_render_html` (one A4 page: grid, QR, rule, 10-row table) | 333.32 µs | 331.18 - 335.54 µs |

These numbers measure only the Rust-side HTML string. WebView rendering is
measured by the end-to-end PDF path; the batch scenario and the C-Lodop
comparison are recorded in the sections added by the print and batch steps.

## Embedded PDF merge (criterion)

Command:

```
cargo bench -p printkit-pdf --bench merge -- --warm-up-time 1 --measurement-time 3
```

| bench | median | interval |
|---|---|---|
| `bench_merge_passthrough` (parse, font validation, text-map repair, save) | 14.48 µs | 14.47 - 14.50 µs |
| `bench_merge_form` (same work plus one one-page Form XObject) | 28.94 µs | 28.89 - 28.98 µs |

The fixture PDFs are synthetic one-page documents with an embedded subset font;
the renderer's own PDF is not in the measurement. The difference is the cost of
copying the page's resources and content stream into a Form XObject.

## Batch engine overhead (criterion)

Command:

```
cargo bench -p printkit-engine --bench batch -- --warm-up-time 1 --measurement-time 3
```

| bench | median | interval |
|---|---|---|
| `bench_batch_100x2` (100 two-page documents, one merged job) | 105.82 ms | 105.24 - 106.41 ms |

The fake renderer removes WebView variance, so this measures validation, HTML
layout, PDF concatenation and the submission call. WebView cost is in the
end-to-end number below.

## Batch end to end (macOS, K=1)

Date: 2026-11-08, same tree as the `bench_batch_100x2` row. Release binary,
`doc.printBatch` through `serve`, virtual IPP queue created with
`ippeveprinter` + `lpadmin`.

Command:

```
PRINTKIT_BENCH_PRINTER=<queue> bash scripts/bench-batch.sh 100 2
```

| scenario | wall clock | spooler jobs | budget |
|---|---|---|---|
| 100 documents x 2 pages, one `doc.printBatch` | 13.71 s | 1 | 45 s (macOS, K=1) |

The measured interval starts at the WebSocket request and ends at the `done`
job event. The Windows budget (20 s, K=4) belongs to W1.

## PDF passthrough (macOS, P0)

Date: 2026-11-08. `doc.printPdf` through `serve` with a 1000-page, 118041-byte
PDF and the same virtual IPP queue.

| scenario | wall clock | trace stages |
|---|---|---|
| 1000-page PDF, zero render | 0.045 s | recv / validate / submit / result |

The interval is request to `done`; the trace has no `render` stage because the
bytes go from `lopdf` straight to `cupsPrintFile`.

## C-Lodop comparison

Status: **not measured on this machine.** C-Lodop is not installed, and the
virtual IPP queue proves the submission chain, not paper output. The comparison
needs the real laser printer from H1.

Procedure, run once on that machine with the same input, driver and queue:

1. Print the 100 x 2 document batch with C-Lodop, timing from the call to the
   spooler reporting completion, then to the last sheet leaving the printer.
2. Repeat with `scripts/bench-batch.sh 100 2` and the same queue.
3. Record both wall clocks and their ratio here. The acceptance criterion is
   `printkit / C-Lodop <= 1.0`; the per-page numbers above are the printkit
   side of that ratio.

## Release re-measurement (2026-11-08)

Machine and profile as above, on the current tree. The render numbers are not
comparable with the first table: `to_html` now embeds the bundled faces as
base64 data URIs, so each call copies the font payload. The absolute values
are still far below the request budgets.

Commands:

```
cargo bench -p printkit-engine --bench render -- --warm-up-time 1 --measurement-time 3
cargo bench -p printkit-pdf --bench merge -- --warm-up-time 1 --measurement-time 3
cargo bench -p printkit-engine --bench batch -- --warm-up-time 1 --measurement-time 3
```

| bench | median | interval |
|---|---|---|
| `bench_render_text` | 338.73 µs | 338.41 - 339.06 µs |
| `bench_render_html` | 686.44 µs | 684.71 - 688.38 µs |
| `bench_merge_passthrough` | 14.42 µs | 14.41 - 14.43 µs |
| `bench_merge_form` | 29.48 µs | 29.45 - 29.51 µs |
| `bench_batch_100x2` | 106.62 ms | 106.35 - 106.89 ms |

The merge path is within a percent of the first table; the Form XObject row is
1.9% slower and the batch engine row 0.8% slower, both inside their budgets.

End-to-end on the temporary virtual IPP queue created for this run (the queue is
removed after the benchmark; pass its current name, not the historical one):

```
PRINTKIT_BENCH_PRINTER=<virtual-queue> bash scripts/bench-batch.sh 100 2
doc.printBatch: 100 documents x 2 pages in 14.26s (budget 45s), job job-1
spooler jobs: 1
```

CLI, one page including process start:

```
/usr/bin/time -p ./target/release/printkit pdf \
  -i tests/fixtures/minimal.json -o /tmp/perf-one.pdf
real 0.33
```

The 1000-page PDF passthrough row was not re-run here; the path is unchanged
(no render, no ToUnicode repair) and the earlier measurement is retained.
