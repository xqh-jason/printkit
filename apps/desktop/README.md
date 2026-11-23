# printkit desktop

Optional macOS-first desktop app and the only UI for the local printkit
service. It starts the service automatically and reaches it only over
`http://127.0.0.1`. Installation is covered by the
[root README](../../README.md#install); this document covers building,
developing and testing the app.

The bundle is `printkit desktop.app` (identifier `dev.printkit.desktop`); it
carries the `printkit` binary as a Tauri `externalBin` sidecar.

## What it is

The app is one resizable business window with these pages:

| Page | Content | Visibility |
|---|---|---|
| Overview | service state, four KPIs, seven-day print-pages chart, status split, frequent printers, recent print tasks, recovery actions | always |
| Records | usage records, row details (error/trace/template/tags), filters, CSV export | records plugin on |
| Logs, Jobs | trace log and job observability | observability plugin on |
| Printers | installed queues, paper sizes, test print and PDF-file print | always |
| Plugins | enable/disable, presets, recovery, config | always |
| Settings | service, web-access origins, data retention, logging, about | always |

Navigation has two groups: core is Overview, Printers and Settings; plugins
starts with the always-visible Plugin management entry, followed by Records,
Logs and Jobs while their owners are enabled. Plugin rows show page
chips: enabled pages open their view, disabled pages are inert, and external or
page-less plugins show “No page”. Built-in plugins report bridge permissions as
not applicable; external pages show allowed/denied/undeclared for self-config,
printers and records summary.

The Jobs page polls without replaying card animations: `orderJobs` keeps the
first-observation order, the selected detail is resolved by id and is not
remounted, and `initialLoading` gates only the first request so a failed poll
does not flash a loading placeholder. The shared `.page` fade still plays once
on entry.

The Records filters (`from/to`, status, printer, tags) are passed to both
`records_summary` and `records_list`, so KPIs, success rate and today’s print
counts describe the visible rows. `loadMore` guards `generation`, `loadingMore`,
`dataDown` and the cursor; stale pages are discarded. Export is disabled while
loading or when the service is down.

Navigation uses a 120ms out-in fade/translate (the old page leaves before the
new one enters), and every page keeps its previous data visible while a refresh
is in flight. Shared skeletons wait 130ms before appearing, so a fast request
never flashes one; slow first loads show a stable placeholder. Top-level
actions expose `aria-busy` and keep Element Plus loading spinners inline.
Runtime detail fields are labelled Runtime info; the plugin card's id/version
block is Plugin details.

The print/choose/clear buttons are disabled while a job is queued or printing,
until the 60-second poll timeout. Each new Print PDF click creates a random
attempt nonce and derives `Idempotency-Key` from the file (name/size/mtime),
printer, copies and that nonce. Two clicks on the same file/printer/copies are
therefore two jobs, even inside the service's 10-minute idempotency window.
After 60 seconds without a terminal state the UI says the state is unknown,
keeps the key, and offers Refresh status or Print again; printing again is an
explicit new attempt/key, so the unknown old job never locks the page.
Success, failure, cancel and network submit errors drop the key, so an ordinary
retry after a failure is a new job. Choose PDF and both print actions require
an owned, reachable service and a selected printer; stopped and external
services show the reason instead of an enabled write action.

At the 900px window width Records keeps error code, copies, template and trace
visible; duration moves to the detail view. The table uses compact cell padding
instead of hiding those columns. Test print is enabled only for a selected
printer on an owned, running service; external read-only and stopped services
explain why the button is disabled. Copies accept the service range 1-999; the
fixed A4/default-paper limitation remains.

The desktop sends one PDF to `/print/batch`; the service detects a single
application/pdf part and prints it byte-for-byte through the same passthrough
path as the CLI and `doc.printPdf`, so no merge warning is raised for one file.
If a completed job reports `warnings` (for example a future mixed batch), the
PDF card shows each code/message as a non-blocking warning; terminal success
still requires `status: done`.

A PDF submission is not reported as success when the HTTP request returns.
`print_pdf_bytes` returns the service `jobId`, and the page polls the
business-window `job_status` command (`GET /admin/jobs/{jobId}`) until
`queued`/`printing` becomes `done`, `failed` or `cancelled`; failures show the
error code (for example `E_PRINTER_NOT_FOUND`), and a cancel button posts
`/admin/jobs/{jobId}/cancel`. The desktop does not synthesize a success state.

On the first owned start, if `<data_dir>/plugins.toml` does not exist, the shell
writes a complete sibling file and publishes it with an atomic no-replace link,
so a concurrent writer or a user-created file is never truncated. It writes a
core preset with `observability` and `records` explicitly enabled, so Jobs and
Records are visible immediately. Those are the only built-in optional plugins;
Calibration and Templates no longer exist. An existing
`plugins.toml` is never rewritten, and the CLI/`serve` defaults remain core-only.
Because the file lives in the shared data directory, a later CLI `serve` on the
same data directory sees the same observability/records enablement.

The Printers page prints a PDF chosen in the WebView through the local service's
`multipart/mixed` `/print/batch` route: the first part carries printer/copies,
the second is the PDF, and the engine submits the PDF path without re-rendering.
The desktop refuses files over 128 MiB or without a `%PDF-` header, matching the
service's single-PDF ceiling; the service still applies its 2000-page and
`E_TOO_LARGE` bounds. It sends base64 over IPC because Tauri arguments are JSON.

Settings has a Web-access card backed by `GET/POST/DELETE /admin/origins`. It
lists allowed browser origins, adds one at a time and removes after confirmation.
No token is shown; an externally attached service is read-only there.

Records rows open a detail drawer with status, error code, trace id, template,
tags, printer and duration. Failed records explain that reprint is unavailable
because the original document is not stored, and offer copy-as-JSON instead of a
fake reprint button.

The Settings data-retention card edits `config.toml` `[retention].days`
(default 90, range 1–3650), which applies to both JSONL logs and
`print_record`. It calls the `get_retention_days`/`set_retention_days` bridge
commands, shows the value source, log/records components and restart hint, and
uses an atomic config write that preserves unrelated keys.

## Overview contract

Overview queries `records_summary`, `records_stats` (`groupBy=day` and
`groupBy=printer`) and `records_list(limit=5)` through the Rust bridge; it also
lists system printers. The seven-day window is today plus the previous six days,
`from=localDay(-6)`, `to=localDay(+1)` (the service upper bound is exclusive),
and missing days are filled with real zeros. The status split derives `done`
from `total-failed-cancelled-pending`; a missing `cancelled` count is shown as
unavailable, never as zero. Failure rate is `failed / (done + failed)`; cancelled
and pending rows are not in the denominator. Frequent printers are ranked by task count, then
sheets, then key.

The first KPI is today’s successful print count: `todayDone` counts today’s
`status = done` rows once per successful print request, regardless of pages or
copies. `todayDoneSheets` is a deprecated compatibility field and is not used by
the KPI. `todayFailed`/`todayCancelled` are row counts; the window `pending`
field has no today-specific counterpart. Failed/cancelled/pending rows are
excluded from the successful count. Legacy `todaySheets` remains the today total
across all statuses for old clients. Records uses the same print count;
pages/copies remain table details. KPIs/status columns stay on one row at
900/560/600. Printer duplex/paperSource values are driver metadata only; v1
print requests remain single-sided and test print uses the default paper.

When records is off or the service is stopped/unreachable, no `records_*` call
is made. The page has explicit off, down, loading, empty and error states. Layout
is one column at 900px and two columns at 1024px (the CSS breakpoint is 960px).

Plugin-owned pages disappear when their plugin is off. All pages use
`@printkit/i18n` (`packages/i18n`) and switch between English and Simplified
Chinese; the shared catalogue is checked by `check.mjs`.

## Brand and footer

The sidebar brand is an original inline fox mascot, not a third-party icon. It
plays one 0.7s ears/tail gesture on first paint and again on hover; both the
`data-motion="reduce"` switch and `prefers-reduced-motion` disable it.

The sidebar footer shows the service endpoint. A reachable owned or external
service displays `127.0.0.1:<actual port>` (external adds a short label);
unreachable, missing-port and stopped states have their own copy. A click copies
the address when one exists, otherwise it opens Settings. The configured port is
never rendered as a live host. The language selector has moved to Settings; the
shell no longer contains `LocaleSelect`.

The footer is the only ordinary UI surface that shows the port. Overview and
Settings no longer render a port row or a diagnostics panel; data directory,
PID, lock and version live behind the collapsed Runtime info block. Logs and exported
diagnostics retain the raw port for troubleshooting. `printkit doctor` stays
available on the CLI.

The desktop app seeds a first owned start with `observability` and `records`
enabled, so Jobs and Records are visible without a trip to Plugin management.
An existing `plugins.toml` is never overwritten, and the CLI/`serve` default
stays `core` with both rows off. Disabling a plugin on the Plugins page keeps
its data and changes only its navigation entry and routes.
There is no developer window, `--dev` flag or extra
data window: every page above lives in this single `business` window.

macOS uses an `Overlay` titlebar with `hiddenTitle`, inset traffic lights at
`12,16`. It is one 50px header with a 96px traffic inset and content starting at
50/52px. At 900px+ the sidebar owns the logo and the header shows only the
13.5px/600 breadcrumb with a single toggle on its right (no duplicate brand).
Without a sidebar the header carries the toggle and page tabs: `tight` at 660px
keeps the logo, `crowded` at 560px hides it. Header and sidebar both use
`--pk-bg` with no 1px bottom border; the header is a
`data-tauri-drag-region`/`start-dragging` drag area with interactive controls
opting out.

The sidebar brand shows the original fox mascot plus the `printkit` wordmark
at the 212px default; 168/184px keep only the mascot and 192px restores the
wordmark. There is no collapsed rail: the window never shrinks the sidebar below 168px, legacy
`printkit.sidebarCollapsed` is removed on startup, and a persisted width ≤64px
migrates back to the default. The traffic lights sit at `x=12,y=27` on the
brand line.

The OS title comes from `shell.windowTitle` and is set when the locale changes;
zh-CN uses the localized printkit console title, en uses `printkit`. `LocaleSelect`
now lives in Settings; the shell has no locale selector. `@printkit/i18n` must
be consumed via `link:../../packages/i18n`, never copied with `file:`.

The app is a separate product: `apps/desktop` is its own Cargo workspace and
depends on no `printkit-*` crate.

## Build

```sh
pnpm --dir apps/desktop install
pnpm --dir apps/desktop tauri build
```

The Tauri `beforeBuildCommand` is `pnpm build && pnpm sidecar`. `pnpm sidecar`
compiles `printkit` from the repository workspace and stages it at
`src-tauri/binaries/printkit-<target-triple>` for `externalBin` bundling. The
macOS arm64 artifact is
`src-tauri/target/release/bundle/dmg/printkit desktop_<version>_aarch64.dmg`;
the bundle is ad-hoc signed (`signingIdentity: "-"`), not notarized.

Windows builds read `tauri.windows.conf.json`, which adds NSIS and MSI targets
and downloads the WebView2 bootstrapper. The release workflow builds them on
`windows-latest`; both are unsigned and no installer has been run on a real
Windows host, so Windows support is unverified.

## Development

```sh
./scripts/dev.sh desktop
```

That stages a debug sidecar and points the app at the development data
directory and port. The manual equivalent is:

```sh
bash apps/desktop/scripts/prepare-sidecar.sh debug
PRINTKIT_DATA_DIR=/tmp/printkit-dev PRINTKIT_PORT=19080 \
  pnpm --dir apps/desktop tauri dev
```

A development session uses `PRINTKIT_DATA_DIR`, so it cannot edit the installed
application's database by accident.

## Service lifecycle

The only source of truth for a running service is `<data_dir>/serve.lock`. The
app reports `stopped`, `running(owned)` when the lock pid is the child it
spawned, and `running(external)` for any other live lock. An external lock is
trusted only when its pid belongs to a `printkit` executable; otherwise it is
reported stale. Write commands (test print) require `running(owned)`; an attached
external service is read-only. The admin token is sent only after
`/admin/session` returns a printkit-shaped body.

Stopping an owned service goes through the Rust control client: it reads the
0600 `admin-token`, calls `POST /admin/login`, and sends `POST /admin/shutdown`
with the session cookie and CSRF token. If the service does not exit within five
seconds, the app terminates its own child and removes the lock only when it
still names that process.

## Window geometry and layout

Window defaults live in `src-tauri/src/layout.rs`; the single business window is
resizable with a 900×633 default (1.422 ≈ 16:11.25, not 3:2; the 212px
sidebar and two-column content have balanced breathing room) and a shared 560×420 minimum. Layout
version 4 resets the old 1024×720/1024×640/560×420/660×600/663×458 defaults once while
preserving position and maximized state; custom sizes survive. The last
size/position is stored in `desktop-window.json` under the app config directory
(`PRINTKIT_LAYOUT_FILE` overrides the path). Navigation follows the same order: the
breakpoint `SIDE_AT` is 900px, so the default window opens with a 212px sidebar;
below 900px the sidebar is replaced by a top tab bar. The breakpoint uses
`window.innerWidth`, not `documentElement.clientWidth`, so a Windows scrollbar
cannot turn exactly 900px into 892px and wrongly select the top tabs. Below
860px the default sidebar width drops to 184px (the 212px default applies at or
above it); a pinned sidebar stays between 168 and 320px and never collapses to
a rail. Ctrl/Cmd+Shift+S switches navigation between the sidebar and top tabs.

## Permissions and security

`src-tauri/capabilities/default.json` applies to the `business` window and
allows only core commands plus setting the localized window title. There is no
`fs`, `http`, shell or opener permission; the sidecar is spawned from Rust,
which needs no JS capability. The
window CSP allows `connect-src ipc: http://ipc.localhost` only, so the webview
has no direct network access.

The token stays inside `src-tauri/src/control*`, is used once and dropped, and
never enters a Tauri command return value, IPC, Vue, HTML or a log. The desktop
UI has no token issuance or authorization entry: `POST /admin/tokens` remains a
loopback JSON API with no desktop UI, for external integrations and tests. The
service binds `127.0.0.1` only.

Both platforms also enforce the same policy at command level: page data
commands (records, logs, exports, printers, paths, plugins, retention; reads
included) require the `business` window label, and mutating commands are
rejected by `require_owned` when the service is external. The Vite dev server sends its own CSP header
(including the HMR websocket), so a development page is not less restricted
than the packaged webview.

## Code signing

Local builds are ad-hoc signed (`signingIdentity: "-"`); `release.sh` and
`s16_accept.sh` accept either an ad-hoc signature (printed as "local only") or a
Developer ID signature. A Developer ID bundle must pass `spctl`; notarization
still needs the Apple credentials and `notarytool` in the release environment
(`PRINTKIT_NOTARY_*`), which is intentionally not faked here. Windows
Authenticode is still pending a certificate.

## Smoke tests

```sh
bash tests/scripts/desktop_smoke.sh
pnpm --dir apps/desktop test:desktop
```

`desktop_smoke.sh` builds the CLI if needed and checks the shell contract:
sidecar start, lock and port, administrator login, the logs and records APIs,
shutdown, and the desktop manifests.
