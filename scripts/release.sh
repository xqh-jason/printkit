#!/usr/bin/env bash
# Build and assemble the release bundle for the current host.
#
# `--build-only` emits shards for the current host (macOS arm64/x64 and
# Windows x64). `--assemble` runs on both hosts and also folds in desktop
# installers built by other platforms, so a macOS publish job can ship the
# Windows NSIS/MSI installers. Windows installers get a structural check only;
# Authenticode signing and verification are W4.
#
# Without a mode the script runs both phases: `--build-only` builds the CLI and
# the desktop console for the current host and writes per-artifact SHA256
# shards; `--assemble` verifies those shards, adds the public contracts,
# notices and SBOM, and writes SHA256SUMS for the bundle.
#
# Both phases use one output directory: build-only writes `shards/`, assemble
# reads it and writes the release items beside it.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'USAGE'
usage: scripts/release.sh [--all|--build-only|--assemble|--sbom-only] [--dist DIR]
       scripts/release.sh --build-only [--skip-cli] [--skip-desktop]

  --all           Build this host's artifacts, then assemble the bundle
                  (default when no mode is given).
  --build-only    Build this host's CLI and desktop artifacts plus SHA256 shards.
  --assemble      Verify the shards and write the complete release bundle.
                  Runs on macOS and Windows; desktop installers from other
                  platforms in the shard directory are published as-is.
  --sbom-only     Generate dist/sbom.cdx.json without building artifacts
                  (CI notice freshness check).
  --dist DIR      Output directory (default: dist).
  --skip-cli      Do not build the CLI artifact (CI matrix job).
  --skip-desktop  Do not build the desktop artifact (CI matrix job).
  --allow-dirty   With --build-only, record dirty=1 instead of refusing a
                  dirty worktree; --assemble always requires a clean tree.
USAGE
}

die() {
  echo "release: $*" >&2
  exit 1
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

sha256_check() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$@"
  else
    shasum -a 256 -c "$@"
  fi
}
worktree_dirty() {
  [ -n "$(git status --porcelain)" ]
}

package_version() {
  sed -n '/^\[package\]$/,/^\[/p' "$1" \
    | sed -n 's/^version = "\(.*\)"/\1/p' \
    | head -n 1
}

workspace_version() {
  sed -n '/^\[workspace.package\]$/,/^\[/p' "$1" \
    | sed -n 's/^version = "\(.*\)"/\1/p' \
    | head -n 1
}

verify_versions() {
  local root desktop tauri conf sdk i18n
  root="$(workspace_version Cargo.toml)"
  [ -n "$root" ] || die "workspace version is missing from Cargo.toml"
  desktop="$(jq -r '.version // empty' apps/desktop/package.json)"
  tauri="$(package_version apps/desktop/src-tauri/Cargo.toml)"
  conf="$(jq -r '.version // empty' apps/desktop/src-tauri/tauri.conf.json)"
  sdk="$(jq -r '.version // empty' packages/sdk-ts/package.json)"
  i18n="$(jq -r '.version // empty' packages/i18n/package.json)"
  local file value
  for pair in \
    "apps/desktop/package.json:$desktop" \
    "apps/desktop/src-tauri/Cargo.toml:$tauri" \
    "apps/desktop/src-tauri/tauri.conf.json:$conf" \
    "packages/sdk-ts/package.json:$sdk" \
    "packages/i18n/package.json:$i18n"; do
    file="${pair%%:*}"
    value="${pair#*:}"
    [ -n "$value" ] || die "$file has no version"
    [ "$value" = "$root" ] || die "$file is $value, Cargo.toml says $root"
  done
  echo "versions: $root across Cargo.toml, desktop package/Cargo/tauri.conf, SDK and i18n"
}

write_build_info() {
  local artifact="$1"
  {
    echo "commit=$COMMIT"
    echo "dirty=$DIRTY"
    echo "artifact=$artifact"
    echo "platform=$PLATFORM"
    echo "rustc=$RUSTC_VERSION"
    echo "built=$BUILT_AT"
    echo "source_date_epoch=$SOURCE_DATE_EPOCH"
    echo "reproducible=not-byte-reproducible"
  } >"$SHARDS/$artifact.build-info"
}

