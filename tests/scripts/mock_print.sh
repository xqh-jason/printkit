#!/usr/bin/env bash
# Mock print acceptance: run N prints through the fake renderer, then read the
# usage statistics over the WebSocket. No printer and no WebView are involved.
#
# Usage: mock_print.sh [count]
#   PRINTKIT_DATA_DIR    use this database directory (default: a temporary directory)
#   PRINTKIT_KEEP_DATA=1 keep the data directory for a later trace_subscribe run;
#                        without it the directory (and its database) is removed
#   PRINTKIT_BIN         binary to exercise (default: the isolated fake target)
#   PRINTKIT_FAKE_TARGET_DIR
#                        CARGO_TARGET_DIR for the fake-render build
#                        (default: target/fake-render)
#   MOCK_SECOND_ORIGIN   also print the last document from this origin, so the
#                        origin filter of `records.*` can be checked
#   MOCK_EXPECT_RECORDS  expected usage rows (default: the print count); use 0
#                        for `--log off`
# The data directory is printed at the end so the caller can query the same
# database with sqlite3 and `printkit stats` before cleanup. The default is to
# remove it; pass PRINTKIT_KEEP_DATA=1 to keep it for trace_subscribe.sh.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

count=${1:-20}
shift || true
target_dir=${PRINTKIT_FAKE_TARGET_DIR:-$root/target/fake-render}
bin=${PRINTKIT_BIN:-$target_dir/debug/printkit}
data=${PRINTKIT_DATA_DIR:-$(mktemp -d)}
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
  if [[ ${PRINTKIT_KEEP_DATA:-0} != 1 ]]; then
    rm -rf "$data"
  fi
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

mkdir -p "$data"
# Build into a separate target directory: the fake-render binary must never
# replace target/debug/printkit, which the CUPS acceptance scripts use.
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  CARGO_TARGET_DIR="$target_dir" cargo build -q -p printkit --features fake-render
fi

# Default core must still reject records/logs; this is not skipped by the
# opt-in below.
default_data=$(mktemp -d)
set +e
"$bin" stats --data-dir "$default_data" --json >"$default_data/stats.out" 2>"$default_data/stats.err"
default_stats_rc=$?
"$bin" logs --data-dir "$default_data" --json >"$default_data/logs.out" 2>"$default_data/logs.err"
default_logs_rc=$?
set -e
[[ $default_stats_rc == 4 ]] || fail "default core stats exited $default_stats_rc (want 4)"
[[ $default_logs_rc == 4 ]] || fail "default core logs exited $default_logs_rc (want 4)"
grep -q E_UNSUPPORTED "$default_data/stats.err" \
  || fail "default core stats did not report E_UNSUPPORTED"
grep -q E_UNSUPPORTED "$default_data/logs.err" \
  || fail "default core logs did not report E_UNSUPPORTED"
printf 'default core rejection: stats/logs exit 4 with E_UNSUPPORTED\n'
rm -rf "$default_data"

# This e2e reads records.stats and the CLI logs; opt in the plugins in this
# temporary data directory without changing the product default. A persistent
# PRINTKIT_DATA_DIR with an existing plugins.toml must not be silently rewritten.
if [[ -e "$data/plugins.toml" ]]; then
  fail "refusing to overwrite existing $data/plugins.toml; use a fresh temporary data dir"
fi
cat >"$data/plugins.toml" <<'PLUGINS'
version = 1
settled = 2
preset = "core"
[[plugins]]
id = "records"
enabled = true
[[plugins]]
id = "observability"
enabled = true
PLUGINS

"$bin" --data-dir "$data" --port "$port" "$@" serve >"$data/serve.log" 2>&1 &
serve_pid=$!

for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer on $port"

# Origin CLI: list/add/remove through the live service, never sending the
# administrator token to a process that does not own serve.lock.
"$bin" --data-dir "$data" --port "$port" origin add https://cli.example --json >"$data/origin-add.json"
jq -e '.origins | index("https://cli.example") != null' "$data/origin-add.json" >/dev/null \
  || fail "origin add did not return the new origin"
