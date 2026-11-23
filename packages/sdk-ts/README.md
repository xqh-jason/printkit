# @printkit/sdk

TypeScript client for the printkit print service: one WebSocket session, typed
requests, and the capability flags a page needs to hide unsupported entries.

## Install

`@printkit/sdk` is a repository-internal package; it is not published to a
registry. Install it from the repository path:

```sh
pnpm add <path-to-repo>/packages/sdk-ts
```

Or run `pnpm pack` inside `packages/sdk-ts` and install the generated tarball.
Node 20.19 or newer (`engines`); CI runs it on Node 22. npm resolves the same
`package.json`, but the repository scripts and CI use pnpm.

## Quick start

### Node

```ts
import { createEngine } from "@printkit/sdk";

// Local script: no url, no token. The SDK finds the running service.
const engine = await createEngine();

const info = await engine.info(); // { engine, protocol, platform, capabilities }
const printers = await engine.listPrinters();
await engine.print(document); // → { jobId }
```

### Browser

```ts
// Pass the page origin once the desktop app whitelisted it; keep the token out.
const engine = await createEngine({ origin: window.location.origin });
```

`createEngine` sends the `hello` handshake and then `sys.info`, so `engine.ui` is
usable before the first render. `engine.info()` returns that cached result;
`engine.info(true)` asks the service again.

## Endpoint and token

`url` and `token` are both optional.

| Setting | Browser | Node |
|---|---|---|
| `url` omitted | `ws://127.0.0.1:19080/print` | port from `serve.lock`, else `19080` |
| `url` given | used as is | used as is |
| `token` omitted | `print` from a whitelisted origin | `print` from the local machine |

Under Node the SDK reads `serve.lock` from `PRINTKIT_DATA_DIR`, or from the
platform data directory the service uses (`~/Library/Application Support/printkit`
on macOS, `$XDG_DATA_HOME/printkit` or `~/.local/share/printkit` on Linux). The
field is only read when the service is running; a missing or unusable lock falls
back to port `19080`.

A handshake without a token never receives `raw` or `admin`. Those methods need
a token issued by an administrator.

## Methods

One row per SDK method; the `protocol` column is the wire method name, taken from
`schema/protocol-v1.json`; `createEngine` is the only entry that sends a frame
instead of a request envelope, and `hello` is the name the contract gives it.

| SDK | protocol | notes |
|---|---|---|
| `createEngine` | `hello` | handshake with optional token and origin, capability negotiation |
| `info` | `sys.info` | cached by `createEngine`; `info(true)` refreshes |
| `listPrinters` | `printers.list` | |
| `paperSizes` | `printers.papers` | parameter `name` |
| `print` | `doc.print` | the document is the parameter object |
| `printBatch` | `doc.printBatch` | parameters `docs` (`Document[]`, 1–100), `printer`, `copies`, `collate`; it never carries PDF parts. `copies` is the only spooler count for the merged job; every document must set `output.copies` to 1 (otherwise `E_SCHEMA` with `/documents/{index}/output.copies`) |
| `printPdf` | `doc.printPdf` | parameters `bytes` (base64), `printer`, `copies`; the bytes are submitted byte-for-byte without rendering, `collate` is always `true`, and 0-page PDFs are `E_SCHEMA` with pointer `/pdf/pages` |
| `printPdfBatch` | `doc.printPdfBatch` | parameters `pdfs` (`{bytes, copies?=1}` 1–100), `printer`, `copies`, `collate`; one PDF is byte-for-byte, several are merged; per-PDF `copies` must be 1. A metadata-loss warning is reported once on the job's done event; the response warnings array stays empty. WS messages are capped at 32 MiB, so an oversized frame closes the connection (use HTTP /print/batch for the 128 MiB path) |
| `preview` | `doc.preview` | the document is the parameter object |
| `exportPdf` | `doc.exportPdf` | parameters `document`, `path`; result adds `url`/`expiresAt`, a signed 10-minute link for `GET /exports` |
| `sendRaw` | `raw.send` | parameters `printer`, `bytes` (base64), optional `requestId` for retry-safe resubmission |
| `jobStatus` | `job.status` | parameter `jobId` |
| `cancelJob` | `job.cancel` | parameter `jobId`; always available |
| `previewPage` | `preview.pages` | parameters `windowId`, `pageIndex`, `dpi` |
| `closePreview` | `preview.close` | parameter `windowId` |
| `myRecords` | `records.list` | `from`, `to`, `tags`, `printer`, `status` (`done`, `failed`, `cancelled` or `pending`), `cursor`, `limit` |
| `myStats` | `records.stats` | `groupBy` plus the same filters |
| `mySummary` | `records.summary` | KPI cards plus the same filters; `todayDone` is the count of today’s successful print requests (one per request), `todayFailed`/`todayCancelled` are today record counts, and `todaySheets` is legacy all-status sheets. `pending` is a required window total in v1 (read an absent value from an older service as 0; no today-specific counterpart field); `done = total - failed - cancelled - pending`; `failureRate = failed / (done + failed)` excludes cancelled and pending. `todayDoneSheets` is a deprecated compatibility field. |
| `exportRecords` | `records.export` | returns CSV bytes; bounded to the newest 5000 rows (the default retention ceiling) and ordered by id |

