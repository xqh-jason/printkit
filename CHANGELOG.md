# Changelog

All notable changes to printkit are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses the versioning policy in [CONTRIBUTING.md](CONTRIBUTING.md#versioning).

## [Unreleased]


## [0.2.6] - 2026-11-23

CI-only cut after the v0.2.5 Release run failed in the gates. No printing
behaviour changes.

### Fixed

- macOS PDF capture waits for each inline image to finish loading before calling
  `Image.decode()`. Calling `decode()` on a still-loading image rejects at once;
  the old barrier swallowed that rejection and could capture a white page
  background on a cold WKWebView.
- The desktop shell handles SIGTERM/SIGINT through the same quit path as the
  tray action: stop the owned sidecar, wait for it, then exit.
- `printkit serve` unblocks inherited SIGTERM/SIGINT before installing its
  async handlers, so a sidecar spawned by a shell that blocked those signals
  still stops when signalled.
- `desktop_smoke.sh` treats a zombie as exited, checks the process state instead
  of `kill -0` alone, requires the sidecar port to stay closed, and reports the
  process state and serve log on a real survivor.

### Changed

- Package, desktop, Tauri, SDK and i18n versions are 0.2.6.

## [0.2.5] - 2026-11-23

CI-only cut. Proves the reworked Release workflow; no printing behaviour changes.

### Fixed

- The Release verify job no longer hangs: the engine test that waited on a
  blocking reader now bounds its wait, so `cargo test` completes on a loaded
  runner.
- GitHub Actions are pinned to Node 24 releases: `actions/checkout` v7.0.1,
  `actions/setup-node` v7.1.0 and `pnpm/action-setup` v6.1.1 (SHA pinned).
- `cookie` is updated to 0.18.3 (RUSTSEC-2026-0594: private-cookie nonces could
  repeat after `fork()`; printkit's single-process service was not affected,
  and the advisory gate now passes on the patched release).

### Changed

- Package, desktop, Tauri, SDK and i18n versions are 0.2.5.


## [0.2.4] - 2026-11-23

Paint-barrier cut. Makes the macOS render deterministic on loaded CI runners;
no printing behaviour changes.

### Fixed

- macOS rendering decodes every inline image and waits for two animation frames
  (with a `setTimeout` fallback) before `createPDF`. `Image.complete` only says
  the bytes arrived; the decoded image may not be on the captured layer yet, so
  a slow arm64/Intel runner could export a white background page.
- The desktop smoke test allows a loaded runner 10 seconds to reap the sidecar
  after SIGTERM (the service's own drain bound is 2 s).
  `trace_subscribe.sh` builds the service binary it needs when a clean checkout
  has no root build.

### Changed

- Package, desktop, Tauri, SDK and i18n versions are 0.2.4.

## [0.2.3] - 2026-11-23

CI-green cut. Fixes the three failures seen in the 0.2.2 public CI run and
changes no printing behaviour.

### Fixed

- macOS rendering waits for every inline image to finish decoding before
  `createPDF`, so a slow Intel runner no longer exports a page whose background
  SVG has not painted yet (Intel geometry gate).
- `trace_subscribe.sh` builds the service binary it needs when the checkout has
  no root build, instead of failing to start `serve` in a clean CI job.
- `desktop_smoke.sh` polls `/admin/session` after `serve.lock` appears, covering
  the window where the lock exists but the port is not bound yet.

### Changed

- Package, desktop, Tauri, SDK and i18n versions are 0.2.3.

## [0.2.2] - 2026-11-23

CI-baseline and dependency-policy cut. No product behaviour changes for
printing; the release exists so the public repository can ship a tree whose
checks are runnable and whose Dependabot queue is bounded.

### Fixed

- The `serve` feature now bundles SQLite instead of relying on a system copy,
  so the CI release build links the same storage engine as the shipped binary.
- `printkit-platform-win` tests compile and run on Windows again; the
  `page_only_html` conversion helper and the `parse-count` test hook no longer
  produce flaky results.

### Changed

- Dependabot runs monthly with per-ecosystem groups, `open-pull-requests-limit: 3`
  and a cooldown (5 days default, 30 for major, 7 minor, 3 patch). Major updates
  for `rusqlite`, `ts-rs`, `toml`, `tauri`, `tauri-build` and the desktop
  toolchain are ignored until deliberately migrated. Security updates remain
  ungrouped and immediate.
- Package, desktop, Tauri, SDK and i18n versions are 0.2.2.

## [0.2.1] - 2026-11-23

### Fixed

- Removed the remaining machine-specific home path from `tests/scripts/ui_frames.mjs`; it now derives the data directory from `os.homedir()`.
- Replaced the remaining internal layout reference name in tracked docs/comments with neutral printkit wording.
- Bumped the package, desktop, Tauri, SDK and i18n versions to 0.2.1 for the public hotfix.

## [0.2.0] - 2026-11-23

Second pre-1.0 cut. It closes the removal of calibration/templates and demo UI,
makes finished PDFs a first-class print path (`print --pdf`, `doc.printPdf`,
`POST /print/batch` and the new `doc.printPdfBatch`, all byte-for-byte for a
single PDF), adds V10/V11 record indexes, shuts the service down through one
bounded drain path, updates `url` to 2.5.9 for RUSTSEC-2026-0592, scopes four
upstream unmaintained desktop advisories with a review date, and gives the
desktop smooth page transitions with loading states that keep their old data.
Protocol surface: 22 descriptors (`hello` + 21 requests); SDK: 19 public
methods. Root and desktop `cargo deny check` pass; `dist` is built at this
commit.

### Changed

- `printkit print` validates PDF inputs before opening a job: a missing or unreadable file, a non-PDF `--pdf` input and an unparsable/zero-page PDF return `E_SCHEMA` (exit 2); encrypted and structurally unsupported PDFs keep `E_PDF_ENCRYPTED`/`E_UNSUPPORTED`. Mixed `--pdf` and `-i` inputs are ordered deterministically (all PDFs first, then documents, each in repetition order).
- The desktop PDF print button is disabled while its job is active and reuses one request id for a retry with the same file, printer and copies; `print_pdf_bytes` rejects a request id that is not 1-128 printable ASCII characters. Records keeps error, copies, template and trace columns visible at 900px, and test print stays disabled unless the owned service is running and reachable.
- A desktop first start publishes its `plugins.toml` default through a 0600 sibling and an atomic no-replace link, so a concurrent writer or a user file wins without truncation; the temporary file is removed afterwards.
- Removed the built-in `calibration` plugin, its Calibration page, `calib.set/list` methods, `/admin/calibrations` routes (including the sample sheet) and `calibration` table; printer calibration is no longer part of the product. Built-in optional plugins are now `records` and `observability`.
- Records summary exposes the window-scoped `pending` (no today-specific counterpart); `done = total - failed - cancelled - pending`, and the failure rate is `failed / (done + failed)` with cancelled/pending excluded.
- DNS lookups for `data.url` now run on a fixed four-worker pool with a bounded queue instead of one thread per host, so a stuck system resolver cannot accumulate threads; queue-full and deadline misses return explicit errors.
- The idempotency cache is bounded to 1024 entries / 1 MiB (LRU) as well as the 600-second window, and keys are namespaced by transport, method and origin. Duplicate suppression is best-effort: after the window, an eviction or a restart the same key prints again, so clients that must not repeat a page need their own business id.
- Upgrading a database that still holds calibration rows writes `printkit.db.bak-before-v9` before `V9__drop_calibration.sql`; a failed backup aborts the migration and leaves the original database intact.
- Host and plugin gates normalise the request path once (percent-decoding, collapsing empty segments, refusing `.`/`..`/trailing-dot segments), so encoded spellings such as `/%61dmin` cannot skip the loopback-host or plugin checks.
- `raw.send` accepts `requestId` (the snake_case `request_id` alias still parses), and the SDK sends the camelCase name.
- Records status is documented and validated as `done|failed|cancelled|pending`; `ok` is no longer accepted as a filter value.
- `W_PDF_OVERLAY_TOP` and `W_BARCODE_QUIET_ZONE` are part of the published warning contract; an unknown warning is written to the trace sink instead of only stderr.
- A WebSocket job is cached for idempotent replay only after its response was written and `start()` ran; a dropped socket retries and prints instead of replaying a job that never started.
- External print jobs fail with `E_TIMEOUT` after `queue_timeout` instead of waiting forever for a render slot.
- The pre-V8 template backup is written through a unique `create_new` temporary file, fsynced, chmod 0600 and atomically renamed; zero-length or unreadable leftovers no longer count as an existing backup.
- `printkit print` accepts existing PDFs with `--pdf FILE` or a `.pdf`/`%PDF-` input, passing them through without a renderer; CLI and JSON output are unchanged.
- `DELETE /admin/origins` removes one allowlisted origin, and `printkit origin list/add/remove` forwards through the live `serve.lock` administrator session.
- The preview e2e opts observability in through a temporary `plugins.toml` while still asserting the default core preset disables it.
- Embedded PDF pages and `doc.printPdf` use the PDF passthrough path (`prints_pdf`); macOS is verified, and the Windows WebView2 viewer is implemented but not yet verified on real hardware. Protocol `0.2.0` is a pre-1.0 release: methods removed before the first public release, such as `calib.set`/`calib.list`, carry no compatibility shim.
- `THIRD-PARTY-NOTICES.md` and `assets/licenses/component-licenses.*` are regenerated from the current Cargo/SBOM inventory; `gen-notices --check-cargo` reports no stale versions.
- The export view decodes its raw path segment exactly once, so a file whose name contains `%` is served without a second decode while traversal and trailing-dot names stay rejected.
- `/admin/shutdown`, SIGTERM and Ctrl-C share one bounded drain: they set the draining flag, refuse new print/raw/batch jobs, wait up to two seconds for reserved jobs, and log/report `drained`/`inflight` before the listener stops.
- Deleting an origin also revokes its tokens, and an existing WebSocket session from that origin is refused with `E_ORIGIN` on its next print or raw request.
- Export link signing is fail-closed: a failed or all-zero random key disables `/exports` link verification instead of using a predictable HMAC key.
- Desktop control login validates the `/admin/session` status and JSON shape before sending the administrator token to the port.
- The Tauri `snapshot` command now requires the business window, like the other read-only commands.
- `records.*` query `status` is the closed `RecordStatus` enum (`done|failed|cancelled|pending`) in the schema and SDK, not an arbitrary string.
- `HEAD /exports` answers from file metadata without reading the body.
- CLI `print --pdf` / `print -i *.pdf` with exactly one PDF now submits the original bytes through the `print_pdf` path instead of concat-rebuilding them; forms, signatures and metadata are preserved and 1000-page inputs avoid the reassembly cost. That fast path, and the WebSocket `doc.printPdf` path, keep the batch ceilings: more than 2000 pages or more than 128 MiB returns `E_TOO_LARGE`. Several inputs under `--batch` still merge into one PDF.
- A `POST /print/batch` with exactly one PDF part accepts up to the same 128 MiB single-PDF ceiling (mixed batches keep 64 MiB per part); the desktop pre-check now matches 128 MiB and preserves the service `E_TOO_LARGE` code and message.
- Batch `copies` is the only spooler copy count for the merged job. A batch part whose `output.copies` is not 1 is refused with `E_SCHEMA` and pointer `/documents/{index}/output/copies`, instead of silently printing one copy or multiplying twice.
- A deleted origin now blocks every method on an existing WebSocket session (records, printers, job status, logs as well as print/raw) with `E_ORIGIN` until it is re-added.
- A `--batch` invocation with exactly one PDF uses the byte-for-byte passthrough (no needless concat). The assembled batch is re-checked against the 128 MiB output ceiling, and each PDF part's page count is parsed once and reused. Merging several PDFs now reports `W_PDF_MERGE_METADATA_LOST` when an input carried AcroForm, Outlines or Metadata (page content is preserved).
- Records CSV export is one bounded query over the newest 5000 rows (the default retention ceiling) instead of paging and sorting an unbounded set in memory; contents, BOM and formula escaping are unchanged.
- Database migration V10 adds indexes for the records list/summary/stats filters (origin/status/printer/id plus a `julianday(at)` expression index); migration V11 adds `(origin,status,id)` so `records.list` with both filters orders by id without a temporary sort. `EXPLAIN QUERY PLAN` tests, upgrade tests and a 50k-row scale test cover them.
- `POST /print/batch` with one PDF now forwards the multipart `collate` value to the spooler (a `copies>1, collate=false` request is no longer silently collated). The single-PDF HTTP/CLI/`doc.printPdf` paths report a 0-page PDF with pointer `/pdf/pages`; a multi-PDF batch part keeps `/documents/{index}`.
- New WebSocket method `doc.printPdfBatch` prints finished PDFs as one job: parameters `pdfs: [{bytes, copies?=1}]` plus top-level `printer`, `copies`, `collate`, `requestId`; one PDF is byte-for-byte, several are merged with `W_PDF_MERGE_METADATA_LOST` when metadata is dropped; limits stay 128 MiB / 2000 pages. Protocol is now 21 request methods + `hello` (22 descriptors), with 18 SDK request wrappers.
- A duplicate `doc.printPdfBatch` `requestId` now returns the full `{jobId, pages}` result instead of only `jobId`; the cached pages come from the first response.
- `doc.printPdfBatch` validates each PDF once (the handler no longer pre-reads metadata), reports `W_PDF_MERGE_METADATA_LOST` once on the done event only, and rejects a combined WS payload over 32 MiB with `E_TOO_LARGE` (HTTP keeps the 128 MiB path).
- A 0-page PDF request (single or batch) returns `E_SCHEMA`; Windows `duplex=false` explicitly maps `ONE_SIDED`; macOS `printers.list` no longer falls back to PrintCore-only names that cannot be submitted.
- The desktop business window now defaults to 900×633 (1.422 ≈ 16:11.25; minimum 560×420),
  keeping a 212px sidebar while adding balanced vertical breathing
  room. Layout version 4 migrates the old 1024×720/1024×640 defaults once and
  preserves user positions; below 900px macOS uses the top tab bar.
- Overview now renders today’s successful print count (one successful print
  request counts as 1) with today’s done/failed/cancelled row counts; `pending` is a window summary field (optional/0), not a today counter. It also has four KPI cards,
  a seven-day print-pages chart (all tasks), a status distribution, frequent printers and the
  five most recent print tasks. Legacy `todaySheets` remains unchanged for old
  clients. Records shows the same print-request KPI and keeps tables inline at
  900/560/600 without horizontal scrolling.
- The desktop sidebar now uses an original inline fox mascot with one 0.7s
  ears/tail gesture (disabled by reduce-motion) and a footer showing the live
  `127.0.0.1:{port}` endpoint, with explicit unreachable/no-port/stopped states.
  Language selection moved from the shell to Settings.
- The sidebar footer is now the only ordinary UI surface that shows the service
  port. Overview and Settings no longer render a port row; config JSON, doctor
  output, sidecar diagnostics and logs keep the raw port for troubleshooting.
- Settings no longer exposes the Advanced / Diagnostics panel. Raw config,
  sidecar output and `printkit doctor` remain available through the config
  file, CLI and exported diagnostics.
- Plugin management now sits first in the Plugins navigation group and stays
  visible with every plugin off. Plugin rows show page chips (enabled pages
  open, disabled pages are inert, external/page-less rows show “No page”),
  and bridge permissions are plain-language allowed/denied/undeclared for
  external pages, with built-ins explicitly marked not applicable.
- The Jobs page now polls in place: cards keep their first-observation order
  and do not replay entry/breathing animations, selected details stay mounted,
  and only the first load shows a loading placeholder. The global page fade
  remains a one-time entry effect.
- Records filters now scope both the summary and the list. KPIs, success rate
  and today’s print counts match the visible rows; load-more discards stale
  pages and repeated clicks; export is disabled while loading or when the service is down.
- Settings now has a data-retention card. `[retention].days` defaults to 90
  (range 1–3650), controls JSONL logs and `print_record`, preserves explicit
  legacy `[log]`/`[records]` values, and is saved atomically through the desktop
  get/set bridge commands.
- Third-party notices are now an explicit best-effort disclosure: upstream
  copyright holders stay unchanged, missing packaged copyright lines are marked
  as such, Apache appendix templates are not treated as attribution, and no
  text claims full compliance.
- Public copy now says Logs is an event stream, keeps the single-window and page/copy details visible without using them in the KPI, and calls out the v1
  test-print default-paper and driver-metadata limits.

### Security

- `url` is updated to 2.5.9 (with `form_urlencoded` 1.2.3), fixing RUSTSEC-2026-0592: `Url::origin()` parsed nested `blob:` URLs recursively, so a crafted URL could overflow the stack of the thread that called it. The crate is reached through `reqwest`, `salvo_core` and `tower-http`, so this affects both the CLI and the desktop sidecar. Root and desktop `Cargo.lock` are updated.

## [0.1.0] - 2026-11-14

First public-ready cut of the local declarative print toolkit. Pre-1.0:
interfaces may change, but `schema/document-v1.json` and
`schema/protocol-v1.json` remain the compatibility boundary for their major
versions.

### Added

- Single `printkit` binary: `pdf`, `print`, `printers`, `papers`, `raw`,
  `preview`, `serve`, `doctor`, `logs`, `stats`, `plugin`, `config`, `schema`
  and `version`.
- Declarative JSON document model with `document-v1` JSON Schema validation,
  explicit pages, paper sizes, blocks, copies and output routing.
- macOS WKWebView and Windows WebView2 render hosts; PDF output, graphical
  printing, raw device bytes and preview windows.
- Loopback HTTP/WebSocket service with exact `Origin` allowlisting, bearer
  tokens stored as SHA-256 digests, `print`/`raw`/`admin` scopes, CSRF-protected
  JSON control plane and read-only `/exports/<path>`.
- Built-in capability plugins (`records`, `observability`, `calibration`) with
  a stable `plugin-api` contract, a manifest/`plugins.toml` registry and a
  fail-open job-hook runner. `records.summary` joins the protocol method table;
  the CLI gains `printkit plugin list|enable|disable|config|repair`, and the
  service gains `/admin/plugins`.
- Single business window: Overview, Printers, Plugins and Settings always,
  plus Records, Logs/Jobs and Calibration when their plugins are enabled;
  macOS `Overlay` titlebar and the D5 token layer.
- TypeScript SDK (`@printkit/sdk`, a repository-local package) with generated
  protocol types, browser bundle, Node client and `pluginFamilies`-aware UI
  flags.
- Trace/logging, retention, usage records, calibration and diagnostics export,
  owned by the corresponding plugins; `printkit plugin list` reports their state.
- Acceptance gates 0–17: schema/TS drift, geometry, table pagination, S16
  release checks, pairing, WebSocket limits, URL-fetch guard, rustdoc and the
  plugin core-only/concurrency/retention/fail-open/S2-absence suites.

### Changed

- New installs use the `core` preset: records, observability and calibration
  are off until enabled. Existing `plugins.toml` files migrate once to
  `preset = core` with a `plugins.toml.bak-core` backup. A corrupt file is left
  untouched; the loader falls back to `plugins.toml.bak` for diagnosis and the
  service runs core-only until the file is repaired.
- `welcome.pluginFamilies` lists the active plugin method families;
  `welcome.capabilities` remains only as an empty compatibility field.
- The desktop app has one `business` window (minimum 560×420; the current default
  is 1024×640, see Unreleased). The
  developer window, `--dev` flag, extra data windows and `open_window` command
  are removed.
- Windows shipping is preview-only: installers are built by the release
  workflow, unsigned and not yet installed on real Windows hardware.
- Duplex requests are rejected with `E_UNSUPPORTED` until a physical duplex
  device is verified; jobs never silently print single-sided.
- README, CLI guide, browser guide, SDK guide and desktop docs describe the
  plugin model, page ownership and the single-window shell.

### Removed

- `template.*` methods and the `template` capability, the `template` table
  (dropped by `V8__drop_template.sql` after a `printkit.db.bak-before-v8`
  backup), template SDK helpers and the designer UI plus its frontend
  dependency. The `print_record.template` column stays as a historical template
  name for `--group template`.

### Fixed

- Hello frame requires the `type` discriminator in the published protocol
  schema and generated TypeScript.
- URL fetching pins checked DNS answers, never routes through a system proxy
  and keeps the private-fetch opt-in to debug builds.
- Desktop license texts no longer carry project attribution in third-party
  files; the remaining per-component notice gap is documented in
  `THIRD-PARTY-NOTICES.md`.
- Store reads recover a WAL database whose side files were removed without
  opening a writable handle.

### Security

- `serve` binds `127.0.0.1` only; `/admin` uses an owner-only token file,
  constant-time comparison, HttpOnly/SameSite session cookies and exact
  `Origin` checks.
- Release gates reject dirty worktrees and verify the assembled `BUILD-INFO`
  commit against HEAD.
