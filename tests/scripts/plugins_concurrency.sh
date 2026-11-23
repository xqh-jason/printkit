#!/usr/bin/env bash
# Plugin concurrency: duplicate requestId still returns one job, and CLI
# forwarding never writes plugins.toml when the service is unreachable.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"
cups_test_lock_acquire \
  || { printf 'FAIL: could not acquire the CUPS test lock within %ss\n' "${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-180}" >&2; exit 1; }
trap 'cups_test_lock_release' EXIT
bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  cargo build -q -p printkit --features serve,fake-render
fi

data=$(mktemp -d)
port=$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
)
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"
serve_pid=
cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$data"
  cups_test_lock_release
}
trap cleanup EXIT
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$1"; }

"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer"
admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
post() {
  local path=$1 body=$2
  curl -sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" -d "$body" "$base$path"
}
[[ $(post /admin/login "{\"token\":\"$admin\"}") == 200 ]] || fail "admin login failed"
[[ $(post /admin/origins '{"origin":"https://concurrency.test"}') == 200 ]] || fail "origin add failed"
[[ $(post /admin/tokens '{"origin":"https://concurrency.test","admin":true,"nonBrowser":true}') == 201 ]] \
  || fail "token issue failed"
token=$(jq -r .token "$data/body.json")

# Duplicate requestId from two concurrent connections.
jq '. + {requestId:"dup-request"}' tests/fixtures/minimal.json >"$data/params.json"
python3 tests/scripts/plugins_client.py "$port" "$token" doc.print "$(cat "$data/params.json")" >"$data/one.json" &
pid1=$!
python3 tests/scripts/plugins_client.py "$port" "$token" doc.print "$(cat "$data/params.json")" >"$data/two.json" &
pid2=$!
wait "$pid1"
wait "$pid2"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/one.json" >/dev/null \
  || fail "first doc.print failed"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/two.json" >/dev/null \
  || fail "second doc.print failed"
job1=$(jq -r .response.result.jobId "$data/one.json")
job2=$(jq -r .response.result.jobId "$data/two.json")
[[ $job1 == "$job2" ]] || fail "duplicate requestId made two jobs: $job1 vs $job2"
ok "concurrent duplicate requestId returns one job id"

python3 - "$port" "$token" "$root/data-http-idem.json" <<'PY'
import json, sys, urllib.request, urllib.error
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

port, token, _out = int(sys.argv[1]), sys.argv[2], sys.argv[3]
boundary = "IDEMBOUNDARY"
document = json.loads(Path("tests/fixtures/minimal.json").read_text())

def part(content_type, payload):
    return f"--{boundary}\r\nContent-Type: {content_type}\r\n\r\n".encode() + payload + b"\r\n"

body = (
    part("application/json", json.dumps({"printer": "fake-printer", "copies": 1}).encode())
    + part("application/json", json.dumps(document).encode())
    + f"--{boundary}--\r\n".encode()
)

def post():
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/print/batch",
        data=body,
        method="POST",
        headers={
            "Content-Type": f"multipart/mixed; boundary={boundary}",
            "Authorization": f"Bearer {token}",
            "Idempotency-Key": "http-duplicate",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)["jobId"]

with ThreadPoolExecutor(max_workers=2) as pool:
    ids = list(pool.map(lambda _: post(), range(2)))
if len(set(ids)) != 1:
    raise SystemExit(f"duplicate HTTP Idempotency-Key made two jobs: {ids}")
PY
ok "concurrent duplicate HTTP Idempotency-Key returns one job id"

# A dropped response must not be remembered: the retry has to print again.
[[ $(post /admin/plugins '{"id":"records","on":true}') == 200 ]] \
  || fail "could not enable records for the dropped-response test"
python3 - "$port" "$token" <<'PY'
import base64, json, os, socket, struct, sys

port, token = int(sys.argv[1]), sys.argv[2]
document = json.load(open("tests/fixtures/minimal.json"))
document["requestId"] = "drop-retry"

