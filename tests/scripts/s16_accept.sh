#!/usr/bin/env bash
# S16 acceptance 3/4/6/9 against a release bundle or --build-only shards.
#
# The checks are deliberately release-artifact oriented: archives, schema,
# checksums, BUILD-INFO, portable mode, the mounted .app and its signatures.
# They never require a physical printer. Set --build-only to inspect
# dist/shards before the assemble phase.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

DIST="dist"
BUILD_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dist) [ $# -ge 2 ] || { echo "FAIL: --dist needs a directory" >&2; exit 2; }
      DIST="$2"; shift 2 ;;
    --build-only) BUILD_ONLY=1; shift ;;
    -h|--help)
      cat <<'USAGE'
usage: tests/scripts/s16_accept.sh [--dist DIR] [--build-only]

default      verify an assembled dist/ bundle
--build-only verify dist/shards artifacts before assemble
USAGE
      exit 0 ;;
    *) echo "FAIL: unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $DIST == /* ]] || DIST="$root/$DIST"
ART="$DIST"
if [ "$BUILD_ONLY" -eq 1 ]; then
  ART="$DIST/shards"
fi

TMP="$(mktemp -d)"
MOUNT=""
cleanup() {
  if [ -n "$MOUNT" ]; then
    hdiutil detach "$MOUNT" >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}
ok() { printf 'ok: %s\n' "$*"; }

sha_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}
sha_check() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$@"
  else
    shasum -a 256 -c "$@"
  fi
}

triple=$(rustc -vV | sed -n 's/^host: //p')
case "$triple" in
  aarch64-apple-darwin) platform=macos-arm64 ;;
  x86_64-apple-darwin) platform=macos-x64 ;;
  *) fail "S16 is macOS-only; host is $triple" ;;
esac

CLI="$ART/printkit-$platform.tar.gz"
DMG="$ART/printkit-desktop-$platform.dmg"
SCHEMA_ARCHIVE="$DIST/printkit-schema-$platform.tar.gz"

verify_info() {
  local info="$1" artifact="${2:-}"
  [ -f "$info" ] || fail "missing build metadata: $info"
  local commit dirty recorded
  commit=$(sed -n 's/^commit=//p' "$info")
  dirty=$(sed -n 's/^dirty=//p' "$info")
  recorded=$(sed -n 's/^artifact=//p' "$info")
  [ "$commit" = "$(git rev-parse HEAD)" ] \
    || fail "$(basename "$info") was built from ${commit:-unknown}, not HEAD $(git rev-parse HEAD)"
  [ "$dirty" = "0" ] || fail "$(basename "$info") records a dirty build"
  if [ -n "$artifact" ]; then
    [ "$recorded" = "$(basename "$artifact")" ] \
      || fail "$(basename "$info") names '$recorded', not $(basename "$artifact")"
  fi
}

check_workspace_isolation() {
  cargo metadata --format-version 1 --no-deps \
    | jq -e '[.workspace_members[] | select(test("apps/desktop"))] | length == 0' >/dev/null \
    || fail "apps/desktop is part of the root Cargo workspace"
  cargo metadata --manifest-path apps/desktop/src-tauri/Cargo.toml --format-version 1 --no-deps \
    | jq -e '[.packages[] | select(.name == "printkit-desktop") | .dependencies[] | select(.name | startswith("printkit-"))] | length == 0' >/dev/null \
    || fail "printkit-desktop depends on a printkit crate"
  ok "acceptance 3: desktop workspace remains isolated"
}

check_sums_and_info() {
  [ -f "$CLI" ] || fail "missing $(basename "$CLI")"
  [ -f "$DMG" ] || fail "missing $(basename "$DMG")"

  if [ "$BUILD_ONLY" -eq 0 ]; then
    [ -f "$DIST/SHA256SUMS" ] || fail "missing SHA256SUMS"
    [ -f "$DIST/BUILD-INFO" ] || fail "missing BUILD-INFO"
    ( cd "$DIST" && sha_check SHA256SUMS ) || fail "SHA256SUMS does not verify"
    verify_info "$DIST/BUILD-INFO"
    [ "$(sed -n 's/^platform=//p' "$DIST/BUILD-INFO")" = "$platform" ] \
      || fail "BUILD-INFO platform is not $platform"
    [ "$(sed -n 's/^reproducible=//p' "$DIST/BUILD-INFO")" = "not-byte-reproducible" ] \
      || fail "BUILD-INFO does not declare the reproducibility policy"
    local hash name
    while read -r hash name; do
      case "$name" in
        SHA256SUMS|BUILD-INFO) continue ;;
      esac
      grep -qx "artifact=$name" "$DIST/BUILD-INFO" \
        || fail "BUILD-INFO lacks artifact=$name (covered by SHA256SUMS)"
    done <"$DIST/SHA256SUMS"
    ok "acceptance 1: SHA256SUMS and BUILD-INFO agree with HEAD"
  else
    for artifact in "$CLI" "$DMG"; do
      name=$(basename "$artifact")
      [ -f "$ART/$name.sha256" ] || fail "missing $name.sha256"
      ( cd "$ART" && sha_check "$name.sha256" ) || fail "$name.sha256 does not verify"
      verify_info "$ART/$name.build-info" "$artifact"
    done
    ok "build-only: shard SHA256 files and build-info agree with HEAD"
  fi
}

