# printkit CLI and local service

English | [Chinese](cli.zh-CN.md)

One binary does everything the desktop app drives: render a JSON document to
PDF, submit it to a printer, send raw device bytes, open a preview window, and
run the loopback HTTP/WebSocket service. The desktop app is optional; every
command below works without it.

## Install layout

Extract both archives into the same directory, for example `~/printkit/`:

```text
printkit-macos-arm64.tar.gz
└── printkit                     the only executable

printkit-schema-macos-arm64.tar.gz
├── schema/
│   ├── document-v1.json         document contract
│   └── protocol-v1.json         WebSocket / HTTP contract
└── assets/
    ├── fonts/OFL.txt            the bundled font licence
    └── licenses/                third-party licence texts
```

There is no installer, no service registration and no `sudo`. Run the binary
from wherever it was extracted:

```sh
./printkit version
./printkit doctor
```

A release binary resolves `schema/` **next to the executable** and nowhere else.
It refuses to fall back to the machine that built it: without the extracted
`schema/` directory it exits non-zero and tells you to unpack the archive or use
`printkit schema --out DIR`. A build from source uses the repository checkout.

The macOS desktop app is a separate download (`printkit-desktop-macos-arm64.dmg`)
that carries its own sidecar; Windows ships no CLI archive.

## Document schema

A document is one JSON object covering pages, paper, blocks, copies and output
routing, validated against `schema/document-v1.json` before rendering. `pdf`
accepts `-` for stdin and stdout, so a generator can pipe through it:

```sh
generate-document | ./printkit pdf -i - -o - > out.pdf
```

Locate or write the contracts (`--path` and `--out` are mutually exclusive; one
is required):

```sh
./printkit schema --path        # print the directory that supplies them
./printkit schema --out DIR     # write document-v1.json and protocol-v1.json
```

## Commands

| Command | Purpose | Notables |
|---|---|---|
| `pdf` | Render a document to a PDF file | `-i` input or `-`, `-o` output or `-`, `--dump-html FILE` keeps the intermediate HTML |
| `print` | Submit a document or an existing PDF to a printer | `-p` printer (default printer when omitted), `-n` copies, `--batch`; one PDF (`--pdf FILE` or `-i *.pdf`) is submitted byte-for-byte and is limited to 128 MiB and 2000 pages, several inputs under `--batch` are merged |
| `raw` | Send device bytes unchanged | `-f` file or `-`, `-p` printer, `--title` spooler title |
| `printers` | List printers | |
| `papers` | List the paper sizes one printer accepts | `-p` printer (required) |
| `preview` | Open a document in a native preview window | Free data directory: runs the service itself; busy: forwards to it |
| `serve` | Run the loopback HTTP/WebSocket service | No arguments |
| `doctor` | Check renderer, data directory, configuration and logs | |
| `logs` | Read local JSONL trace events (owned by the `observability` plugin) | `--trace`, `--stage`, `--field`, `--since`, `--follow` |
| `stats` | Aggregate the local usage table (owned by the `records` plugin) | `--group tag\|template\|printer\|day`, `--from`, `--to`; `template` is the historical `print_record.template` name, not a template feature |
| `origin` | List and change the allowlisted browser origins | `list`, `add`, `remove`; validates the live `serve.lock` before logging in |
| `plugin` | List and change built-in plugins | `list`, `enable`, `disable`, `config`, `repair`; needs a running service except `list`/`repair` |
| `config` | Show effective values and where each came from | `--path`, `--show-source`, `--validate` |
| `schema` | Write or locate the published contracts | `--path` or `--out DIR` |
| `version` | Print the version | |

Every command except `serve` and `schema` accepts `--json` and then writes one
JSON object to stdout. `printkit <command> --help` is the authoritative option
list for the build in front of you.

`print --batch` treats every input as one job: a top-level JSON array is
expanded into one document per element, and the inputs are merged into a single
spooler job. The exception is exactly one PDF, with or without `--batch`: it is
submitted byte-for-byte instead of being rebuilt. Without `--batch`, exactly one
input file is accepted. Explicit `--pdf` files come before `-i` inputs, each list
in its repetition order.

