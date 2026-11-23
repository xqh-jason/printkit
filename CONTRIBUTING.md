# Contributing

Thank you for improving printkit. This repository is English-only except
`README.zh-CN.md`, `docs/web-integration.zh-CN.md` and `docs/cli.zh-CN.md`:
code, comments, error messages, commit messages, issues and documentation are
written in English.

## Before you start

Open an issue for anything larger than a bug fix, so the scope and the contract
can be agreed before code is written. Do not include company names, internal
hostnames, credentials or real customer data in issues, fixtures or commits.

## Prerequisites

- Rust 1.89 or newer (the MSRV in `Cargo.toml`).
- A nightly toolchain for `cargo +nightly fmt`; `rustfmt.toml` uses
  nightly-only options.
- `poppler` (`pdftotext`, `pdfinfo`, `pdffonts`) and `jq` for acceptance checks.
- `cargo-deny`, `cargo-hack` and `cargo-cyclonedx`.
- Node.js **20.19+ or 22.12+** (Vite 7), pnpm 11 for the desktop app and the
  shared i18n/SDK packages.

```sh
rustup toolchain install stable nightly
cargo install cargo-deny cargo-hack cargo-cyclonedx --locked
brew install poppler jq
corepack enable
```

## Build and test

```sh
cargo build --release -p printkit
./target/release/printkit pdf -i tests/fixtures/minimal.json -o /tmp/out.pdf
cargo test --workspace --all-features
```

For the local service and desktop app inner loop, use the one-command wrapper:
`./scripts/dev.sh help` (`setup`, `serve`, `desktop`, `stop`).

The local gate script runs checks 0-17: formatting (nightly), raster and
loopback bans, CJK/hygiene, `cargo deny`, generated schema/font/i18n artefacts,
dependency graph and bin/license/lint/dependency-coordinate rules, a
printer-free PDF and trace chain, the feature matrix, geometry and table
pagination, the pairing/WebSocket-limit/URL-fetch service e2e, a rustdoc pass
(`-D warnings -D missing_docs`, so an undocumented public item or a broken
intra-doc link fails the gate), and the plugin behaviour e2e (core-only,
concurrency, JSONL retention, fail-open, plus S2 absence regressions).

```sh
bash scripts/gates.sh
```

A skipped gate never counts as a pass. Install the missing tool instead of
ignoring the skip.

CI additionally runs workspace tests/doc tests/clippy/feature matrix/release
feature/deny, pairing/plugins/render-pool/trace/ws-handshake/url-guard e2e,
the desktop build/tests/cargo test/smoke/control/license gates, and
the SDK tests (test, browser bundle, consumer types). Preview windows are a
separate job gated by the repository variable `PRINTKIT_GUI_E2E=true` on a
GUI-capable runner: unset means the job is skipped (gray), never a pass; when
enabled, a missing GUI session or required tool fails the job instead of
skipping.

## Generated files

These files are generated from source and must not be edited by hand:

- `schema/` comes from the Rust types in `crates/core` and `crates/protocol`.
  Regenerate with `cargo run -p printkit -- schema --out schema`.
- `packages/sdk-ts/src/types.ts`, `src/methods.ts` and `src/limits.ts` are
  generated. Types come from the protocol crate and the schema's `required`
  projection, methods from the protocol method table, limits from
  `crates/core/src/limits.rs`. Regenerate with
  `pnpm --dir packages/sdk-ts gen` (or `gen:types` / `gen:methods`); the
  protocol test, `verify.mjs` and `test:types` catch drift.

`scripts/gates.sh` fails when the copy drifts from its source. The desktop app
builds its own bundle.

## UI language

`packages/i18n/catalog/en.json` and `zh-CN.json` are the single source of
user-facing strings for the desktop app. Both files must carry the same keys and
placeholders; `node packages/i18n/test/check.mjs` (gate 5d) enforces that and
checks the keys referenced under `apps/desktop/src`. Add a key to both
catalogues, then rebuild the desktop app.