MODE=""
DIST="dist"
SKIP_CLI=0
SKIP_DESKTOP=0
ALLOW_DIRTY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --all|--build-only|--assemble|--sbom-only)
      [ -z "$MODE" ] || die "choose one of --all, --build-only, --assemble and --sbom-only"
      MODE="${1#--}"
      shift
      ;;
    --dist)
      [ $# -ge 2 ] || die "--dist needs a directory"
      DIST="$2"
      shift 2
      ;;
    --skip-cli)
      SKIP_CLI=1
      shift
      ;;
    --skip-desktop)
      SKIP_DESKTOP=1
      shift
      ;;
    --allow-dirty)
      ALLOW_DIRTY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[ -n "$MODE" ] || MODE="all"
if [ "$MODE" != "build-only" ]; then
  [ "$SKIP_CLI" -eq 0 ] && [ "$SKIP_DESKTOP" -eq 0 ] \
    || die "--skip-cli and --skip-desktop apply to --build-only"
fi
if [ "$ALLOW_DIRTY" -ne 0 ] && [ "$MODE" != "build-only" ]; then
  die "--allow-dirty applies to --build-only"
fi

SHARDS="$DIST/shards"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

host_triple() {
  rustc -vV | sed -n 's/^host: //p'
}

host_is_windows() {
  case "$(host_triple)" in
    *-pc-windows-msvc) return 0 ;;
    *) return 1 ;;
  esac
}

platform_name() {
  case "$1" in
    aarch64-apple-darwin) echo "macos-arm64" ;;
    x86_64-apple-darwin) echo "macos-x64" ;;
    x86_64-pc-windows-msvc) echo "windows-x64" ;;
    *) die "release packaging does not support this host yet: $1" ;;
  esac
}

# A release expects exactly one Tauri bundle per installer type. A missing
# bundle is an error, not a silent skip, and so is a second one that would
# otherwise be dropped on the floor.
single_bundle_file() {
  local dir="$1" pattern="$2" label="$3"
  local files count
  files="$(find "$dir" -maxdepth 1 -type f -name "$pattern" 2>/dev/null \
    | LC_ALL=C sort)"
  [ -n "$files" ] \
    || die "Tauri produced no $label under $dir; bundle.targets must include it"
  count="$(printf '%s\n' "$files" | wc -l | tr -d ' ')"
  [ "$count" -eq 1 ] \
    || die "Tauri produced $count ${label}s under $dir; the release expects exactly one"
  printf '%s\n' "$files"
}

# Copy one Tauri installer into the shard directory under its release name and
# write the sidecar checksum and build metadata that --assemble verifies.
publish_installer() {
  local source="$1" name="$2"
  cp "$source" "$SHARDS/$name"
  (
    cd "$SHARDS"
    sha256 "$name" >"$name.sha256"
  )
  write_build_info "$name"
  echo "built $name"
}

assert_no_dynamic_sqlite() {
  local binary="$1"
  if command -v otool >/dev/null 2>&1; then
    if otool -L "$binary" | grep -q 'libsqlite3'; then
      die "$(basename "$binary") dynamically links libsqlite3; build with --features release"
    fi
    return
  fi
  # Windows: the import table is the only place a dynamic sqlite3 would show
  # up. `dumpbin` ships with the MSVC toolchain the build already needs.
  command -v dumpbin >/dev/null 2>&1 \
    || die "dumpbin is required to verify the Windows binary links SQLite statically"
  if dumpbin /dependents "$binary" | grep -qi 'sqlite3'; then
    die "$(basename "$binary") dynamically links sqlite3; build with --features release"
  fi
}

build_cli() {
  PRINTKIT_RELEASE=1 cargo build --locked --release --features release -p printkit
  local suffix="" tar_flags=()
  if host_is_windows; then
    suffix=".exe"
    # Windows 10+ ships bsdtar, but `--no-mac-metadata` is a macOS-only flag.
  else
    tar_flags=(--no-mac-metadata)
  fi
  local binary="$PWD/target/release/printkit$suffix"
  [ -x "$binary" ] || die "missing CLI binary: $binary"
  assert_no_dynamic_sqlite "$binary"

  local stage="$TMP/cli"
  mkdir -p "$stage"
  cp "$binary" "$stage/printkit$suffix"
  COPYFILE_DISABLE=1 tar -czf "$SHARDS/printkit-$PLATFORM.tar.gz" \
    -C "$stage" ${tar_flags[@]+"${tar_flags[@]}"} "printkit$suffix"
  (
    cd "$SHARDS"
    sha256 "printkit-$PLATFORM.tar.gz" >"printkit-$PLATFORM.tar.gz.sha256"
  )
  write_build_info "printkit-$PLATFORM.tar.gz"
  echo "built printkit-$PLATFORM.tar.gz"
}

