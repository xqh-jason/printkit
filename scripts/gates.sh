#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Local gates 0-17. Gates whose inputs do not exist yet print `skip`; skip never
# counts as a pass. Never use `! grep`: `set -e` exempts commands whose status is
# inverted, which turns a gate into a no-op. Collect output first, then test it.
SKIPS=()
skip() {
  echo "$1"
  SKIPS+=("$1")
}

# 0. Formatting: rustfmt.toml uses nightly-only options, so the check must run
#    on nightly; without it the gate is skipped rather than silently passing.
if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
  cargo +nightly fmt --all -- --check
else
  skip "gate 0: skip (nightly toolchain not installed)"
fi

# 1. Raster path: whitelist style. Only crates/platform-win/src/gdi.rs may call
#    a rasteriser, and only on a line carrying `// ALLOW-RASTER`.
hits="$(grep -rn -E 'rasterize|to_bitmap|StretchDIBits|FPDF_|pdfium' crates/ apps/ --include='*.rs' || true)"
bad="$(printf '%s\n' "$hits" | grep -Ev 'crates/platform-win/src/gdi\.rs' | grep -v '^$' || true)"
if [ -n "$bad" ]; then
  printf '%s\n' "$bad"
  echo "gate 1: raster call outside gdi.rs" >&2
  exit 1
fi
bad="$(printf '%s\n' "$hits" | grep -E 'crates/platform-win/src/gdi\.rs' | grep -v 'ALLOW-RASTER' | grep -v '^$' || true)"
if [ -n "$bad" ]; then
  printf '%s\n' "$bad"
  echo "gate 1: gdi.rs raster line without // ALLOW-RASTER" >&2
  exit 1
fi

# 2. Never bind anything but loopback. Code only: docs may quote 0.0.0.0.
hits="$(grep -rn '0\.0\.0\.0' apps/ crates/ --include='*.rs' || true)"
if [ -n "$hits" ]; then
  printf '%s\n' "$hits"
  echo "gate 2: non-loopback bind" >&2
  exit 1
fi

# 3a. Repository language: only README.zh-CN.md, docs/cli.zh-CN.md,
#     docs/web-integration.zh-CN.md and the i18n package may contain CJK.
cjk="$(python3 scripts/cjk-scan.py || true)"
if [ -n "$cjk" ]; then printf '%s\n' "$cjk"; echo "gate 3a: CJK text outside the allowed language files" >&2; exit 1; fi

# 3. Open-source hygiene: private ranges, credentials and database URLs.
#    -I skips binaries; the exclusions keep the gate honest:
#    gates.sh itself (basename, --exclude only matches basenames), _work/ design
#    docs, fictional fixtures/baselines, pnpm's local module cache, and
#    .mimosa/ local tool state.
hits="$(grep -rEi -I \
  --exclude=gates.sh --exclude-dir=.git --exclude-dir=target \
  --exclude-dir=_work \
  --exclude-dir=node_modules --exclude-dir=.mimosa \
  -e '10\.[0-9]+\.[0-9]+\.[0-9]+|172\.(1[6-9]|2[0-9]|3[01])\.|192\.168\.' \
  -e 'BEGIN (RSA|OPENSSH|PRIVATE)|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|sk-[A-Za-z0-9]{20,}' \
  -e 'postgres(ql)?://|mysql://|mongodb\+srv://' . || true)"
if [ -n "$hits" ]; then
  printf '%s\n' "$hits"
  echo "gate 3: hygiene violation" >&2
  exit 1
fi

# 4. Licenses and advisories. The advisory database is refreshed when the
#    network allows; when the fetch fails the cached copy is used, and a real
#    finding still fails the gate.
deny_out="$(cargo deny check licenses advisories bans sources 2>&1)" || {
  if grep -qiE 'failed to (fetch|update|download)|network|timed out|could not connect|unable to update' <<< "$deny_out"; then
    echo "gate 4: advisory database refresh failed, retrying against the cached copy" >&2
    cargo deny --offline check licenses advisories bans sources
  else
    printf '%s\n' "$deny_out" >&2
    echo "gate 4: cargo deny failed" >&2
    exit 1
  fi
}

# 5. Generated artefacts must not drift.
if [ -d schema/ ]; then
  cargo run -p printkit -- schema --out /tmp/schema-check > /dev/null
  diff -r /tmp/schema-check schema/ || {
    echo "schema out of date: run printkit schema --out schema" >&2
    exit 1
  }
else
  skip "gate 5a: skip (schema/ not generated)"
fi

