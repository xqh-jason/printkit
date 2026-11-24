# AGENTS.md

Process details live in `CONTRIBUTING.md`; this file is the short version a coding agent needs.

## Delivery
- Every change ends with evidence: the machine, the files or suites covered, the pass count,
  which failures already existed on the base commit, and what was **not** run.
  "Done" without that is not done; if nothing was verified, say so.
- Rust: `cargo test --workspace --all-features`. Desktop: `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml`.
- Leave nothing behind: no background processes, scratch scripts, debug output or commented-out code.

## Scope
- One change, one subsystem. If a fix needs another subsystem, say so and stop instead of expanding scope.

## Design pages
- `docs/` holds one page per externally visible subsystem (`cli`, `web-integration`, `release-checklist`).
  A behaviour change updates its page in the same commit, and pages link to source paths, not to line numbers.

## Repository
- Rust workspace: `crates/*`, `plugins/*`, `apps/*` (`apps/desktop` is excluded and built with pnpm).
- `origin` is the only public repository: `https://github.com/xqh-jason/printkit.git`.
  There is no private mirror any more; do not push private history.
- English only, except `README.zh-CN.md`, `docs/cli.zh-CN.md`, `docs/web-integration.zh-CN.md`.