build_desktop() {
  local arch_name sidecar bundle_root setup msi artifact
  case "$(host_triple)" in
    aarch64-apple-darwin) arch_name="aarch64" ;;
    x86_64-apple-darwin) arch_name="x64" ;;
    *) arch_name="" ;;
  esac

  pnpm --dir apps/desktop install --frozen-lockfile
  pnpm --dir apps/desktop tauri build

  sidecar="apps/desktop/src-tauri/binaries/printkit-$(host_triple)"
  if host_is_windows; then
    # Tauri appends `.exe` when resolving the externalBin on Windows.
    sidecar="$sidecar.exe"
    [ -x "$sidecar" ] || die "missing desktop sidecar: $sidecar"
    assert_no_dynamic_sqlite "$sidecar"
    # tauri.windows.conf.json asks for both bundles; shipping only one of them
    # is an incomplete release, so neither may be dropped silently.
    bundle_root="apps/desktop/src-tauri/target/release/bundle"
    setup="$(single_bundle_file "$bundle_root/nsis" '*-setup.exe' "NSIS installer")"
    msi="$(single_bundle_file "$bundle_root/msi" '*.msi' "MSI installer")"
    publish_installer "$setup" "printkit-desktop-$PLATFORM-setup.exe"
    publish_installer "$msi" "printkit-desktop-$PLATFORM.msi"
    echo "signing is W4: the Windows installers are unsigned"
    return
  fi

  [ -n "$arch_name" ] || die "unsupported desktop host: $(host_triple)"
  [ -x "$sidecar" ] || die "missing desktop sidecar: $sidecar"
  assert_no_dynamic_sqlite "$sidecar"

  local bundle_mac="apps/desktop/src-tauri/target/release/bundle/dmg"
  local dmg
  dmg="$(find "$bundle_mac" -maxdepth 1 -type f -name "*_$arch_name.dmg" | sort | tail -n 1)"
  [ -n "$dmg" ] || die "no $arch_name .dmg under $bundle_mac"
  cp "$dmg" "$SHARDS/printkit-desktop-$PLATFORM.dmg"
  (
    cd "$SHARDS"
    sha256 "printkit-desktop-$PLATFORM.dmg" >"printkit-desktop-$PLATFORM.dmg.sha256"
  )
  write_build_info "printkit-desktop-$PLATFORM.dmg"
  echo "built printkit-desktop-$PLATFORM.dmg"
}

sbom_only() {
  verify_versions
  mkdir -p "$DIST"
  write_sbom
}

build_only() {
  COMMIT="$(git rev-parse HEAD)"
  RUSTC_VERSION="$(rustc -V)"
  BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git show -s --format=%ct "$COMMIT")}"
  export SOURCE_DATE_EPOCH
  DIRTY=0
  if worktree_dirty; then
    DIRTY=1
    [ "$ALLOW_DIRTY" -eq 1 ] \
      || die "the worktree has uncommitted changes; commit them or pass --allow-dirty"
  fi
  verify_versions
  mkdir -p "$SHARDS"
  PLATFORM="$(platform_name "$(host_triple)")"
  if [ "$SKIP_CLI" -eq 0 ]; then
    build_cli
  fi
  if [ "$SKIP_DESKTOP" -eq 0 ]; then
    build_desktop
  fi
  echo "shards: $SHARDS"
}

node_license_report() {
  command -v pnpm >/dev/null 2>&1 || die "pnpm is required for the Node license gate"
  pnpm --dir apps/desktop licenses list --json >"$TMP/desktop-licenses.json"
  python3 scripts/node-license-check.py "$TMP/desktop-licenses.json"
  pnpm --dir apps/desktop licenses list --prod --json >"$TMP/desktop-runtime.json"
  python3 scripts/node-license-check.py \
    --components "$TMP/node-packages.json" "$TMP/desktop-runtime.json"
}

check_desktop_licenses() {
  cargo deny --version >/dev/null 2>&1 \
    || die "cargo-deny is required: cargo install cargo-deny --locked"
  (
    cd apps/desktop/src-tauri
    cargo deny check licenses
  )
}