A missing input file, a non-PDF `--pdf` file, a file without a `%PDF-` header
and a PDF that does not parse all fail as `E_SCHEMA` (exit 2) before a job is
reserved. Encrypted PDFs keep `E_PDF_ENCRYPTED`; structurally unsupported ones
keep `E_UNSUPPORTED`.

`raw` never re-encodes the bytes. The file is text (ESC/POS, TSPL, ZPL, CPCL) or
binary; whatever it contains is handed to the queue as-is. If the printer's
driver rejects RAW, the command fails with `E_RAW_UNSUPPORTED` instead of
falling back to graphic printing.

## Global options

| Option | Meaning |
|---|---|
| `--data-dir DIR` | Data directory; beats `PRINTKIT_DATA_DIR` and the platform default |
| `--port PORT` | Loopback port of the local service; beats `PRINTKIT_PORT` and `config.toml` |
| `--log LEVEL` | `off`, `debug`, `info` or `usage-only`; beats `PRINTKIT_LOG` and `config.toml` |
| `-h`, `--help` | Command help; exits 0 |
| `-V`, `--version` | Binary version; exits 0 |

## Configuration

`data_dir` is resolved first, then `<data_dir>/config.toml` is read, and every
key is resolved independently. Precedence: command-line option, environment
variable, `config.toml`, a value stored in the service database, built-in
default. `data_dir` cannot be set in `config.toml`; allowed origins are managed
through `POST`/`DELETE /admin/origins` or `printkit origin`, not configuration.

`printkit config --json` lists every key with its effective value and the source
that won, so it is the authoritative key list for the build in front of you.
`--show-source` annotates the text output, `--path` prints only the file path
and `--validate` exits non-zero on an invalid key. Configuration is read once by
`serve` at startup and never re-read while the service runs; restart to apply a
change.

Environment variables: `PRINTKIT_DATA_DIR`, `PRINTKIT_PORT`, `PRINTKIT_LOG`,
`PRINTKIT_RENDER_WORKERS`, `PRINTKIT_QUEUE_MAX`, `PRINTKIT_QUEUE_TIMEOUT_MS`,
`PRINTKIT_FETCH_TIMEOUT_MS`, `PRINTKIT_FETCH_MAX_BYTES`.

## Data directory and portable mode

Without any override, macOS uses `~/Library/Application Support/printkit/`. The
directory holds:

```text
config.toml       optional overrides
printkit.db       SQLite: usage records, origins
printkit.db.bak-before-v8   backup taken before the template table is dropped; a readable copy is kept, an empty/corrupt one is replaced
printkit.db.bak-before-v9   backup of calibration rows before that table is dropped (readable copy kept; empty/corrupt replaced)
plugins.toml      built-in plugin state (defaults to preset core)
plugins.toml.bak* backups written before a save or the core migration
plugins.lock      service-lifetime lock that owns plugins.toml
logs/             JSONL trace events, rotated by `[retention].days`
exports/          files written by doc.exportPdf
serve.lock        pid and port of the process that owns the directory
admin-token       administrator secret, owner-only (0600)
```

`serve.lock` enforces one service per data directory. A second `serve` on the
same directory exits with "another printkit serve owns ...", naming the pid and
port. Do not point two processes at the same directory, and do not run `serve`
and a data-directory-owning `preview` at the same time.

Portable mode keeps everything beside the binary: create an empty file named
`portable.flag` next to the executable and the data directory becomes
`<install dir>/data/`.

```sh
touch portable.flag
./printkit config --json     # data_dir points at <install dir>/data
```

## `serve`: endpoints and authentication

`printkit serve` binds `127.0.0.1` only (default port `19080`) and serves no
HTML. Every response is JSON, a WebSocket frame or a binary download. The
desktop app is the UI; external integrations talk to these endpoints directly.

Administrator authentication is three steps:

1. `GET /admin/session` (no session required) issues the `pk_csrf` cookie and
   reports whether a session is already authenticated.
2. `POST /admin/login` with `{"token":"<admin-token contents>"}`, the `pk_csrf`
   cookie and its value in `X-CSRF-Token`, plus an `Origin` naming this service,
   sets the `pk_admin` cookie.
3. Every state-changing `/admin` request sends the same `pk_csrf` value back in
   `X-CSRF-Token`, with its `Origin` and the `pk_admin` cookie.

The token file is created on first start and is readable by its owner only (a
user-only DACL on Windows). A runnable sequence from an empty cookie jar:

```sh
base=http://127.0.0.1:19080
jar=$(mktemp)
curl -s -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6=="pk_csrf"{print $7}' "$jar")
curl -s -b "$jar" -c "$jar" -H "Origin: $base" -H "X-CSRF-Token: $csrf" \
  -H 'Content-Type: application/json' \
  -d "{\"token\":\"$(cat "$HOME/Library/Application Support/printkit/admin-token")\"}" \
  "$base/admin/login"
# The pk_admin cookie is now in $jar; every write repeats Origin + CSRF:
curl -s -b "$jar" -H "Origin: $base" -H "X-CSRF-Token: $csrf" \
  -H 'Content-Type: application/json' -d '{"origin":"http://localhost:5173"}' \
  "$base/admin/origins"
```

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/admin/login` | admin token + CSRF + Origin | Start an administrator session |
| GET | `/admin/session` | none; issues `pk_csrf` | Current session state |
| GET, POST, DELETE | `/admin/origins` | admin | List, add or remove allowed browser origins (`DELETE` takes `{"origin"}`) |
| POST | `/admin/logout` | admin session + CSRF + Origin | End the session and clear the `pk_admin` cookie |
| POST | `/admin/tokens` | admin | Issue a token bound to an allowlisted origin; optional `raw` and `admin` scopes |
| DELETE | `/admin/tokens/{tokenId}` | admin session + CSRF + Origin | Revoke a token by id; unknown/already revoked returns `E_NOT_FOUND` |
| POST | `/admin/preview` | admin | Open a preview window for a document |
| GET, POST | `/admin/plugins` | admin | Plugin state and configuration (core page) |
| GET | `/admin/logs/events` | admin | Trace events (`observability` plugin) |
| POST | `/admin/logs/export` | admin | Export a diagnostics bundle (`observability` plugin) |
| POST | `/admin/shutdown` | admin | Stop the service |
| GET | `/records/list` | session | Usage records (`records` plugin) |
| GET | `/records/stats` | session | Aggregated usage (`records` plugin) |
| GET | `/records/summary` | session | Record counts and totals (`records` plugin) |
| GET | `/records/export.csv` | session | Records as CSV (`records` plugin) |
| GET, HEAD | `/exports/{path}?exp&sig` | loopback `Host` + signed link | Read-only file from `<data_dir>/exports/` |
| POST | `/print/batch` | Origin allowlist; token optional | Script/non-browser `multipart/mixed` batch (documents or finished PDFs); browsers use WS `doc.printBatch` for documents and `doc.printPdfBatch` for finished PDFs |
| GET (upgrade) | `/print` | Origin allowlist; token optional | WebSocket print protocol |

Duplicate suppression is best-effort: `requestId` (WebSocket) and
`Idempotency-Key` (`POST /print/batch`) are remembered for at most 600 seconds,
1024 entries and about 1 MiB per service process. A duplicate inside that
budget returns the original job id; after expiry, eviction or a restart the same
key prints again, so a caller that must not repeat a page needs its own
business id.

Printing needs no token: a whitelisted `Origin` and a local script without
an `Origin` both receive the print scope. `raw` and `admin` need a token
issued through `/admin/tokens`; a browser token is bound to its exact
origin. Method list and payloads: [docs/web-integration.md](web-integration.md).

## Plugins

Two optional families are built-in plugins: `records` (`/records/*`, `printkit stats`)
and `observability` (`/admin/logs/*`, `printkit logs`). A CLI or `serve` data
directory uses the `core` preset, so both are off and their routes return 404;
`printkit plugin enable records` turns one on (the desktop app enables both on
its first owned start and never overwrites an existing `plugins.toml`). Core
print, preview, export and administration keep working with every plugin off.
Plugin state lives in
`<data_dir>/plugins.toml` and survives upgrades; disabling keeps the data,
enabling restores the pages.

`/exports/<path>` exists because a browser page cannot navigate to `file://`. It
accepts loopback `Host` headers only, canonicalises the path and refuses anything
outside `<data_dir>/exports/`. `doc.exportPdf` returns a signed
`url` plus `expiresAt` (a 10-minute capability); the link carries a per-run HMAC
over the canonical file path and that expiry. An unsigned, expired or
wrong-file link is `403 E_AUTH`; the signature is the capability, so do not
construct, log or share a bare `/exports/<path>` URL.
`diagnostics.export_ttl_hours` applies to the diagnostics bundle directory
(swept when `POST /admin/logs/export` runs) and to `exports/`: each
`doc.exportPdf` call removes files older than that TTL before writing its own
output. Success is the binary PDF/`inline` response; every 400/403/404/500 on
this route is `application/json; charset=utf-8` with
`{ok:false,error:{code,message}}`.

## Examples

Render only, no printer and no service:

```sh
./printkit pdf -i document.json -o out.pdf --json
./printkit pdf -i document.json -o out.pdf --dump-html /tmp/document.html
```

Print to a named printer, then confirm the queue knows it:

```sh
./printkit printers
./printkit papers -p "Office Printer"
./printkit print -i document.json --printer "Office Printer" -n 2
./printkit print -i part-a.json -i part-b.json --batch --json
./printkit print --pdf existing.pdf --printer "Office Printer"   # raw bytes, no concat
./printkit print -i part-a.pdf -i part-b.pdf --batch -n 2          # merged once, then 2 spooler copies
./printkit print -i existing.pdf --json          # .pdf is detected by name
./printkit origin add https://app.example
./printkit origin list --json
```

Send raw device bytes, preview, and read diagnostics:

```sh
./printkit raw -f sample.tspl -p "Office Printer" --title "bin 42"
printf '\x1b@hello\n\x1dV\x00' | ./printkit raw -f - -p "Receipt"
./printkit preview -i document.json
./printkit doctor --json
./printkit logs --stage render --field html_ms --json
./printkit logs --follow
./printkit stats --group printer --from 2026-11-01 --to 2026-11-08 --json
./printkit config --json
./printkit schema --path
```

`printkit logs` needs the `observability` plugin and `printkit stats` needs
`records`. With the default `core` preset both exit with a message naming the
plugin to enable; start `serve`, then run `printkit plugin enable observability`
or `records` first.

## Exit behaviour

| Exit | Meaning |
|---|---|
| 0 | Success |
| 1 | Usage or configuration error (`E_CONFIG`) |
| 2 | Input or document failed schema validation (`E_SCHEMA`) |
| 3 | Job failure: render, submit, cancel, printer not found, timeout |
| 4 | Host cannot do it: renderer unavailable, unsupported request (`E_UNSUPPORTED`) |

With `--json`, failures write one object to **stderr** and keep stdout for the
command's own output:

```json
{"code":"E_SCHEMA","message":"...","detail":{"pointer":"/pages/0"}}
```

## Troubleshooting

**`schema` fails with "no schema directory beside ..."** — the schema archive was
not extracted next to the binary. Unpack `printkit-schema-*.tar.gz` into the same
directory as `printkit`, or run `./printkit schema --out DIR`.

**`serve` says another process owns the data directory** — read the pid and port
from the message, or from `<data_dir>/serve.lock`. Use `--data-dir` for a second
instance, or stop the other process.

**`logs` says "observability is disabled"** — enable the plugin on a running
service: `printkit plugin enable observability`. Until then the command exits
nonzero instead of returning an empty log.

**`stats` says "records is disabled"** — enable `records` the same way
(`printkit plugin enable records`). Core printing does not need either plugin.

**`E_PRINTER_NOT_FOUND`** — the name must match a queue as reported by
`printkit printers`, including case. Omit `-p` to use the system default.

**`E_UNSUPPORTED` on a duplex request** — duplex is rejected on both current
platforms until a physical duplex device is verified, never silently printed
single-sided. Print single-sided, or duplex in the printer driver.

**0-page PDF / macOS discovery** — a 0-page PDF (single or batch) returns `E_SCHEMA`; macOS `printers` only lists submit-capable cupsd queues, not PrintCore-only names.

**`resolver busy` / repeated `E_TIMEOUT` on `data.url`** — DNS lookups run on
four fixed workers. If the system resolver never returns, those workers cannot
recover; restart the service (or the desktop app) to get a fresh pool.

**`E_CANCEL_UNSUPPORTED`** — the job never had a spooler id, so there is nothing
to retract. On Windows, `doc.printPdf` and `doc.printBatch` go through the
WebView2 PDF viewer and are not cancellable.
