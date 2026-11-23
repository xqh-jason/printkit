#!/usr/bin/env bash
# Live control-client contract: real salvo responses, chunked zip, CSV BOM.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

if [[ -n ${PRINTKIT_BIN:-} ]]; then
  bin=$PRINTKIT_BIN
else
  cargo build -p printkit
  bin=$root/target/debug/printkit
fi

data=$(mktemp -d)
# The control contract exercises observability endpoints, so opt in.
printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$data/plugins.toml"
port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
base="http://127.0.0.1:$port"
serve_pid=

cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$data"
}
trap cleanup EXIT

"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!

for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || {
  echo "FAIL: serve did not answer on $port" >&2
  exit 1
}

jar="$data/cookies.txt"
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || { echo "FAIL: /admin/session set no CSRF cookie" >&2; exit 1; }
admin=$(tr -d '\n' <"$data/admin-token")
status=$(curl -sS -o "$data/login.json" -w '%{http_code}' -b "$jar" -c "$jar" \
  -H "Origin: $base" -H 'Content-Type: application/json' \
  -H "X-CSRF-Token: $csrf" -X POST \
  --data "{\"token\":\"$admin\"}" "$base/admin/login")
[[ $status == 200 ]] || { echo "FAIL: admin login returned $status" >&2; exit 1; }

status=$(curl -sS -D "$data/events.headers" -o "$data/events.json" -w '%{http_code}' \
  -b "$jar" -H "Origin: $base" "$base/admin/logs/events?limit=5")
[[ $status == 200 ]] || { echo "FAIL: /admin/logs/events returned $status" >&2; exit 1; }
tr -d '\r' <"$data/events.headers" >"$data/events.headers.clean"
grep -qi '^content-length:' "$data/events.headers.clean" \
  || { echo "FAIL: /admin/logs/events has no Content-Length" >&2; exit 1; }
if grep -qi '^transfer-encoding:' "$data/events.headers.clean"; then
  echo "FAIL: /admin/logs/events is chunked, expected Content-Length JSON" >&2
  exit 1
fi
jq -e 'type == "array"' "$data/events.json" >/dev/null \
  || { echo "FAIL: /admin/logs/events is not a JSON array" >&2; exit 1; }
printf '/admin/logs/events: 200, Content-Length JSON (%s events)\n' "$(jq length "$data/events.json")"

control_log="$data/control-test.log"
if ! PRINTKIT_TEST_DATA="$data" PRINTKIT_TEST_PORT="$port" \
  cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml \
  control::tests::live_salvo -- --ignored --exact --nocapture >"$control_log" 2>&1; then
  cat "$control_log" >&2
  echo "FAIL: the live_salvo test binary exited non-zero" >&2
  exit 1
fi
grep -Eq 'test result: ok\. 1 passed; 0 failed' "$control_log" \
  || { cat "$control_log" >&2; echo "FAIL: expected exactly 1 passing live_salvo test (a zero-test run is not a pass)" >&2; exit 1; }
printf 'control live: 1 exact test passed\n'

printf 'PASS: control live\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, so without this the
# serve process and its data directory survive the script.
exit 0
