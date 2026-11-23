#!/usr/bin/env bash
# End-to-end batch benchmark: one `doc.printBatch` of N documents with M pages
# each, through the service and a real spooler queue.
#
# The measured interval starts when the request is sent and ends when the job
# event reports `done`, so it includes rendering, PDF merging and submission.
# The spooler job count is read from `cupsd`, not from the tool's own counters.
#
# Usage: bench-batch.sh [documents] [pages]
#   PRINTKIT_BENCH_PRINTER  queue to print to (default: the system default)
#   PRINTKIT_BENCH_BUDGET_S wall-clock budget in seconds (default: 45)
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

# A missing host tool or printer is a skip for a casual local run, but a real
# failure when the caller declares the benchmark required (release/CI).
skip_or_fail() {
  local reason="$1"
  if [[ ${PRINTKIT_BENCH_REQUIRED:-0} == 1 ]]; then
    printf 'FAIL: %s\n' "$reason" >&2
    exit 1
  fi
  printf 'SKIP: %s\n' "$reason" >&2
  exit 0
}

documents=${1:-100}
pages=${2:-2}
budget=${PRINTKIT_BENCH_BUDGET_S:-45}
bin=${PRINTKIT_BIN:-$root/target/release/printkit}

[[ $(uname -s) == Darwin ]] || skip_or_fail "this benchmark submits through CUPS on macOS"
printer=${PRINTKIT_BENCH_PRINTER:-}
if [[ -z $printer ]]; then
  printer=$(lpstat -d 2>/dev/null | sed -n 's/^system default destination: //p' || true)
fi
[[ -n $printer ]] || skip_or_fail "no printer; set PRINTKIT_BENCH_PRINTER"
lpstat -p "$printer" >/dev/null 2>&1 || skip_or_fail "CUPS does not know $printer"

if [[ ! -x $bin ]]; then
  cargo build -q --release -p printkit
fi

work=$(mktemp -d)
data="$work/data"
mkdir -p "$data"
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"
serve_pid=

cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

"$bin" --data-dir "$data" --port "$port" serve >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 200); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer on $port"

admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "the admin session did not set a CSRF cookie"

admin_post() {
  local path=$1 body=$2
  curl -sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" \
    -X POST --data "$body" "$base$path"
}

status=$(admin_post /admin/login "{\"token\":\"$admin\"}")
[[ $status == 200 ]] || fail "login returned $status"
status=$(admin_post /admin/origins '{"origin":"https://bench.example"}')
[[ $status == 200 ]] || fail "adding an origin returned $status"
status=$(admin_post /admin/tokens '{"origin":"https://bench.example","nonBrowser":true}')
[[ $status == 201 ]] || fail "issuing a print token returned $status"
token=$(jq -r '.token' "$data/body.json")

python3 - "$work" "$documents" "$pages" <<'PY'
import json
import sys

work = sys.argv[1]
documents = int(sys.argv[2])
pages = int(sys.argv[3])
page = {
    "spec": {"size": "A4"},
    "items": [
        {
            "id": "body",
            "type": "text",
            "frame": {"x": 20, "y": 20, "width": 170, "height": 12},
            "style": {"fontSize": 8, "lineHeight": 1.4, "color": "#000000"},
            "data": {"text": "printkit batch benchmark"},
        }
    ],
}
document = {"version": 1, "pages": []}
for page_index in range(pages):
    page_copy = json.loads(json.dumps(page))
    page_copy["items"][0]["id"] = f"body-{page_index}"
    document["pages"].append(page_copy)
json.dump([document for _ in range(documents)], open(f"{work}/batch.json", "w"))
PY

before=$(lpstat -W all -o "$printer" 2>/dev/null | awk 'NF { print $1 }' | sort)

export BENCH_TOKEN="$token" BENCH_PORT="$port" BENCH_PRINTER="$printer" \
  BENCH_WORK="$work" BENCH_DOCS="$documents" BENCH_PAGES="$pages" BENCH_BUDGET="$budget"