check_schema() {
  local layout="$TMP/layout"
  [ -x "$layout/printkit" ] || fail "CLI was not extracted"
  if "$layout/printkit" schema --path >"$TMP/no-schema.out" 2>"$TMP/no-schema.err"; then
    fail "printkit schema --path succeeded without a schema directory"
  fi
  cat "$TMP/no-schema.out" "$TMP/no-schema.err" >"$TMP/no-schema.all"
  grep -q 'unpack the release' "$TMP/no-schema.all" \
    || fail "schema --path did not explain how to supply schema/"

  local source_schema="$root/schema"
  if [ "$BUILD_ONLY" -eq 0 ]; then
    [ -f "$SCHEMA_ARCHIVE" ] || fail "missing $(basename "$SCHEMA_ARCHIVE")"
    tar -tzf "$SCHEMA_ARCHIVE" >"$TMP/schema-members"
    for member in schema/ schema/document-v1.json schema/protocol-v1.json \
      assets/fonts/OFL.txt assets/licenses/MPL-2.0.txt; do
      grep -qx "$member" "$TMP/schema-members" || fail "schema archive lacks $member"
    done
    [ -f "$DIST/schema/document-v1.json" ] || fail "missing dist/schema/document-v1.json"
    tar -xzf "$SCHEMA_ARCHIVE" -C "$layout"
    source_schema="$DIST/schema"
  else
    cp -R "$root/schema" "$layout/schema"
    ok "build-only: schema archive/assembled SHA256SUMS are produced by --assemble"
  fi

  local schema_path
  schema_path=$("$layout/printkit" schema --path)
  schema_path=$(cd "$schema_path" && pwd -P)
  local expected
  expected=$(cd "$layout/schema" && pwd -P)
  [ "$schema_path" = "$expected" ] \
    || fail "schema --path points at $schema_path, expected $expected"
  diff -r "$layout/schema" "$source_schema" >/dev/null \
    || fail "extracted schema differs from $source_schema"

  local bin_schema="$TMP/bin-schema"
  mkdir -p "$bin_schema"
  "$layout/printkit" schema --out "$bin_schema" >/dev/null
  diff -r "$bin_schema" "$source_schema" >/dev/null \
    || fail "binary-embedded schema differs from $source_schema"
  ok "acceptance 5: schema path and binary-embedded schema match"
}

check_archives() {
  local members
  members=$(tar -tzf "$CLI")
  [ "$members" = "printkit" ] || fail "$(basename "$CLI") contains more than printkit"
  mkdir -p "$TMP/layout"
  tar -xzf "$CLI" -C "$TMP/layout"
  [ -x "$TMP/layout/printkit" ] || fail "CLI archive has no executable printkit"

  [ -f CONTRIBUTING.md ] || fail "missing CONTRIBUTING.md"
  [ -d .github/ISSUE_TEMPLATE ] || fail "missing .github/ISSUE_TEMPLATE"
  ok "acceptance 4: CLI archive is one executable; contribution entries exist"
}

