#!/usr/bin/env bash
# The real desktop shell must find its sidecar, start the service and expose
# the lock. This covers the probe/auto-start path that desktop_smoke.sh cannot.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

if [[ $(uname -s) != Darwin ]]; then
  echo "SKIP: the desktop probe test needs a macOS GUI session" >&2
  exit 0
fi

# dev.sh deliberately points PRINTKIT_SIDECAR_PATH at the cargo-built binary:
# the staged externalBin is only for Tauri packaging and can be SIGKILLed by
# macOS provenance when executed in place.
sidecar="$root/target/debug/printkit"
if [[ ! -x $sidecar ]]; then
  cargo build -p printkit
fi
desktop="$root/apps/desktop/src-tauri/target/debug/printkit-desktop"
if [[ ! -x $desktop ]]; then
  # Tauri's build script requires the staged externalBin even though this
  # probe overrides the runtime path with the cargo-built executable.
  bash apps/desktop/scripts/prepare-sidecar.sh debug
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
app_pid=
serve_pid=
cleanup() {
  if [[ -n $app_pid ]] && kill -0 "$app_pid" 2>/dev/null; then
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

PRINTKIT_DATA_DIR="$data" PRINTKIT_PORT="$port" \
PRINTKIT_SIDECAR_PATH="$sidecar" PRINTKIT_DESKTOP_REQUIRE_SIDECAR=1 \
  "$desktop" >"$work/desktop.log" 2>&1 &
app_pid=$!

for _ in $(seq 1 100); do
  if [[ -s $data/serve.lock ]]; then
    break
  fi
  sleep 0.1
done
[[ -s $data/serve.lock ]] || {
  echo "FAIL: the shell never started the sidecar" >&2
  cat "$work/desktop.log" >&2
  exit 1
}
lock_port=$(jq -r '.port' "$data/serve.lock")
serve_pid=$(jq -r '.pid' "$data/serve.lock")
[[ $lock_port == "$port" ]] || { echo "FAIL: lock port is $lock_port, expected $port" >&2; exit 1; }
kill -0 "$serve_pid" 2>/dev/null || { echo "FAIL: lock pid $serve_pid is not alive" >&2; exit 1; }
pgrep -f "serve --data-dir $data" >/dev/null \
  || { echo "FAIL: the sidecar is not running for the shell data dir" >&2; exit 1; }
curl -fsS -o /dev/null "http://127.0.0.1:$port/admin/session" \
  || { echo "FAIL: the started service is not reachable" >&2; exit 1; }

echo "desktop_sidecar_probe: ok (shell pid $app_pid started serve pid $serve_pid on $port)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