# 5c. The CID map shipped with the Source Han subset must cover the locl digits.
cids=assets/fonts/SourceHanSansSC-Regular-subset.cids.txt
[ -f "$cids" ] || { echo "gate 5c: missing $cids" >&2; exit 1; }
[ "$(wc -l < "$cids")" -ge 10000 ] || { echo "gate 5c: $cids looks truncated" >&2; exit 1; }
for cid in f6be f6bf f6c0 f6c1 f6c2 f6c3 f6c4 f6c5 f6c6 f6c7; do
  grep -q "$cid" "$cids" || { echo "gate 5c: $cids is missing $cid" >&2; exit 1; }
done

# 5d. The shared catalogue and every page/script marker must agree.
if [ -f packages/i18n/test/check.mjs ]; then
  node packages/i18n/test/check.mjs
else
  skip "gate 5d: skip (packages/i18n not created)"
fi

# 6. Dependency graph. Collect output first: `cargo tree | grep -q` lets sed take
#    SIGPIPE and pipefail then hides the hit.
tree_has() {
  local pkg="$1" dep="$2" meta deps
  meta="$(cargo metadata --no-deps --format-version 1)" \
    || { echo "gate 6: cargo metadata failed" >&2; exit 1; }
  if ! grep -q "\"name\":\"$pkg\"" <<< "$meta"; then
    if [ -d "crates/$pkg" ]; then
      echo "gate 6: crates/$pkg exists but is not a workspace member" >&2
      exit 1
    fi
    skip "gate 6: skip $pkg (not created yet)"
    return 1
  fi
  deps="$(cargo tree -p "$pkg" --target all --edges normal --prefix none | sed 1d)" \
    || { echo "gate 6: cargo tree failed for $pkg" >&2; exit 1; }
  grep -q "^$dep" <<< "$deps"
}
tree_has printkit-render printkit-platform && { echo "gate 6: render depends on platform" >&2; exit 1; }
tree_has printkit-pdf printkit-engine && { echo "gate 6: pdf depends on engine" >&2; exit 1; }
tree_has printkit-render printkit-pdf && { echo "gate 6: render depends on pdf" >&2; exit 1; }
tree_has printkit-pdf printkit-render && { echo "gate 6: pdf depends on render" >&2; exit 1; }
tree_has printkit-platform printkit-engine && { echo "gate 6: platform depends on engine" >&2; exit 1; }
tree_has printkit-core printkit- && { echo "gate 6: core depends on an internal crate" >&2; exit 1; }
tree_has printkit-trace printkit- && { echo "gate 6: trace is not a leaf" >&2; exit 1; }
tree_has printkit-store printkit-trace && { echo "gate 6: store depends on trace" >&2; exit 1; }
tree_has printkit-core printkit-schema && { echo "gate 6: core depends on schema" >&2; exit 1; }
tree_has printkit-render printkit-schema && { echo "gate 6: render depends on schema" >&2; exit 1; }
tree_has printkit-engine printkit-schema && { echo "gate 6: engine depends on schema" >&2; exit 1; }
tree_has printkit-protocol printkit-schema && { echo "gate 6: protocol depends on schema" >&2; exit 1; }
tree_has printkit-platform-win printkit-platform-mac && { echo "gate 6: platform crates depend on each other" >&2; exit 1; }
tree_has printkit-platform-mac printkit-platform-win && { echo "gate 6: platform crates depend on each other" >&2; exit 1; }
if grep -rn 'sdk-ts' --include='Cargo.toml' . > /dev/null 2>&1; then
  echo "gate 6: a Cargo.toml references sdk-ts" >&2
  exit 1
fi
bad="$(cargo metadata --no-deps --format-version 1 | jq -r -f scripts/apps-dep.jq)"
if [ -n "$bad" ]; then
  echo "gate 6: internal crates depend on apps/*: $bad" >&2
  exit 1
fi

# 6c. plugins/* depend on printkit-plugin-api only; never core/host/another
#     plugin. plugin-host may depend on the builtins (it composes them).
bad="$(cargo metadata --no-deps --format-version 1 | jq -r '
  [.packages[]
   | select(.manifest_path | contains("/plugins/"))
   | .dependencies[]
   | select(.kind == null)
   | .name
   | select(. != "printkit-plugin-api")] | unique | join(" ")' )"
if [ -n "$bad" ]; then
  echo "gate 6c: plugin crate depends on something other than printkit-plugin-api: $bad" >&2
  exit 1
fi

# 7. One binary: count real bin targets, not [[bin]] sections.
bins="$(cargo metadata --no-deps --format-version 1 | jq '[.packages[].targets[] | select(.kind | index("bin"))] | length')"
[ "$bins" -le 1 ] || { echo "gate 7: $bins bin targets (only one allowed)" >&2; exit 1; }