sock = socket.create_connection(("127.0.0.1", port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
sock.sendall(
    (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    ).encode()
)
buffer = b""
while b"\r\n\r\n" not in buffer:
    buffer += sock.recv(4096)

def send(payload):
    data = json.dumps(payload).encode()
    header = bytearray([0x81])
    if len(data) < 126:
        header.append(0x80 | len(data))
    else:
        header.append(0x80 | 126)
        header += struct.pack("!H", len(data))
    mask = os.urandom(4)
    sock.sendall(bytes(header) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

send({"type": "hello", "protocol": 1, "sdk": "drop-retry", "token": token})
send({"id": "drop", "method": "doc.print", "params": document})
# RST instead of a graceful FIN: the server write must fail before start().
sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
sock.close()
PY
before=$(python3 - "$data/printkit.db" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("select count(*) from print_record").fetchone()[0])
except sqlite3.Error:
    print(0)
PY
)
sleep 0.5
jq '. + {requestId:"drop-retry"}' tests/fixtures/minimal.json >"$data/drop-retry-params.json"
python3 tests/scripts/plugins_client.py "$port" "$token" doc.print "$(cat "$data/drop-retry-params.json")" >"$data/drop-retry.json"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/drop-retry.json" >/dev/null \
  || fail "the retry after a dropped response failed"
for _ in $(seq 1 50); do
  after=$(python3 - "$data/printkit.db" <<'PY'
import sqlite3, sys
try:
    print(sqlite3.connect(sys.argv[1]).execute("select count(*) from print_record").fetchone()[0])
except sqlite3.Error:
    print(0)
PY
)
  [[ $after -gt $before ]] && break
  sleep 0.1
done
[[ ${after:-$before} -gt $before ]] || fail "the retry reused a dropped request instead of printing"
ok "a dropped response is not cached; the same requestId retries and prints"

# CLI forwarding while the service is running.
[[ $($bin plugin disable records --confirm-data-count --data-dir "$data" --port "$port" --json 2>/dev/null | jq -r '.plugins[] | select(.id == "records") | .state') == off ]] \
  || fail "CLI forwarding did not disable records"
ok "CLI forwards changes through the running admin API"

# Service stopped: the CLI must fail without rewriting plugins.toml.
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=
before=$(shasum -a 256 "$data/plugins.toml" | awk '{print $1}')
set +e
"$bin" plugin enable records --data-dir "$data" --port "$port" --json >"$data/enable.out" 2>"$data/enable.err"
rc=$?
set -e
[[ $rc == 3 ]] || fail "offline plugin enable exited $rc (want 3)"
grep -q E_CONNECT "$data/enable.err" || fail "offline enable did not report E_CONNECT"
[[ $(shasum -a 256 "$data/plugins.toml" | awk '{print $1}') == "$before" ]] \
  || fail "offline CLI rewrote plugins.toml"
ok "offline CLI fails with E_CONNECT and leaves plugins.toml untouched"

# Broken file: service starts core-only and refuses writes.
printf 'version = 9\n' >"$data/plugins.toml"
printf 'version = 1\npreset = "core"\n' >"$data/plugins.toml.bak"
"$bin" serve --data-dir "$data" --port "$port" >"$data/broken.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ $(post /admin/login "{\"token\":\"$admin\"}") == 200 ]] || fail "broken admin login failed"
curl -fsS -b "$jar" "$base/admin/plugins" >"$data/broken-plugins.json"
jq -e '.broken != null and ([.plugins[] | select(.state == "on")] | length == 0)' \
  "$data/broken-plugins.json" >/dev/null \
  || fail "valid backup did not resolve to core-only with a broken reason"
"$bin" plugin list --data-dir "$data" --port "$port" --json >"$data/cli-list.json"
jq -e '.broken != null and ([.plugins[] | select(.state == "on")] | length == 0)' \
  "$data/cli-list.json" >/dev/null || fail "CLI list disagrees with the service on a broken file"
status=$(post /admin/plugins '{"id":"observability","on":true}')
[[ $status == 409 ]] || fail "write against a broken file returned $status"
jq -e '.error.code == "E_CONFIG" and .error.detail.reason == "broken"' "$data/body.json" >/dev/null \
  || fail "broken write did not report E_CONFIG/broken"
ok "broken file starts core-only and refuses writes"

echo "plugins_concurrency: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