check_portable() {
  local layout="$TMP/layout"
  touch "$layout/portable.flag"
  local config
  config=$("$TMP/layout/printkit" config --json)
  jq -e --arg dir "$layout/data" '.data_dir.value == $dir' <<<"$config" >/dev/null \
    || fail "portable mode data_dir is not $layout/data"
  mkdir -p "$layout/data"
  "$layout/printkit" pdf -i "$root/tests/fixtures/minimal.json" -o "$layout/portable-core.pdf" --json >/dev/null
  pdftotext "$layout/portable-core.pdf" - | grep -q 'printkit headless PDF fixture' \
    || fail "portable PDF content is missing with the default core preset"
  if [ -d "$layout/data/logs" ] \
    && find "$layout/data/logs" -maxdepth 1 -type f | grep -q .; then
    fail "default core/observability off wrote logs under data/logs"
  fi
  printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$layout/data/plugins.toml"
  "$layout/printkit" pdf -i "$root/tests/fixtures/minimal.json" -o "$layout/portable.pdf" --json >/dev/null
  pdftotext "$layout/portable.pdf" - | grep -q 'printkit headless PDF fixture' \
    || fail "portable PDF content is missing with plugins enabled"
  portable_log=$(find "$layout/data/logs" -maxdepth 1 -type f -name 'printkit-*.jsonl' -print -quit)
  [ -n "$portable_log" ] || fail "portable mode wrote no JSONL under data/logs"
  grep -q '"stage":"recv"' "$portable_log" \
    || fail "portable JSONL has no recv event"
  stray_log=$(find "$layout" -path "$layout/data/logs" -prune -o -name '*.jsonl' -print -quit)
  [ -z "$stray_log" ] || fail "portable mode wrote JSONL outside data/logs: $stray_log"
  ok "acceptance 6: default core writes no logs; preset all writes under data/logs"
}

check_offline_pdf() {
  mkdir -p "$TMP/offline-data"
  command -v sandbox-exec >/dev/null 2>&1 \
    || fail "sandbox-exec is required to prove the offline PDF path"
  sandbox-exec -p '(version 1)(allow default)(deny network*)' \
    "$TMP/layout/printkit" pdf -i "$root/tests/fixtures/minimal.json" \
    --data-dir "$TMP/offline-data" -o "$TMP/offline.pdf" --json >/dev/null
  pdftotext "$TMP/offline.pdf" - | grep -q 'printkit headless PDF fixture' \
    || fail "offline PDF content is missing"
  ok "acceptance 4: offline PDF succeeds"
}

# Developer ID is the release path; a local ad-hoc build is accepted but
# labelled. Unknown signatures fail closed.
signature_kind() {
  if grep -q '^Signature=adhoc$' <<<"$1"; then
    echo adhoc
  elif grep -q '^Authority=Developer ID Application' <<<"$1"; then
    echo developer-id
  else
    echo unknown
  fi
}

check_dmg_signature() {
  MOUNT="$TMP/mount"
  mkdir -p "$MOUNT"
  hdiutil attach -nobrowse -readonly -mountpoint "$MOUNT" "$DMG" >/dev/null
  local app
  app=$(find "$MOUNT" -maxdepth 1 -type d -name '*.app' | sort | tail -n 1)
  [ -n "$app" ] || fail "$(basename "$DMG") contains no .app bundle"
  [ -x "$app/Contents/MacOS/printkit-desktop" ] || fail "app lacks printkit-desktop"
  [ -x "$app/Contents/MacOS/printkit" ] || fail "app lacks printkit sidecar"
  [ -d "$app/Contents/_CodeSignature" ] || fail "app lacks Contents/_CodeSignature"

  local verify_out
  if ! verify_out=$(codesign --verify --deep --strict --verbose=2 "$app" 2>&1); then
    printf '%s\n' "$verify_out" >&2
    fail "codesign --verify --deep --strict failed"
  fi
  printf 'codesign: %s\n' "$verify_out"

  local info sig_kind
  info=$(codesign -dvvv "$app" 2>&1)
  grep -q '^Identifier=dev.printkit.desktop$' <<<"$info" \
    || fail "app signature identifier is not dev.printkit.desktop"
  sig_kind=$(signature_kind "$info")
  [ "$sig_kind" != unknown ] || fail "app is neither Developer ID nor ad-hoc signed"
  grep -q '^Format=app bundle' <<<"$info" || fail "app is not a bundle signature"
  printf 'app signature: %s (%s)\n' "$sig_kind" "$(grep '^CDHash=' <<<"$info" | head -1)"

  local bin_name bin_out
  for bin_name in printkit printkit-desktop; do
    codesign --verify --strict "$app/Contents/MacOS/$bin_name" >/dev/null \
      || fail "$bin_name signature does not verify"
    bin_out=$(codesign -dvvv "$app/Contents/MacOS/$bin_name" 2>&1)
    [ "$(signature_kind "$bin_out")" != unknown ] \
      || fail "$bin_name is neither Developer ID nor ad-hoc signed"
  done
  ok "acceptance 4: app and sidecar signatures are Developer ID or labelled ad-hoc"

  local spctl_out spctl_status
  set +e
  spctl_out=$(spctl -a -t exec --raw "$app" 2>&1)
  spctl_status=$?
  set -e
  if [ "$spctl_status" -eq 0 ]; then
    echo "spctl: accepted"
  elif [ "$spctl_status" -eq 3 ] && grep -q 'assessment:verdict' <<<"$spctl_out" \
    && grep -q 'false' <<<"$spctl_out"; then
    if [ "$sig_kind" = developer-id ]; then
      printf '%s\n' "$spctl_out" >&2
      fail "a Developer ID bundle must pass spctl (notarization missing?)"
    fi
    echo "spctl: rejected (expected for a local ad-hoc signature)"
  else
    printf '%s\n' "$spctl_out" >&2
    fail "spctl rejected the app for an unexpected reason (exit $spctl_status)"
  fi

  hdiutil detach "$MOUNT" >/dev/null 2>&1 || true
  MOUNT=""
}

