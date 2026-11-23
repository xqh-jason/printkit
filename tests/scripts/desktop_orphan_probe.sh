#!/usr/bin/env bash
# SIGKILLing the shell must leave an orphaned sidecar that the next launch
# detects (parent launchd) and reclaims, instead of pinning external read-only.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

if [[ $(uname -s) != Darwin ]]; then
  echo "SKIP: the orphan probe needs a macOS GUI session" >&2
  exit 0
fi

triple=$(rustc -vV | sed -n 's/^host: //p')
sidecar="$root/apps/desktop/src-tauri/binaries/printkit-$triple"
if [[ ! -x $sidecar ]]; then
  bash apps/desktop/scripts/prepare-sidecar.sh debug
fi
desktop="$root/apps/desktop/src-tauri/target/debug/printkit-desktop"
if [[ ! -x $desktop ]]; then
  cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml
fi

work=$(mktemp -d)
data=$work/data
mkdir -p "$data"
port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
first_shell=
second_shell=
old_side=
new_side=
cleanup() {
  for pid in $first_shell $second_shell $old_side $new_side; do
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$work"
}
trap cleanup EXIT

launch() {
  PRINTKIT_DATA_DIR="$data" PRINTKIT_PORT="$port" \
  PRINTKIT_SIDECAR_PATH="$sidecar" \
    "$desktop" >"$work/shell-$1.log" 2>&1 &
  echo $!
  disown
}

launch first >"$work/first.pid"
first_shell=$(cat "$work/first.pid")
for _ in $(seq 1 150); do
  [[ -s $data/serve.lock ]] && break
  sleep 0.1
done
[[ -s $data/serve.lock ]] || { echo "FAIL: first shell never started the sidecar" >&2; exit 1; }
old_side=$(jq -r '.pid' "$data/serve.lock")
killed_shell=$first_shell
kill -9 "$first_shell"
first_shell=
sleep 2
if ! kill -0 "$old_side" 2>/dev/null; then
  echo "desktop_orphan_probe: ok (sidecar exited with SIGKILLed shell $killed_shell; no orphan left)"
  exit 0
fi
parent=$(ps -o ppid= -p "$old_side" | tr -d ' ')
[[ $parent == 1 ]] || { echo "FAIL: orphan parent is $parent, expected launchd(1)" >&2; exit 1; }

launch second >"$work/second.pid"
second_shell=$(cat "$work/second.pid")
new_side=""
for _ in $(seq 1 150); do
  new_side=$(jq -r '.pid' "$data/serve.lock" 2>/dev/null || true)
  [[ -n $new_side && $new_side != "$old_side" ]] && break
  sleep 0.1
done
[[ -n $new_side && $new_side != "$old_side" ]] || {
  echo "FAIL: the next shell did not reclaim the orphan" >&2
  cat "$work/shell-second.log" >&2
  exit 1
}
kill -0 "$new_side" 2>/dev/null || { echo "FAIL: the reclaimed sidecar is not alive" >&2; exit 1; }
[[ $(ps -o ppid= -p "$new_side" | tr -d ' ') == "$second_shell" ]] \
  || { echo "FAIL: the reclaimed sidecar is not owned by the new shell" >&2; exit 1; }
kill -0 "$old_side" 2>/dev/null && { echo "FAIL: the orphan sidecar survived takeover" >&2; exit 1; }

echo "desktop_orphan_probe: ok (SIGKILLed $killed_shell; reclaimed orphan as $new_side)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