## Desktop app

`apps/desktop` is an independent Cargo workspace and the only UI. It links no
`printkit-*` crate and talks to the service only over `http://127.0.0.1`.

Start the real development loop with `./scripts/dev.sh desktop`. It stops a
stale Tauri dev CLI, a stale `target/debug/printkit-desktop`, a repository Vite
server on port 1420, and an installed `printkit desktop.app` with bundle id
`dev.printkit.desktop`. A non-repository process on 1420 aborts the script with
its pid/command instead of being killed. It then builds the debug sidecar and
runs `pnpm --dir apps/desktop tauri dev`.

The app is one resizable business window (default 900×633, minimum 560×420)
with Overview, Printers, Plugins and Settings always present, plus Records and
Logs/Jobs when their plugins are enabled. A desktop-owned first start seeds
`plugins.toml` with the `observability` and `records` plugins enabled, so both
pages appear; an existing file is never overwritten, and a CLI/`serve` data
directory keeps its core-only default. Toggle a plugin on the Plugins page or
with `printkit plugin enable|disable <id>`. There is no developer window or
`--dev` flag. The window hides to the tray. Single-instance applies:
do not run the installed app and the dev build together, and do not run `serve`
and `desktop` against the same data directory/port. The first run compiles the
sidecar and is slow.

Manual commands and checks:

```sh
pnpm --dir apps/desktop install --frozen-lockfile
bash apps/desktop/scripts/prepare-sidecar.sh debug
pnpm --dir apps/desktop tauri dev
pnpm --dir apps/desktop build
pnpm --dir apps/desktop test:desktop
cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml
cargo test  --manifest-path apps/desktop/src-tauri/Cargo.toml
bash tests/scripts/desktop_smoke.sh
bash tests/scripts/desktop_control_live.sh
```

For a framework-agnostic browser page guide (raw WebSocket and vendored SDK),
see [docs/web-integration.md](docs/web-integration.md).

## Versioning

The project is pre-1.0. A minor release may change CLI or service behaviour;
a patch release fixes bugs. `schema/document-v1.json` and `protocol-v1.json`
are versioned independently; after the first public release, version 1 of each
stays compatible within its major version. The MSRV is Rust 1.89 and is raised
only in a minor release. Deprecations are announced in
[CHANGELOG.md](CHANGELOG.md) at least one minor release ahead. `0.2.5` is still
a pre-1.0 release, so methods removed before the first public release (for
example `calib.set`/`calib.list`) are a pre-release breaking change and carry
no compatibility shim.

Every release tag must correspond to a changelog entry. Update `CHANGELOG.md`
in the same pull request that changes user-visible behaviour, and keep the
entry under `Unreleased` until the tag is cut. GitHub release notes are
generated from the tag; they supplement, not replace, the changelog.

## Dependencies and licenses

The project is distributed under `MIT OR Apache-2.0`. GPL, AGPL and LGPL
dependencies are not accepted in the CLI. The desktop shell has one approved
exception: `cssparser`, `cssparser-macros`, `dtoa-short`, `option-ext`,
`selectors` (Tauri's codegen/utility chain; see requirements section 1.2 rev
29). Add every new license identifier to `deny.toml`, run `cargo deny check`,
and regenerate the legal files when a dependency or bundled font changes:

```sh
# cargo-about, pnpm and the installed dependency trees are required
bash scripts/gen-legal-notices.sh
python3 scripts/gen-notices.py --check --sbom dist/sbom.cdx.json
```

`THIRD-PARTY-NOTICES.md` is the inventory plus a per-component copyright
summary. `assets/licenses/component-licenses.txt` reproduces the LICENSE and
NOTICE files of each exact upstream package; the committed
`assets/licenses/component-licenses.json` lets `--check` run without
cargo-about or pnpm. Canonical SPDX texts live in `assets/licenses/*.txt`, with
their URLs and SHA-256 values in `assets/licenses/SOURCES.md`.