`doc.printBatch` and `POST /print/batch` are separate routes. The SDK method sends documents only.
The HTTP route is `multipart/mixed`: one `application/json` metadata part followed by
`application/pdf` or `application/json` (Document) parts. Metadata:

| field | type | default | meaning |
|---|---|---|---|
| `printer` | `string?` | system default | destination printer |
| `copies` | `number?` | `1` | 1–999, applied once at the spooler level |
| `collate` | `boolean?` | `true` | `true` collates sets; `false` leaves page order to the spooler |

One metadata part plus exactly one PDF part is passed through byte-for-byte, and `copies`/`collate`
still apply. Other combinations merge into one PDF; several PDFs may raise
`W_PDF_MERGE_METADATA_LOST` when an input carries AcroForm, outlines or metadata. The merged path
still uses one spooler submission, and a PDF part error points at `/documents/{index}`.

`records.*` use the token's origin; the service filters, the SDK does not.

`trace.subscribe`, `trace.tail` and `logs.export` are admin-only protocol methods
without SDK wrappers: `onTrace` only delivers events after an admin session
subscribes on the wire, and diagnostics export is a loopback HTTP call. The table
covers every method the SDK sends.

## Capability-gated entries

`engine.ui` is derived from `sys.info.capabilities` for platform abilities and
from `welcome.pluginFamilies` for plugin-owned method families. Render a control
when its field is true; never branch on `info.platform`.

| `engine.ui` | source | effect |
|---|---|---|
| `ui.duplex` | `capabilities.duplex` | duplex option in the print form |
| `ui.paperSource` | `capabilities.paperSource` | paper-tray selector |
| `ui.cancelQueued` | always `true` | cancel a queued job |
| `ui.cancelSubmitted` | `capabilities.cancelSubmitted` | cancel an already submitted graphic/PDF job |
| `ui.cancelRaw` | `capabilities.cancelRaw` | cancel an already submitted raw job |
| `ui.preview` | `capabilities.preview` | preview window entry |
| `ui.rasterFallback` | `capabilities.rasterFallback` | raster fallback entry |
| `ui.raw` | `capabilities.raw` | raw channel entry |
| `ui.batch` | `capabilities.batch` | `printBatch` entry |
| `ui.printPdf` | `capabilities.pdfPassthrough` | `printPdf` entry |
| `ui.renderWorkers` | `capabilities.renderWorkers` | render pool parallelism |
| `ui.observability` | `pluginFamilies` includes `observability` | `trace.*`/`logs.export` entries |
| `ui.records` | `pluginFamilies` includes `records` | `records.*` entries |

A fresh install uses the `core` preset, so `ui.observability` and `ui.records`
are `false` until `printkit plugin enable <id>` (or the desktop Plugins page)
turns the plugin on.

`cancelJob` exists on every build: cancelling a queued job works on every
platform. `cancelSubmitted: false` hides only the graphic/PDF "cancel submitted"
entry and `cancelRaw: false` only the raw one; neither removes the method.

### Methods that refuse locally

`printPdf` is the one method that refuses locally: when `pdfPassthrough` is
false it rejects with `E_UNSUPPORTED` without sending a request, because the
service cannot print PDF bytes without rendering them. `preview` follows the
same shape: with `capabilities.preview` false it rejects locally with
`E_UNSUPPORTED` without sending a request; fall back to `exportPdf` and hand the
file to the user.

### Exports and warnings

`exportPdf` returns these fields:

| field | type | meaning |
|---|---|---|
| `path` | `string` | relative to the data directory (`exports/...`) |
| `absolutePath` | `string` | canonical file path on the service machine |
| `pages` | `number` | pages in the written PDF |
| `bytes` | `number` | file size |
| `url` | `string?` | signed capability URL for this exact file |
| `expiresAt` | `number?` | Unix seconds after which `url` stops working (10 minutes after the call) |

Open the export only through `url`, for example in a browser page, which cannot
navigate to `file://`. The route requires a loopback `Host`, confines the
canonical path to `exports/` (traversal and symlink escapes are rejected), and
answers `application/pdf` with `Content-Disposition: inline`. An unsigned,
expired or wrong-file link returns `403 E_AUTH`; never construct or log a bare
`/exports/<path>` URL.

Warnings (`W_*`) arrive on successful responses and are delivered to
`engine.onWarning`. Job progress is pushed by the service to `engine.onJob`.

## Types

`src/types.ts` is generated by `ts-rs` from `crates/core` and `crates/protocol`
and then projected against `schema/document-v1.json`; `src/limits.ts` is
generated from `crates/core/src/limits.rs`; `src/methods.ts` is generated from
`schema/protocol-v1.json`. Do not edit any of them. `pnpm build` regenerates all
three and then verifies them against the contracts and this README.

