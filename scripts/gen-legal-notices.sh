#!/usr/bin/env bash
# Rebuild the authoritative legal inputs and both committed Markdown files.
#
# Rust: Mozilla cargo-about (JSON is authoritative; the template render is a
#       review artifact under target/legal/).
# Node: pnpm licenses list --long --json plus the LICENSE files installed from
#       the npm package tarballs.
#
# The generated assets/licenses/component-licenses.json is committed so that
# `gen-notices.py --check` works in CI and release without these tools.
set -euo pipefail
cd "$(dirname "$0")/.."

sbom="${1:-dist/sbom.cdx.json}"
[ -f "$sbom" ] || {
    echo "missing SBOM: $sbom (pass the assembled CycloneDX JSON)" >&2
    exit 1
}

command -v cargo >/dev/null 2>&1 || {
    echo "cargo is required" >&2
    exit 1
}
cargo about --version >/dev/null 2>&1 || {
    echo "Mozilla cargo-about is required: cargo install cargo-about --locked --features cli" >&2
    exit 1
}
command -v pnpm >/dev/null 2>&1 || {
    echo "pnpm is required" >&2
    exit 1
}
[ -d apps/desktop/node_modules ] || {
    echo "apps/desktop/node_modules is missing: run pnpm --dir apps/desktop install --frozen-lockfile" >&2
    exit 1
}

mkdir -p target/legal
work=target/legal

cargo about generate \
    --manifest-path apps/printkit/Cargo.toml \
    --workspace --all-features --locked --format json \
    >"$work/about-cli.json"
cargo about generate \
    --manifest-path apps/desktop/src-tauri/Cargo.toml \
    --locked --format json \
    >"$work/about-desktop.json"
pnpm --dir apps/desktop licenses list --long --json >"$work/pnpm-licenses.json"

# Human-readable renderings for reviewers; not committed.
cargo about generate about.hbs \
    --manifest-path apps/printkit/Cargo.toml \
    --workspace --all-features --locked \
    -o "$work/cargo-about-cli.md"
cargo about generate about.hbs \
    --manifest-path apps/desktop/src-tauri/Cargo.toml \
    --locked \
    -o "$work/cargo-about-desktop.md"

python3 scripts/gen-notices.py --rebuild-legal-data \
    --sbom "$sbom" \
    --licenses-dir assets/licenses \
    --licenses-output assets/licenses/component-licenses.txt \
    --about-cli "$work/about-cli.json" \
    --about-desktop "$work/about-desktop.json" \
    --pnpm-json "$work/pnpm-licenses.json" \
    --about-version "$(cargo about --version | awk '{print $2}')"

python3 scripts/gen-notices.py --check \
    --sbom "$sbom" \
    --licenses-dir assets/licenses \
    --licenses-output assets/licenses/component-licenses.txt