"$bin" --data-dir "$data" --port "$port" origin remove https://cli.example --json >"$data/origin-remove.json"
jq -e '.origins | index("https://cli.example") == null' "$data/origin-remove.json" >/dev/null \
  || fail "origin remove did not return the remaining list"
printf 'origin CLI add/remove: ok\n'

# Existing PDFs print without a renderer, through --pdf and by .pdf name.
# Both assertions read the real page count, copies and sheets; `pages >= 1`
# would pass even if a 3-page input silently became one page.
one_page="$data/one-page.pdf"
"$bin" --data-dir "$data" pdf -i tests/fixtures/minimal.json -o "$one_page"
[[ -s $one_page ]] || fail "pdf produced no one-page fixture"
"$bin" --data-dir "$data" --port "$port" print --pdf "$one_page" \
  -p fake-printer -n 2 --no-record --json >"$data/print-pdf.json"
jq -e '.pages == 1 and .copies == 2 and .sheets == 2
  and .printer == "fake-printer" and (.jobId | type == "number")
  and (.warnings | length == 0)' "$data/print-pdf.json" >/dev/null \
  || fail "print --pdf of a one-page fixture did not report 1 page x 2 copies"
"$bin" --data-dir "$data" --port "$port" print -i tests/baseline/table-multipage.pdf \
  -p fake-printer -n 2 --no-record --json >"$data/print-pdf-input.json"
jq -e '.pages == 3 and .copies == 2 and .sheets == 6
  and .printer == "fake-printer" and (.jobId | type == "number")
  and (.warnings | length == 0)' "$data/print-pdf-input.json" >/dev/null \
  || fail "print -i .pdf of the three-page fixture did not report 3 pages x 2 copies"
printf 'CLI PDF pages/copies/sheets: ok\n'

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
status=$(admin_post /admin/origins '{"origin":"https://records.example"}')
[[ $status == 200 ]] || fail "adding an origin returned $status"

status=$(admin_post /admin/tokens '{"origin":"https://records.example","nonBrowser":true}')
[[ $status == 201 ]] || fail "issuing a print token returned $status"
print_token=$(jq -r '.token' "$data/body.json")
printf '%s\n' "$print_token" >"$data/print-token"

status=$(admin_post /admin/tokens '{"origin":"https://records.example","admin":true,"nonBrowser":true}')
[[ $status == 201 ]] || fail "issuing an admin token returned $status"
admin_token=$(jq -r '.token' "$data/body.json")
printf '%s\n' "$admin_token" >"$data/admin-ws-token"

second_token=
if [[ -n ${MOCK_SECOND_ORIGIN:-} ]]; then
  status=$(admin_post /admin/origins "{\"origin\":\"$MOCK_SECOND_ORIGIN\"}")
  [[ $status == 200 ]] || fail "adding the second origin returned $status"
  status=$(admin_post /admin/tokens "{\"origin\":\"$MOCK_SECOND_ORIGIN\",\"nonBrowser\":true}")
  [[ $status == 201 ]] || fail "issuing the second print token returned $status"
  second_token=$(jq -r '.token' "$data/body.json")
  printf '%s\n' "$second_token" >"$data/print-token-2"
fi

expected_records=${MOCK_EXPECT_RECORDS:-$count}
python3 - "$port" "$print_token" "$admin_token" "$count" "${second_token:-}" "$expected_records" <<'PY' | tee "$data/summary.json"
import base64
import json
import os
import socket
import struct
import sys
import time