The projection makes every document field the schema leaves outside `required`
optional in TypeScript, so a minimal document compiles:

```ts
const document: Document = { version: 1, pages: [{ spec: {} }] };
```

A batch that rebuilds PDFs reports `W_PDF_MERGE_METADATA_LOST` when an input carried AcroForm,
Outlines or Metadata; page content is preserved, and the warning is reported once per job rather
than once per PDF part. `JobStatus`, `JobEvent` and `Response` likewise make
`warnings?` optional: the service omits an empty array, so `status.warnings?.length` is the correct read.
Integers are `number` because JSON has no `i64`. Query-style parameters are
accepted as `Partial<...>`, so callers pass only what they set.

`LIMITS` (`src/limits.ts`) exposes the stable caps as a frozen object, so an
SDK can preflight a document, batch or WebSocket message instead of waiting for
`E_TOO_LARGE`: `maxDocumentBytes`, `maxWsMessageBytes`, `maxFetchUrls`,
`maxBatchDocs`, `maxBatchPages`, `maxBatchRequestBytes`, and the rest.

`cargo test -p printkit-protocol --features typescript` runs the export, the
document projection and the limits generator, then compares every committed
file byte for byte; only `UPDATE_TS=1` rewrites them.

**Contract note:** `records.list` returns `{ records, nextCursor }`. The SDK
returns that shape unchanged, because the method is a thin wrapper and its
parameter and field names must match `protocol-v1.json`.

## Errors and timeouts

Failures are `EngineError` with the wire `code` (`E_AUTH`, `E_SCHEMA`, ...),
`message`, and `detail`. Connection failures and request timeouts use `E_CONNECT`
and `E_TIMEOUT` locally; the rest come from the service. `createEngine` accepts
`connectTimeoutMs` (default 5000) and `requestTimeoutMs`, a fallback for every
method; when set it overrides the per-method default.
`heartbeatMs` (default 240000) sends `sys.info` periodically so the service's
five-minute idle close does not drop an otherwise quiet connection; set it to
`0` to disable, and `close()` clears the timer.

`print`, `printBatch`, `printPdf`, `printPdfBatch` and `sendRaw` accept a per-call `requestId`.
Duplicate suppression is best-effort, not a delivery guarantee. The service
remembers successful `(method, origin, requestId)` keys for at most ten
minutes, 1024 entries and about 1 MiB in one process lifetime. A duplicate
inside that budget returns the original job id without reserving or printing
again; after 600 seconds, after an entry is evicted, or after a service restart
the same key starts a new job and prints again. Printing has a physical side
effect, so when a repeated sheet is unacceptable, keep your own business id and
reconcile returned job ids instead of relying on this cache.

`exportPdf`, `previewPage` and `print` accept a per-call `timeoutMs`. A
call-level value wins over `requestTimeoutMs`, which wins over the per-method
default:

| method | default | reason |
|---|---|---|
| `exportPdf`, `previewPage` | 120s | renders before answering; the host budget is `10s × (pages + 1)` |
| every other method | 30s | submits work and returns a job id or handle |

The service keeps working after a local `E_TIMEOUT`: `exportPdf` may still
finish and write the file, and `print`/`sendRaw` may still submit a job. A
timed-out call therefore loses the `jobId` (find the job through `myRecords`)
and its late response is dropped, never delivered to a later request. The 200
page maximum budgets about 2010s, so pass a larger `timeoutMs` for big
documents: `exportPdf(document, path, { timeoutMs: 2_100_000 })`.

The SDK never reconnects on its own: a session carries a token, an origin
binding and granted scopes, so replacing it stays with the caller. Call
`engine.close()` when done, or `await engine.reconnect()` to replace a dead
session; reconnect repeats the handshake and `sys.info`, keeps the
`createEngine` options and every `onJob`/`onTrace` handler, and fails pending
calls from the old socket with `E_CONNECT`.

## Browser notes

- The page's origin must be whitelisted by the desktop app; pass it to
  `createEngine` and omit `token`.
- Chrome was tested with an `https://` page connecting to
  `ws://127.0.0.1:19080`: loopback is treated as trustworthy. Safari, Firefox,
  enterprise proxies and Chrome Private Network Access are untested; if a
  connection is blocked, evaluate `wss` with a certificate.
- If the page sets a `connect-src` CSP, allow `ws://127.0.0.1:19080`.
- The signed `url` from `doc.exportPdf` works as a top-level page, so link to
  that capability URL (not a bare `/exports/<path>`) when a PDF must leave the app.
- One tab owns one session; call `engine.close()` on unload.

## Tests

```sh
pnpm build         # gen:types, gen:methods, tsc, verify
pnpm test          # build, then the mock-service suite
pnpm test:browser  # esbuild browser bundle; rejects static node: imports
pnpm test:types    # compile a no-argument consumer with tsc --strict
pnpm check:types   # regenerate and fail on any drift
```
