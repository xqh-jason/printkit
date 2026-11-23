#!/usr/bin/env bash
# Build the printkit binary and place it where Tauri expects an externalBin.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
profile="${1:-release}"
triple="$(rustc -vV | sed -n 's/^host: //p')"

case "$profile" in
  release)
    PRINTKIT_RELEASE=1 cargo build --manifest-path "$repo_root/Cargo.toml" \
      -p printkit --release --features release
    ;;
  debug)
    cargo build --manifest-path "$repo_root/Cargo.toml" -p printkit
    ;;
  *)
    echo "usage: $0 [release|debug]" >&2
    exit 1
    ;;
esac

# Windows binaries carry `.exe`, and Tauri resolves an externalBin by the
# exact `name-triple[.exe]` form, so the suffix is part of both paths.
case "$triple" in
  *-pc-windows-*) suffix=".exe" ;;
  *) suffix="" ;;
esac

binary="$repo_root/target/$profile/printkit$suffix"
destination="$repo_root/apps/desktop/src-tauri/binaries/printkit-$triple$suffix"
mkdir -p "$(dirname "$destination")"
cp "$binary" "$destination"
echo "sidecar: $destination"