port = int(sys.argv[1])
print_token = sys.argv[2]
admin_token = sys.argv[3]
count = int(sys.argv[4])
second_token = sys.argv[5]
expected_records = int(sys.argv[6])


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self, token):
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
        head, buffer = buffer.split(b"\r\n\r\n", 1)
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise SystemExit("upgrade failed: " + head.decode("utf-8", "replace"))
        self.sock = sock
        self.buffer = buffer
        self.next_id = 1
        self.events = []
        self.send({"type": "hello", "protocol": 1, "sdk": "mock-print", "token": token})
        welcome = self.message()
        if welcome.get("type") != "welcome":
            raise SystemExit(f"handshake refused: {welcome}")

    def send(self, payload):
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
        self.sock.sendall(bytes(header) + body)

    def message(self):
        while True:
            head, self.buffer = read_exact(self.sock, self.buffer, 2)
            opcode = head[0] & 0x0F
            masked = head[1] & 0x80
            length = head[1] & 0x7F
            if length == 126:
                extra, self.buffer = read_exact(self.sock, self.buffer, 2)
                length = struct.unpack("!H", extra)[0]
            elif length == 127:
                extra, self.buffer = read_exact(self.sock, self.buffer, 8)
                length = struct.unpack("!Q", extra)[0]
            mask = b""
            if masked:
                mask, self.buffer = read_exact(self.sock, self.buffer, 4)
            payload, self.buffer = read_exact(self.sock, self.buffer, length)
            if masked:
                payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
            if opcode == 1:
                message = json.loads(payload)
                if message.get("type") == "event":
                    self.events.append(message)
                return message
            if opcode == 8:
                raise SystemExit("the server closed the connection")

    def call(self, method, params=None, timeout=60):
        request_id = f"req-{self.next_id}"
        self.next_id += 1
        message = {"id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)
        deadline = time.monotonic() + timeout
        while True:
            if time.monotonic() > deadline:
                raise SystemExit(f"{method} timed out")
            reply = self.message()
            if reply.get("id") == request_id:
                return reply

    def wait_done(self, job_id, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for event in self.events:
                if event.get("jobId") == job_id and event.get("status") in ("done", "failed"):
                    return event
            self.message()
        raise SystemExit(f"job {job_id} did not finish")

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def document(index):
    return {
        "version": 1,
        "pages": [
            {
                "spec": {"size": "A4"},
                "items": [
                    {
                        "id": "body",
                        "type": "text",
                        "frame": {"x": 20, "y": 20, "width": 160, "height": 12},
                        "style": {
                            "fontSize": 8,
                            "lineHeight": 1.4,
                            "color": "#000000",
                            "align": "left",
                        },
                        "data": {"text": f"mock record {index}"},
                    }
                ],
            }
        ],
        "output": {
            "copies": 1 + (index % 2),
            "collate": True,
            "printer": {"name": f"mock-{index % 2}"},
        },
        "meta": {
            "templateId": f"tpl-{index % 4}",
            "tags": [f"tag-{index}", f"group-{index % 3}"],
        },
    }


printer = Session(print_token)
second = Session(second_token) if second_token else None
for index in range(count):
    target = second if second is not None and index == count - 1 else printer
    reply = target.call("doc.print", document(index))
    if not reply.get("ok"):
        raise SystemExit(f"doc.print failed: {reply}")
    event = target.wait_done(reply["result"]["jobId"])
    if event.get("status") != "done":
        raise SystemExit(f"job failed: {event}")
printer.close()
if second is not None:
    second.close()

admin = Session(admin_token)
stats = admin.call("records.stats", {"groupBy": "tag"})
if not stats.get("ok"):
    raise SystemExit(f"records.stats failed: {stats}")

# The usage row is written before the job reports done, but allow the trim and
# the SQLite write to settle if the pool is still finishing its hook.
deadline = time.monotonic() + 10
while True:
    listing = admin.call("records.list", {"limit": 500})
    if not listing.get("ok"):
        raise SystemExit(f"records.list failed: {listing}")
    if len(listing["result"]["records"]) >= expected_records or time.monotonic() > deadline:
        break
    time.sleep(0.05)
admin.close()

if len(listing["result"]["records"]) != expected_records:
    raise SystemExit(
        f"{len(listing['result']['records'])} records were written, expected {expected_records}"
    )

print(json.dumps({"stats": stats["result"], "records": listing["result"]}, indent=2))
PY

printf 'data_dir=%s\n' "$data"
printf 'db=%s\n' "$data/printkit.db"
printf 'mock print: %s document(s) printed and recorded\n' "$count"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