check_sbom_and_licenses() {
  if [ "$BUILD_ONLY" -eq 1 ]; then
    echo "build-only: SBOM/licenses are assembled by --assemble (not in shards)"
    return
  fi
  [ -f "$DIST/sbom.cdx.json" ] || fail "missing sbom.cdx.json"
  jq -e '[.components[] | select(.purl | startswith("pkg:cargo/"))] | length > 0' \
    "$DIST/sbom.cdx.json" >/dev/null || fail "SBOM has no Cargo components"
  jq -e '[.components[] | select(.purl | startswith("pkg:npm/"))] | length > 0' \
    "$DIST/sbom.cdx.json" >/dev/null || fail "SBOM has no npm components"
  # The bundle must carry the version this checkout declares, not a literal.
  local expected_version
  expected_version="$(awk '
    /^\[workspace.package\]/ { found = 1; next }
    found && /^version = / { gsub(/"/, "", $3); print $3; exit }
  ' Cargo.toml)"
  [ -n "$expected_version" ] || fail "cannot read the workspace version from Cargo.toml"
  jq -e --arg version "$expected_version" \
    '.components[] | select(.name == "printkit" and .version == $version)' \
    "$DIST/sbom.cdx.json" >/dev/null \
    || fail "SBOM lacks the printkit first-party entry at $expected_version"
  jq -e '.components[] | select(.name == "printkit-platform-win")' \
    "$DIST/sbom.cdx.json" >/dev/null || fail "SBOM lacks printkit-platform-win"
  if compgen -G "$DIST/shards/*windows*" >/dev/null; then
    jq -e '.components[] | select(.name == "webview2-com" or .name == "windows")' \
      "$DIST/sbom.cdx.json" >/dev/null || fail "SBOM lacks Windows-only components"
  else
    # A local macOS-only assemble has no Windows shards to merge. Say so
    # explicitly; the CI/release job with Windows shards takes the branch above.
    printf 'ok: acceptance 9: macOS-only bundle; Windows-only SBOM components checked in the Windows release job\n'
  fi
  python3 scripts/gen-notices.py --check --sbom "$DIST/sbom.cdx.json" \
    --licenses-dir assets/licenses
  cmp -s THIRD-PARTY-NOTICES.md "$DIST/THIRD-PARTY-NOTICES.md" \
    || fail "release NOTICES differ from the repository copy"
  local license
  for license in assets/licenses/*.txt; do
    [ -f "$DIST/assets/licenses/$(basename "$license")" ] \
      || fail "release bundle lacks assets/licenses/$(basename "$license")"
  done

  local node_report="$TMP/desktop-licenses.json"
  local node_runtime="$TMP/desktop-runtime.json"
  pnpm --dir apps/desktop licenses list --json >"$node_report"
  pnpm --dir apps/desktop licenses list --prod --json >"$node_runtime"
  python3 scripts/node-license-check.py "$node_report"
  python3 scripts/node-license-check.py --components "$TMP/node-packages.json" "$node_runtime"
  ( cd apps/desktop/src-tauri && cargo deny check licenses >/dev/null )
  ok "acceptance 9: SBOM components, notices and license gates"
}

check_workspace_isolation
check_sums_and_info
check_archives
check_schema
check_dmg_signature
check_offline_pdf
check_portable
check_sbom_and_licenses
if [ "$BUILD_ONLY" -eq 1 ]; then
  echo "PARTIAL: S16 build-only checks passed; skipped: assembled SHA256SUMS/BUILD-INFO, schema archive, SBOM/licenses"
else
  echo "PASS: S16 acceptance 3/4/6/9"
fi

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