# 8. License files match the manifest.
[ -f LICENSE-MIT ] && [ -f LICENSE-APACHE ] || {
  echo "gate 8: missing LICENSE-MIT / LICENSE-APACHE" >&2
  exit 1
}
grep -q 'license = "MIT OR Apache-2.0"' Cargo.toml || {
  echo "gate 8: Cargo.toml license field mismatch" >&2
  exit 1
}

# 9. PDF without a printer. Also builds the binary used by gates 11 and 12.
#    A throwaway data directory keeps local logs and config out of the run.
GATE_DATA_DIR="$(mktemp -d)"
trap 'rm -rf "$GATE_DATA_DIR"' EXIT
PRINTKIT="./target/release/printkit --data-dir $GATE_DATA_DIR"
cargo build --release -p printkit
$PRINTKIT pdf -i tests/fixtures/minimal.json -o /tmp/minimal.pdf
text="$(pdftotext /tmp/minimal.pdf -)"
grep -q "printkit" <<< "$text" || {
  echo "gate 9: pdftotext output does not contain the document text" >&2
  exit 1
}

# A pure ASCII run takes alternate digit glyphs from the font's `locl`
# substitutes. Extract them as digits, not as unmapped CIDs.
$PRINTKIT pdf -i tests/fixtures/numeric-text.json -o /tmp/numeric.pdf
numeric="$(pdftotext /tmp/numeric.pdf -)"
for token in 0123456789 12345.67 890.00; do
  grep -qF -- "$token" <<< "$numeric" || {
    echo "gate 9: pdftotext output does not contain $token" >&2
    exit 1
  }
done

# 9b. Mixed paper keeps each page's own media box.
$PRINTKIT pdf -i tests/fixtures/mixed-paper.json -o /tmp/mixed.pdf
pdfinfo -f 1 -l 2 /tmp/mixed.pdf > /tmp/mixed.info
grep -q "Page    1 size:  595.276 x 841.89 pts" /tmp/mixed.info || {
  echo "gate 9b: first page is not A4" >&2
  exit 1
}
grep -q "Page    2 size:  419.528 x 595.276 pts" /tmp/mixed.info || {
  echo "gate 9b: second page is not A5" >&2
  exit 1
}
mixed="$(pdftotext /tmp/mixed.pdf -)"
grep -qF "page two A5" <<< "$mixed" || {
  echo "gate 9b: second page text missing" >&2
  exit 1
}

# 9c. The result's `pages` is the rendered PDF page count, not JSON pages.
$PRINTKIT pdf -i tests/fixtures/table-multipage.json -o /tmp/table.pdf --json > /tmp/table.json
reported="$(jq -r .pages /tmp/table.json)"
actual="$(pdfinfo /tmp/table.pdf | awk '/^Pages:/ {print $2}')"
[ "$reported" = "$actual" ] || {
  echo "gate 9c: result pages $reported != PDF pages $actual" >&2
  exit 1
}

# 9d. The published protocol schema accepts the real hello frame.
jq -e '
  ."$defs".Hello.properties.type.const == "hello"
  and (."$defs".Hello.required | sort) == ["protocol", "sdk", "type"]
' schema/protocol-v1.json >/dev/null || {
  echo "gate 9d: Hello schema does not require the wire discriminator" >&2
  exit 1
}

# 10. Feature matrix. `test-support` is a test double, not a capability matrix;
#     the matrix runs as soon as any package declares a `full` feature, so a
#     crate set without one cannot silently stand in for a matrix that exists.
full_packages="$(cargo metadata --no-deps --format-version 1 \
  | jq -r '[.packages[] | select(.features | has("full")) | .name] | join(" ")')"
if [ -n "$full_packages" ]; then
  echo "gate 10: feature matrix for $full_packages"
  cargo hack check --workspace --feature-powerset --depth 1
else
  skip "gate 10: skip (no package declares a full feature)"
fi

# 11. One pdf run must leave a queryable event chain: recv/validate/render/
#     result, with both render timings on the render event.
gate11_data="$(mktemp -d)"
printf 'version = 1\nsettled = 2\npreset = "core"\n[[plugins]]\nid = "observability"\nenabled = true\n' >"$gate11_data/plugins.toml"
TRACE="$($PRINTKIT pdf --data-dir "$gate11_data" -i tests/fixtures/minimal.json -o /tmp/m2.pdf --json | jq -r .traceId)"
events="$($PRINTKIT logs --data-dir "$gate11_data" --trace "$TRACE" --json)"
rm -rf "$gate11_data"
jq -e 'type == "array" and length >= 4' > /dev/null <<< "$events"
jq -e '.[0:4] | map(.stage) == ["recv","validate","render","result"]' > /dev/null <<< "$events"
jq -e 'map(select(.stage == "render"))[0].fields | (.html_ms | type) == "number" and (.pdf_ms | type) == "number"' > /dev/null <<< "$events"