python3 - <<'PY'
import base64
import json
import os
import socket
import struct
import time

port = int(os.environ["BENCH_PORT"])
printer = os.environ["BENCH_PRINTER"]
work = os.environ["BENCH_WORK"]
documents = int(os.environ["BENCH_DOCS"])
pages = int(os.environ["BENCH_PAGES"])
budget = float(os.environ["BENCH_BUDGET"])
token = os.environ["BENCH_TOKEN"]


def connect():
    for _ in range(200):
        sock = socket.create_connection(("127.0.0.1", port), timeout=60)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        sock.sendall(request.encode())
        buffer = b""
        while b"\r\n\r\n" not in buffer:
            chunk = sock.recv(4096)
            if not chunk:
                raise SystemExit("the server closed during the upgrade")
            buffer += chunk
        head, rest = buffer.split(b"\r\n\r\n", 1)
        if b" 101 " in head.split(b"\r\n")[0]:
            return sock, rest
        sock.close()
        time.sleep(0.05)
    raise SystemExit("could not connect")


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


def send(sock, payload):
    data = json.dumps(payload).encode()
    header = bytearray([0x81])
    if len(data) < 126:
        header.append(0x80 | len(data))
    elif len(data) < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", len(data))
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", len(data))
    mask = os.urandom(4)
    header += mask
    body = bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))
    sock.sendall(bytes(header) + body)


def message(sock, buffer):
    while True:
        head, buffer = read_exact(sock, buffer, 2)
        opcode = head[0] & 0x0F
        length = head[1] & 0x7F
        if length == 126:
            extra, buffer = read_exact(sock, buffer, 2)
            length = struct.unpack("!H", extra)[0]
        elif length == 127:
            extra, buffer = read_exact(sock, buffer, 8)
            length = struct.unpack("!Q", extra)[0]
        mask = b""
        if head[1] & 0x80:
            mask, buffer = read_exact(sock, buffer, 4)
        payload, buffer = read_exact(sock, buffer, length)
        if mask:
            payload = bytes(
                byte ^ mask[index % 4] for index, byte in enumerate(payload)
            )
        if opcode == 1:
            return json.loads(payload), buffer
        if opcode == 8:
            raise SystemExit("the server closed the connection")


sock, buffer = connect()
send(sock, {"type": "hello", "protocol": 1, "sdk": "bench", "token": token})
welcome, buffer = message(sock, buffer)
if welcome.get("type") != "welcome":
    raise SystemExit(f"no welcome: {welcome}")

docs = json.load(open(f"{work}/batch.json"))
started = time.monotonic()
send(
    sock,
    {
        "id": "bench",
        "method": "doc.printBatch",
        "params": {"docs": docs, "printer": printer, "copies": 1, "collate": True},
    },
)
while True:
    reply, buffer = message(sock, buffer)
    if reply.get("id") == "bench":
        break
if not reply.get("ok"):
    raise SystemExit(f"doc.printBatch failed: {reply}")
job_id = reply["result"]["jobId"]
while True:
    event, buffer = message(sock, buffer)
    if event.get("type") == "event" and event.get("jobId") == job_id:
        if event.get("status") == "failed":
            raise SystemExit(f"job failed: {event}")
        if event.get("status") == "done":
            break
elapsed = time.monotonic() - started
if elapsed > budget:
    raise SystemExit(
        f"FAIL: {documents} x {pages} took {elapsed:.2f}s, budget {budget:.0f}s"
    )
print(
    f"doc.printBatch: {documents} documents x {pages} pages in {elapsed:.2f}s "
    f"(budget {budget:.0f}s), job {job_id}"
)
PY

after=$(lpstat -W all -o "$printer" 2>/dev/null | awk 'NF { print $1 }' | sort)
new_jobs=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep -c . || true)
[[ $new_jobs == 1 ]] || fail "the batch produced $new_jobs spooler jobs"
printf 'spooler jobs: 1\n'
printf 'PASS: batch %s x %s within %ss with one spooler job\n' \
  "$documents" "$pages" "$budget"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