This is a best-effort disclosure, not a claim of full compliance. Copyright
remains with the upstream authors. A component whose packaged metadata has no
copyright line is marked `[packaged metadata has no copyright line; see
upstream source]`; that marker describes the package we received and is not an
invented holder. Apache-2.0 license files also contain the appendix template
`Copyright {yyyy} {name of copyright owner}` (or its square-bracket variant);
that is license boilerplate, not a component attribution.

The desktop shell's Tauri 2.12.5 tree includes MPL-2.0 crates and
`option-ext` from `dirs`; `apps/desktop/deny.toml` lists those five as
exceptions so the desktop gate runs. They are reached only from the desktop
workspace, not the CLI, and each exact version's source is available from
crates.io; `THIRD-PARTY-NOTICES.md` records that source-availability path.

## Release packaging

`scripts/release.sh` is the only release producer:

```sh
bash scripts/release.sh --build-only     # this host's artifacts and SHA256 shards
bash scripts/release.sh --assemble       # release bundle, checksums and SBOM
bash scripts/release.sh --sbom-only      # SBOM only (CI notices freshness)
```

The tag workflow builds the CLI and the desktop console in separate jobs, then
assembles and verifies the bundle. Real artifacts are produced only there. See
[docs/release-checklist.md](docs/release-checklist.md) for the GitHub settings
and tag prerequisites that are not enforceable from a workflow.

Every artifact written by `--build-only` gets a sibling `.build-info` shard
recording the source commit, dirty marker, platform, `rustc` version, build time,
`SOURCE_DATE_EPOCH` and `reproducible=not-byte-reproducible`. `--assemble`
requires a clean worktree and rejects a shard whose build-info is missing, names
another commit or records a dirty build. It merges the Windows SBOM shards into
the release inventory, runs `printkit schema --out` from the packaged binary and
diffs the result against the bundle, so a stale binary cannot ship next to newer
contracts. `dist/BUILD-INFO` records the assembled commit and artifact list, and
SHA256SUMS covers it. `--allow-dirty` exists only to exercise `--build-only`
locally; a dirty shard can never be assembled.

`bash scripts/release.sh` with no mode builds this host's artifacts and then
assembles the bundle; the two phases remain available as `--build-only` and
`--assemble`. `--sbom-only` generates the same full SBOM without building
artifacts, for the PR notice-freshness job.

Builds are not byte-for-byte reproducible. `SOURCE_DATE_EPOCH` is the commit
timestamp and is exported for tools that honor it, but Rust debuginfo, WebView
and Tauri bundling still embed environment-dependent data. `BUILD-INFO` states
this explicitly; do not compare two builds by hash.

Desktop bundles are ad-hoc signed through
`bundle.macOS.signingIdentity: "-"` in `tauri.conf.json`. The signature is
structurally complete (`codesign --verify --deep --strict` and
`Contents/_CodeSignature`), but it has no Developer ID and is not notarized;
`spctl` therefore rejects it. Release verification accepts that specific
ad-hoc rejection and fails on invalid or unexpected signatures. Quarantine
double-click behavior has not been verified.

`tests/scripts/s16_accept.sh` checks the local S16 release evidence without a
printer: archives, schema consistency, `SHA256SUMS`, `BUILD-INFO`, DMG
app/sidecar signatures, portable mode and SBOM/licenses. Use `--build-only` to
inspect `dist/shards` before assembling:

```sh
bash tests/scripts/s16_accept.sh
bash tests/scripts/s16_accept.sh --build-only
```

## Commits and pull requests

- Keep each commit focused on one capability; do not mix refactors with
  behaviour changes.
- Write English commit messages without step or milestone identifiers.
- Run the gates before opening a pull request and paste the commands you ran
  into the description.
- Explain what changed, why, and how it was verified. Link the issue when one
  exists.
- Tests must cover observable behaviour: boundaries, invariants, error paths.
  Do not test copied strings, wiring or mock echoes.