# 12. Lint wiring: `[workspace.lints]` is not inherited automatically.
bad=""
for f in crates/*/Cargo.toml apps/*/Cargo.toml; do
  if grep -q '^\[lints\]' "$f" && grep -q '^workspace = true' "$f"; then
    :
  else
    bad="$bad $f"
  fi
done
if [ -n "$bad" ]; then
  echo "gate 12: crate(s) without [lints] workspace = true:$bad" >&2
  exit 1
fi

# 13. Dependency coordinates live in the root `[workspace.dependencies]` only:
#     a child manifest that writes a version inside a dependency table reopens
#     the second source of truth. `name.workspace = true` lines carry no version
#     and therefore pass.
bad=""
for f in crates/*/Cargo.toml apps/*/Cargo.toml; do
  hits="$(awk '
    /^\[/ { in_deps = ($0 ~ /dependencies\]$/) }
    in_deps && /^[A-Za-z0-9_-]+[[:space:]]*=[[:space:]]*"/ { print FILENAME ": " $0 }
    in_deps && /^[A-Za-z0-9_-]+[[:space:]]*=[[:space:]]*\{/ && /version/ { print FILENAME ": " $0 }
  ' "$f")"
  if [ -n "$hits" ]; then
    bad="$bad
$hits"
  fi
done
if [ -n "$bad" ]; then
  printf '%s\n' "$bad" >&2
  echo "gate 13: dependency version written in a child manifest" >&2
  exit 1
fi

# 14. Geometry, table pagination and render fidelity. Missing poppler is a
#     failure, not a skip: the geometry gate already depends on it, and a
#     silently skipped check is how table-small drifted past 0.300 mm unnoticed.
for tool in pdftotext pdftoppm pdfinfo; do
  command -v "$tool" > /dev/null || {
    echo "gate 14: $tool is required for the geometry gate" >&2
    exit 1
  }
done
bash scripts/compare-geometry.sh
bash tests/scripts/table_fit.sh
# Text align/clip, page background and auto table columns are pixel assertions;
# geometry alone would not catch a regression that shifts or hides content.
bash tests/scripts/render_fidelity.sh

# 15. Service e2e: the same scripts CI and release run. Missing curl/jq/python3
#     is a failure, never a skip; these catch origin, WS handshake, URL-fetch,
#     single-instance and idle-policy regressions before a push.
for tool in curl jq python3; do
  command -v "$tool" > /dev/null || {
    echo "gate 15: $tool is required for the service e2e" >&2
    exit 1
  }
done
bash tests/scripts/pairing_e2e.sh
bash tests/scripts/ws_handshake_limits.sh
bash tests/scripts/url_fetch_guard.sh
# Ten rounds keeps the local gate quick; CI/release can raise the count and the
# full 50-round run is still an explicit acceptance command.
LOCK_RACE_ROUNDS=${LOCK_RACE_ROUNDS:-10} bash tests/scripts/lock_race.sh
bash tests/scripts/ws_idle_limits.sh

# 16. Rustdoc: public items are documented and intra-doc links resolve. The
#     lint runs over all features, matching the API surface the workspace
#     promises; `-D warnings` turns broken links into failures.
RUSTDOCFLAGS='-D warnings -D missing_docs' \
  cargo doc --workspace --all-features --no-deps

# 17. Plugin P0 service behavior: admin API, gates, lease TTL, core-only
#     printing, concurrency and JSONL retention. These build the fake-render
#     debug binary themselves; a missing tool fails gate 15 first.
bash tests/scripts/plugins_e2e.sh
bash tests/scripts/plugins_core_only_e2e.sh
bash tests/scripts/plugins_concurrency.sh
bash tests/scripts/plugins_jsonl_retention.sh
bash tests/scripts/plugins_fail_open_e2e.sh
bash tests/scripts/template_e2e.sh
bash tests/scripts/s2_absence_e2e.sh

allowed='^(gate 0: skip \(nightly toolchain not installed\)|gate 5a: skip \(schema/ not generated\)|gate 5d: skip \(packages/i18n not created\)|gate 6: skip printkit-(store|protocol|schema) \(not created yet\)|gate 10: skip \(no package declares a full feature\))$'
for s in ${SKIPS[@]+"${SKIPS[@]}"}; do
  [[ "$s" =~ $allowed ]] || { echo "gates: unexpected skip: $s" >&2; exit 1; }
done
echo "gates: ok (${#SKIPS[@]} allowed skips; skip never counts as a pass)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