write_sbom() {
  command -v jq >/dev/null 2>&1 || die "jq is required to merge the SBOM"
  cargo cyclonedx --version >/dev/null 2>&1 \
    || die "cargo-cyclonedx is required: cargo install cargo-cyclonedx --locked"

  node_license_report

  local tag=".release-sbom-$$"
  local cli_raw="$TMP/sbom-cli.json"
  # `--target all` keeps the inventory identical on every runner: a host-only
  # SBOM omits the other platforms' crates (GTK on Linux, WebView2 on Windows),
  # so CI and a local `--sbom-only` would disagree with the committed NOTICES.
  cargo cyclonedx --manifest-path apps/printkit/Cargo.toml --format json \
    --all-features --target all --override-filename "$tag" --quiet >/dev/null
  [ -f "apps/printkit/$tag.json" ] || die "cargo cyclonedx produced no CLI SBOM"
  cp "apps/printkit/$tag.json" "$cli_raw"

  local desktop_raw="$TMP/sbom-desktop.json"
  (
    cd apps/desktop/src-tauri
    cargo cyclonedx --format json --target all --override-filename "$tag" --quiet >/dev/null
  )
  [ -f "apps/desktop/src-tauri/$tag.json" ] \
    || die "cargo cyclonedx produced no desktop SBOM"
  cp "apps/desktop/src-tauri/$tag.json" "$desktop_raw"
  find crates apps plugins -type f -name "$tag.json" -delete

  jq '[.[] | . as $package | ($package.name | gsub("@"; "%40")) as $name
      | {
          type: "library",
          name: $package.name,
          version: $package.version,
          purl: ("pkg:npm/" + $name + "@" + $package.version),
          "bom-ref": ("pkg:npm/" + $name + "@" + $package.version),
          licenses: [{expression: $package.license}]
        }]
      | unique_by(."bom-ref")' \
    "$TMP/node-packages.json" >"$TMP/node-components.json"

  local tauri_version
  tauri_version="$(awk '
    /^name = "tauri"/ { found = 1; next }
    found && /^version = / { gsub(/"/, "", $3); print $3; exit }
  ' apps/desktop/src-tauri/Cargo.lock)"
  [ -n "$tauri_version" ] || die "tauri version not found in the desktop lockfile"

  jq --slurpfile desktop "$desktop_raw" --slurpfile node "$TMP/node-components.json" '
    .components as $existing
    | .components += (
        ($desktop[0].components + $node[0])
        | map(select(."bom-ref" as $ref
            | ($existing | map(."bom-ref") | index($ref)) | not))
      )
    | .components |= unique_by(."bom-ref")
  ' "$cli_raw" >"$TMP/sbom.cdx.json"

  # Merge the Windows SBOM shards uploaded by build-windows. This is what puts
  # windows-only crates (windows, webview2-com) and platform-win into the
  # release inventory even though assemble runs on macOS.
  local shard_sbom
  for shard_sbom in "$SHARDS"/*.sbom.json; do
    [ -f "$shard_sbom" ] || continue
    jq --slurpfile extra "$shard_sbom" '
      .components += $extra[0].components
      | .components |= unique_by(."bom-ref")
    ' "$TMP/sbom.cdx.json" >"$TMP/sbom.with-shard.json"
    mv "$TMP/sbom.with-shard.json" "$TMP/sbom.cdx.json"
  done

  local project_version i18n_version
  project_version="$(workspace_version Cargo.toml)"
  i18n_version="$(jq -r '.version // empty' packages/i18n/package.json)"
  jq --arg v "$project_version" --arg i18n "$i18n_version" '
    (if any(.components[]; .name == "printkit") then .
     else .components += [{
       type: "application",
       name: "printkit",
       version: $v,
       purl: ("pkg:cargo/printkit@" + $v),
       "bom-ref": ("pkg:cargo/printkit@" + $v),
       licenses: [{expression: "MIT OR Apache-2.0"}]
     }] end)
    | (if any(.components[]; .name == "printkit-platform-win") then .
       else .components += [{
         type: "library",
         name: "printkit-platform-win",
         version: $v,
         purl: ("pkg:cargo/printkit-platform-win@" + $v),
         "bom-ref": ("pkg:cargo/printkit-platform-win@" + $v),
         licenses: [{expression: "MIT OR Apache-2.0"}]
       }] end)
    | (if any(.components[]; .name == "@printkit/i18n") then
        .components |= map(if .name == "@printkit/i18n" then
          .version = $i18n
          | .purl = ("pkg:npm/%40printkit/i18n@" + $i18n)
          | ."bom-ref" = ("pkg:npm/%40printkit/i18n@" + $i18n)
        else . end)
       else .components += [{
         type: "library",
         name: "@printkit/i18n",
         version: $i18n,
         purl: ("pkg:npm/%40printkit/i18n@" + $i18n),
         "bom-ref": ("pkg:npm/%40printkit/i18n@" + $i18n),
         licenses: [{expression: "MIT OR Apache-2.0"}]
       }] end)
    | .components |= unique_by(."bom-ref")
  ' "$TMP/sbom.cdx.json" >"$TMP/sbom.with-first-party.json"
  mv "$TMP/sbom.with-first-party.json" "$TMP/sbom.cdx.json"

  cp "$TMP/sbom.cdx.json" "$DIST/sbom.cdx.json"
  echo "wrote sbom.cdx.json (tauri $tauri_version)"
}

verify_notices() {
  python3 scripts/gen-notices.py --check --sbom "$DIST/sbom.cdx.json" \
    --licenses-dir assets/licenses
  [ -f assets/fonts/OFL.txt ] || die "missing assets/fonts/OFL.txt"
  [ -f "$DIST/OFL.txt" ] || die "missing OFL.txt in the bundle"
  [ -f "$DIST/assets/fonts/OFL.txt" ] || die "missing assets/fonts/OFL.txt in the bundle"
  [ -f "$DIST/assets/licenses/MPL-2.0.txt" ] || die "missing MPL-2.0 text in the bundle"
  cmp -s THIRD-PARTY-NOTICES.md "$DIST/THIRD-PARTY-NOTICES.md" \
    || die "release NOTICES differ from the repository copy"
  local license
  for license in assets/licenses/*.txt; do
    cmp -s "$license" "$DIST/assets/licenses/$(basename "$license")" \
      || die "release license text differs from $license"
  done
}

verify_cli_archive() {
  local archive="$1" exe=""
  host_is_windows && exe=".exe"
  local members
  members="$(tar -tzf "$archive")"
  [ "$members" = "printkit$exe" ] \
    || die "$(basename "$archive") must contain only the printkit$exe executable"

  local layout="$TMP/layout"
  mkdir -p "$layout"
  tar -xzf "$archive" -C "$layout"

  if "$layout/printkit$exe" schema --path >"$TMP/no-schema.out" 2>"$TMP/no-schema.err"; then
    die "$(basename "$archive") prints a schema path without a schema directory"
  fi
  grep -q "unpack the release" "$TMP/no-schema.err" \
    || die "$(basename "$archive") does not explain how to supply schema/"

  local schema_archive="$DIST/printkit-schema-$PLATFORM.tar.gz"
  [ -f "$schema_archive" ] || die "missing $(basename "$schema_archive")"
  tar -xzf "$schema_archive" -C "$layout"
  [ -d "$layout/schema" ] || die "$(basename "$schema_archive") has no schema directory"
  [ -f "$layout/assets/licenses/MPL-2.0.txt" ] \
    || die "$(basename "$schema_archive") has no MPL-2.0 text"
  local schema_path expected_path
  schema_path="$("$layout/printkit$exe" schema --path)"
  schema_path="$(cd "$schema_path" && pwd -P)"
  expected_path="$(cd "$layout/schema" && pwd -P)"
  [ "$schema_path" = "$expected_path" ] \
    || die "$(basename "$archive") schema --path prints $schema_path, not $expected_path"
  diff -r "$schema_path" "$DIST/schema" >/dev/null \
    || die "printkit schema --path does not match the bundled schema"
  cmp schema/document-v1.json "$DIST/schema/document-v1.json"
  cmp schema/protocol-v1.json "$DIST/schema/protocol-v1.json"
  # The binary embeds the schema it was compiled with. A stale shard built
  # before a schema change passes the path check above, so compare the bytes
  # the binary writes with the contracts in the bundle.
  local bin_schema="$TMP/bin-schema"
  mkdir -p "$bin_schema"
  "$layout/printkit$exe" schema --out "$bin_schema" >/dev/null
  diff -r "$bin_schema" "$DIST/schema" >/dev/null \
    || die "$(basename "$archive") embeds a schema that differs from $DIST/schema; rebuild the shard"
  verify_tag_version "$layout/printkit$exe"
  echo "verified $(basename "$archive"): one executable, schema path matches"
}

# Windows installers are unsigned until W4, and inspecting an MSI or an NSIS
# executable needs Windows tooling. What --assemble can still do everywhere is
# prove the shard is the real container rather than a truncated or HTML file.
verify_windows_installer() {
  local installer="$1" name magic
  name="$(basename "$installer")"
  [ -s "$installer" ] || die "$name is missing or empty"
  magic="$(od -An -tx1 -N8 "$installer" 2>/dev/null | tr -d ' \n' || true)"
  if [ -z "$magic" ]; then
    echo "warning: od is unavailable; skipped the file-header check for $name" >&2
    return
  fi
  case "$name" in
    *.msi)
      [ "$magic" = "d0cf11e0a1b11ae1" ] \
        || die "$name is not an OLE compound file, so it is not an MSI"
      ;;
    *-setup.exe)
      [ "${magic:0:4}" = "4d5a" ] \
        || die "$name is not a PE executable, so it is not an NSIS installer"
      ;;
    *) die "unknown Windows installer type: $name" ;;
  esac
  echo "verified $name: unsigned Windows installer (Authenticode is W4)"
}

verify_build_info() {
  local artifact="$1"
  local name info commit dirty platform recorded
  name="$(basename "$artifact")"
  info="$SHARDS/$name.build-info"
  [ -f "$info" ] || die "missing $name.build-info; rebuild the shard on a committed tree"
  commit="$(sed -n 's/^commit=//p' "$info")"
  dirty="$(sed -n 's/^dirty=//p' "$info")"
  platform="$(sed -n 's/^platform=//p' "$info")"
  recorded="$(sed -n 's/^artifact=//p' "$info")"
  [ "$recorded" = "$name" ] || die "$name.build-info names '$recorded', not '$name'"
  [ "$commit" = "$(git rev-parse HEAD)" ] \
    || die "$name was built from $commit but HEAD is $(git rev-parse HEAD); rebuild the shard"
  [ "$dirty" = "0" ] || die "$name was built from a dirty worktree and must not be published"
  [ -n "$platform" ] || die "$name.build-info has no platform"
}
verify_tag_version() {
  local tag="${GITHUB_REF_NAME:-}"
  local expected actual
  case "$tag" in
    v[0-9]*) expected="${tag#v}" ;;
    [0-9]*) expected="$tag" ;;
    *) return ;;
  esac
  actual="$("$1" version | awk '{print $2}')"
  [ "$actual" = "$expected" ] \
    || die "binary version $actual does not match tag $tag"
}

# Developer ID is the release path; a local ad-hoc build is accepted but
# labelled as such. Unknown signatures fail closed.
signature_kind() {
  if grep -q '^Signature=adhoc$' <<<"$1"; then
    echo adhoc
  elif grep -q '^Authority=Developer ID Application' <<<"$1"; then
    echo developer-id
  else
    echo unknown
  fi
}

signature_required() {
  [ "$1" != unknown ]
}
verify_desktop_archive() {
  local dmg="$1"
  local mount="$TMP/dmg-mount"
  local app verify_out info spctl_out spctl_status bin_name bin_out signature_kind
  mkdir -p "$mount"
  hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$dmg" >/dev/null

  detach_desktop_dmg() {
    hdiutil detach "$mount" >/dev/null 2>&1 || true
  }
  fail_desktop() {
    detach_desktop_dmg
    die "$*"
  }

  app="$(find "$mount" -maxdepth 1 -type d -name '*.app' | sort | tail -n 1)"
  [ -n "$app" ] || fail_desktop "$(basename "$dmg") contains no .app bundle"
  [ -x "$app/Contents/MacOS/printkit-desktop" ] \
    || fail_desktop "$(basename "$dmg") has no printkit-desktop executable"
  [ -x "$app/Contents/MacOS/printkit" ] \
    || fail_desktop "$(basename "$dmg") has no bundled printkit sidecar"
  [ -d "$app/Contents/_CodeSignature" ] \
    || fail_desktop "$(basename "$dmg") app has no Contents/_CodeSignature"

  if ! verify_out="$(codesign --verify --deep --strict --verbose=2 "$app" 2>&1)"; then
    printf '%s\n' "$verify_out" >&2
    fail_desktop "codesign --verify --deep --strict failed for $(basename "$dmg")"
  fi
  printf 'codesign: %s\n' "$verify_out"

  info="$(codesign -dvvv "$app" 2>&1)"
  grep -q '^Identifier=dev.printkit.desktop$' <<<"$info" \
    || fail_desktop "app signature identifier is not dev.printkit.desktop"
  signature_kind="$(signature_kind "$info")"
  signature_required "$signature_kind" \
    || fail_desktop "app is neither Developer ID nor ad-hoc signed"
  grep -q '^Format=app bundle' <<<"$info" \
    || fail_desktop "app is not a bundle signature"
  printf 'desktop signature: %s (%s)\n' "$signature_kind" \
    "$(grep '^CDHash=' <<<"$info" | head -n 1)"

  for bin_name in printkit printkit-desktop; do
    codesign --verify --strict "$app/Contents/MacOS/$bin_name" >/dev/null \
      || fail_desktop "$bin_name signature does not verify"
    bin_out="$(codesign -dvvv "$app/Contents/MacOS/$bin_name" 2>&1)"
    signature_required "$(signature_kind "$bin_out")" \
      || fail_desktop "$bin_name is neither Developer ID nor ad-hoc signed"
  done

  # An ad-hoc bundle has no trusted Developer ID authority, so Gatekeeper
  # rejects it even when the bundle signature is structurally complete.
  # Treat that exact verdict as expected; any other spctl failure is fatal.
  set +e
  spctl_out="$(spctl -a -t exec --raw "$app" 2>&1)"
  spctl_status=$?
  set -e
  if [ "$spctl_status" -eq 0 ]; then
    echo "spctl: accepted"
  elif [ "$spctl_status" -eq 3 ] && grep -q 'assessment:verdict' <<<"$spctl_out" \
    && grep -q 'false' <<<"$spctl_out"; then
    if [ "$signature_kind" = developer-id ]; then
      printf '%s\n' "$spctl_out" >&2
      fail_desktop "a Developer ID bundle must pass spctl (notarization missing?)"
    fi
    echo "spctl: rejected (expected for a local ad-hoc signature)"
  else
    printf '%s\n' "$spctl_out" >&2
    fail_desktop "spctl rejected the app for an unexpected reason (exit $spctl_status)"
  fi

  detach_desktop_dmg
  echo "verified $(basename "$dmg"): app/sidecar signed ($signature_kind); codesign structure valid"
}

assemble() {
  [ -d "$SHARDS" ] || die "no shards directory: $SHARDS"
  worktree_dirty \
    && die "assemble requires a clean worktree; commit or remove the changes first"
  verify_versions
  PLATFORM="$(platform_name "$(host_triple)")"
  local checksum
  for checksum in "$SHARDS"/*.sha256; do
    [ -f "$checksum" ] || die "no SHA256 shards in $SHARDS"
    (
      cd "$SHARDS"
      sha256_check "$(basename "$checksum")"
    )
  done

  local shard
  for shard in "$SHARDS"/printkit-*.tar.gz "$SHARDS"/printkit-desktop-*.dmg \
    "$SHARDS"/printkit-desktop-windows-*.msi \
    "$SHARDS"/printkit-desktop-windows-*-setup.exe; do
    [ -f "$shard" ] || continue
    verify_build_info "$shard"
  done

  # Only the host's CLI archive can be executed here, so the bundle carries
  # exactly that one; another platform's CLI shard fails loudly instead of
  # being published unverified.
  local cli_archive="$SHARDS/printkit-$PLATFORM.tar.gz"
  [ -f "$cli_archive" ] || die "missing CLI shard: $(basename "$cli_archive")"
  for shard in "$SHARDS"/printkit-*.tar.gz; do
    [ -f "$shard" ] || continue
    [ "$shard" = "$cli_archive" ] \
      || die "$(basename "$shard") is a non-host CLI shard; assemble it on its own platform"
  done

  # The CI matrix merges one shard set per platform into $SHARDS, so collect
  # every desktop installer. The host's own set must be complete, and a
  # Windows release ships NSIS and MSI or it is not a release.
  local desktop_images=() image msi_shards setup_shards
  for shard in "$SHARDS"/printkit-desktop-*.dmg \
    "$SHARDS"/printkit-desktop-windows-*.msi \
    "$SHARDS"/printkit-desktop-windows-*-setup.exe; do
    [ -f "$shard" ] || continue
    desktop_images+=("$shard")
  done
  [ "${#desktop_images[@]}" -gt 0 ] || die "no desktop installer in $SHARDS"
  msi_shards=("$SHARDS"/printkit-desktop-windows-*.msi)
  setup_shards=("$SHARDS"/printkit-desktop-windows-*-setup.exe)
  if [ -f "${msi_shards[0]}" ] || [ -f "${setup_shards[0]}" ]; then
    [ -f "${msi_shards[0]}" ] || die "Windows shards carry no .msi installer"
    [ -f "${setup_shards[0]}" ] \
      || die "Windows shards carry no NSIS *-setup.exe installer"
  fi
  case "$PLATFORM" in
    windows-*)
      [ -f "$SHARDS/printkit-desktop-$PLATFORM.msi" ] \
        || die "missing desktop shard: printkit-desktop-$PLATFORM.msi"
      [ -f "$SHARDS/printkit-desktop-$PLATFORM-setup.exe" ] \
        || die "missing desktop shard: printkit-desktop-$PLATFORM-setup.exe"
      ;;
    *)
      [ -f "$SHARDS/printkit-desktop-$PLATFORM.dmg" ] \
        || die "missing desktop shard: printkit-desktop-$PLATFORM.dmg"
      ;;
  esac

  rm -rf "$DIST/schema" "$DIST/assets"
  rm -f "$DIST"/printkit-*.tar.gz "$DIST"/printkit-desktop-*.dmg \
    "$DIST"/printkit-desktop-windows-*.msi \
    "$DIST"/printkit-desktop-windows-*-setup.exe \
    "$DIST/SHA256SUMS" "$DIST/sbom.cdx.json" "$DIST/THIRD-PARTY-NOTICES.md" \
    "$DIST/LICENSE-MIT" "$DIST/LICENSE-APACHE" "$DIST/OFL.txt" "$DIST/BUILD-INFO"

  cp "$cli_archive" "$DIST/"
  cp "${desktop_images[@]}" "$DIST/"
  local source_date_epoch
  source_date_epoch="$(git show -s --format=%ct HEAD)"
  {
    echo "commit=$(git rev-parse HEAD)"
    echo "dirty=0"
    echo "built=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "source_date_epoch=$source_date_epoch"
    echo "reproducible=not-byte-reproducible"
    echo "platform=$PLATFORM"
    local artifact
    for artifact in "$cli_archive" "${desktop_images[@]}"; do
      echo "artifact=$(basename "$artifact")"
    done
    for artifact in "printkit-schema-$PLATFORM.tar.gz" "sbom.cdx.json" \
      "THIRD-PARTY-NOTICES.md" "LICENSE-MIT" "LICENSE-APACHE" "OFL.txt"; do
      echo "artifact=$artifact"
    done
  } >"$DIST/BUILD-INFO"
  [ "$(sed -n 's/^commit=//p' "$DIST/BUILD-INFO")" = "$(git rev-parse HEAD)" ] \
    || die "BUILD-INFO commit does not match HEAD"
  mkdir -p "$DIST/schema" "$DIST/assets/fonts"
  cp schema/document-v1.json schema/protocol-v1.json "$DIST/schema/"
  mkdir -p "$DIST/assets/licenses"
  cp THIRD-PARTY-NOTICES.md LICENSE-MIT LICENSE-APACHE "$DIST/"
  cp assets/fonts/OFL.txt "$DIST/OFL.txt"
  cp assets/fonts/OFL.txt "$DIST/assets/fonts/OFL.txt"
  cp assets/licenses/*.txt "$DIST/assets/licenses/"

  local tar_flags=()
  # `--no-mac-metadata` is a macOS bsdtar flag; Windows bsdtar rejects it.
  host_is_windows || tar_flags=(--no-mac-metadata)
  COPYFILE_DISABLE=1 tar -czf "$DIST/printkit-schema-$PLATFORM.tar.gz" \
    -C "$DIST" ${tar_flags[@]+"${tar_flags[@]}"} \
    schema assets/fonts/OFL.txt assets/licenses

  check_desktop_licenses
  write_sbom
  verify_notices

  (
    cd "$DIST"
    find . -maxdepth 1 -type f ! -name SHA256SUMS -print0 \
      | while IFS= read -r -d '' file; do
          sha256 "${file#./}"
        done \
      | LC_ALL=C sort -k2 >SHA256SUMS
    sha256_check SHA256SUMS
  )

  verify_cli_archive "$DIST/$(basename "$cli_archive")"
  for image in "${desktop_images[@]}"; do
    case "$image" in
      *.dmg) verify_desktop_archive "$DIST/$(basename "$image")" ;;
      *.msi|*-setup.exe) verify_windows_installer "$DIST/$(basename "$image")" ;;
      *) die "unknown desktop installer type: $(basename "$image")" ;;
    esac
  done

  echo "release items:"
  (
    cd "$DIST"
    find . -maxdepth 3 -type f ! -path './shards/*' | sed 's|^\./||' | LC_ALL=C sort
  )
}

case "$MODE" in
  all)
    build_only
    assemble
    ;;
  build-only) build_only ;;
  assemble) assemble ;;
  sbom-only) sbom_only ;;
esac

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
