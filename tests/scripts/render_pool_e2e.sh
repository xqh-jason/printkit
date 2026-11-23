#!/usr/bin/env bash
# Render-pool acceptance: job events, queue bounds, queue positions, crash
# recovery and the mock print path. The service runs with `--features
# fake-render`, so no WebView and no printer are involved.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

# Build in a throwaway target so a fake-render binary never replaces the
# shared target/debug/printkit that later gates or packaging may run.
cargo_target=$(mktemp -d)
bin=${PRINTKIT_BIN:-$cargo_target/debug/printkit}
root_bin=$root/target/debug/printkit
root_stamp=
if [[ -e $root_bin ]]; then
  root_stamp=$(stat -f '%m %z' "$root_bin" 2>/dev/null || stat -c '%Y %s' "$root_bin")
fi
cleanup() {
  if [[ -n ${serve_pid:-} ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$serve_pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "${data:-}" "$cargo_target"
}
trap cleanup EXIT
# The fake backend owns printer discovery; it must not consult the host's
# CUPS default printer. Point CUPS at a closed port as a belt-and-braces guard.
export CUPS_SERVER=127.0.0.1:9
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  cargo build -q -p printkit --features fake-render --target-dir "$cargo_target"
fi
[[ -x $bin ]] || { echo "FAIL: no fake-render binary at $bin" >&2; exit 1; }

default_data=$(mktemp -d)
set +e
"$bin" logs --data-dir "$default_data" --json >"$default_data/logs.out" 2>"$default_data/logs.err"
default_logs_rc=$?
set -e
[[ $default_logs_rc == 4 ]] || { echo "FAIL: default core logs exited $default_logs_rc (want 4)" >&2; exit 1; }
grep -q E_UNSUPPORTED "$default_data/logs.err" \
  || { echo "FAIL: default core logs did not report E_UNSUPPORTED" >&2; exit 1; }
printf 'default core rejection: logs exit 4 with E_UNSUPPORTED\n'
rm -rf "$default_data"

data=$(mktemp -d)
# The queue trace assertion below uses the logs command; opt in observability
# for this temporary service only.
cat >"$data/plugins.toml" <<'PLUGINS'
version = 1
settled = 2
preset = "core"
[[plugins]]
id = "observability"
enabled = true
PLUGINS
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"
serve_pid=

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

"$bin" --data-dir "$data" --port "$port" serve >"$data/serve.log" 2>&1 &
serve_pid=$!

for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer on $port"

# --- admin bootstrap -------------------------------------------------------

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
status=$(admin_post /admin/origins '{"origin":"https://pool.example"}')
[[ $status == 200 ]] || fail "adding an origin returned $status"
status=$(admin_post /admin/tokens '{"origin":"https://pool.example","nonBrowser":true}')
[[ $status == 201 ]] || fail "issuing a token returned $status"
token=$(jq -r '.token' "$data/body.json")

python3 - "$port" "$token" <<'PY'
import base64
import json
import os
import socket
import struct
import sys
import threading
import time

port = int(sys.argv[1])
token = sys.argv[2]


def connect():
    last = None
    for _ in range(100):
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
        status = head.split(b"\r\n")[0]
        if b" 101 " in status:
            return sock, rest
        sock.close()
        if b" 429 " in status:
            last = "too many connections"
            time.sleep(0.05)
            continue
        raise SystemExit("upgrade failed: " + status.decode("utf-8", "replace"))
    raise SystemExit("could not connect: " + str(last))


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self):
        self.sock, self.buffer = connect()
        self.next_id = 1
        self.events = []
        self.send({"type": "hello", "protocol": 1, "sdk": "pool-e2e", "token": token})
        message = self.message()
        if message.get("type") != "welcome":
            raise SystemExit(f"no welcome: {message}")

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
                    self.events.append((time.monotonic(), message))
                return message
            if opcode == 8:
                raise SystemExit("the server closed the connection")

    def call(self, method, params=None):
        request_id = f"req-{self.next_id}"
        self.next_id += 1
        message = {"id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)
        while True:
            reply = self.message()
            if reply.get("id") == request_id:
                return reply

    def wait_done(self, job_ids, timeout=60):
        wanted = set(job_ids)
        done = {}
        deadline = time.monotonic() + timeout
        while len(done) < len(wanted):
            for _, event in self.events:
                if event.get("jobId") in wanted and event.get("status") in ("done", "failed"):
                    done[event["jobId"]] = event
            if len(done) == len(wanted):
                return done
            if time.monotonic() > deadline:
                raise SystemExit(f"jobs did not finish: {sorted(wanted - set(done))}")
            self.message()
        return done

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def doc_text(text, delay_ms=None):
    body = text if delay_ms is None else f"{text} fake-render-delay:{delay_ms}"
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
                        "data": {"text": body},
                    }
                ],
            }
        ],
    }


# --- capabilities ----------------------------------------------------------

session = Session()
info = session.call("sys.info")
assert info["ok"], info
capabilities = info["result"]["capabilities"]
assert len(capabilities) == 10, capabilities
assert capabilities["renderWorkers"] == 1, capabilities
assert info["result"]["platform"] == "macos", info["result"]
print("sys.info: renderWorkers=1, capabilities=10 keys")
session.close()

# --- eight concurrent clients, one renderer --------------------------------

timeline = []
failures = []


def client(index):
    try:
        own = Session()
        reply = own.call("doc.print", doc_text(f"client {index}", 200))
        if not reply["ok"]:
            raise SystemExit(f"doc.print failed: {reply}")
        job_id = reply["result"]["jobId"]
        statuses = []
        started = None
        while True:
            message = own.message()
            if message.get("type") != "event" or message.get("jobId") != job_id:
                continue
            statuses.append(message["status"])
            if message["status"] == "printing":
                started = time.monotonic()
            elif message["status"] == "done":
                timeline.append((started, time.monotonic()))
                break
            elif message["status"] == "failed":
                raise SystemExit(f"job failed: {message}")
        if statuses != ["queued", "printing", "done"]:
            raise SystemExit(f"unexpected lifecycle: {statuses}")
        own.close()
    except BaseException as error:  # noqa: BLE001 - collected for the main thread
        failures.append(repr(error))


threads = [threading.Thread(target=client, args=(index,)) for index in range(8)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
if failures:
    raise SystemExit("concurrent clients failed: " + "; ".join(failures))
assert len(timeline) == 8, timeline
ordered = sorted(timeline)
for previous, current in zip(ordered, ordered[1:]):
    overlap = previous[1] - current[0]
    assert overlap <= 0.02, f"renders overlapped by {overlap:.3f}s"
print("8 clients: all done, queued->printing->done, renders serialised")

# --- queue bound: 32 in flight, the 33rd is refused ------------------------

session = Session()
holder = session.call("doc.print", doc_text("holder", 5000))
assert holder["ok"], holder
accepted = [holder["result"]["jobId"]]
for index in range(31):
    reply = session.call("doc.print", doc_text(f"queued {index}"))
    assert reply["ok"], reply
    accepted.append(reply["result"]["jobId"])
overflow = session.call("doc.print", doc_text("overflow"))
assert not overflow["ok"], overflow
assert overflow["error"]["code"] == "E_TIMEOUT", overflow
assert overflow["error"]["detail"]["queueMax"] == 32, overflow
assert overflow["error"]["detail"]["queuePos"] == 33, overflow
print("queue bound: 32 accepted, request 33 -> E_TIMEOUT queueMax=32 queuePos=33")
finished = session.wait_done(accepted)
assert all(event["status"] == "done" for event in finished.values()), finished
print("queue bound: all 32 accepted jobs done")
session.close()

# --- queue positions shrink while jobs wait --------------------------------

session = Session()
holder = session.call("doc.print", doc_text("position-holder", 3000))
assert holder["ok"], holder
ids = [holder["result"]["jobId"]]
for index in range(5):
    reply = session.call("doc.print", doc_text(f"waiting {index}", 250))
    assert reply["ok"], reply
    ids.append(reply["result"]["jobId"])
last = ids[-1]
positions = []
deadline = time.monotonic() + 15
while time.monotonic() < deadline:
    status = session.call("job.status", {"jobId": last})
    assert status["ok"], status
    body = status["result"]
    if body.get("queuePos") is not None:
        positions.append(body["queuePos"])
    if body["status"] in ("printing", "done"):
        break
    time.sleep(0.05)
assert positions, "the job never reported a queue position"
assert positions[0] > 1, positions
assert all(later <= earlier for earlier, later in zip(positions, positions[1:])), positions
finished = session.wait_done(ids)
assert all(event["status"] == "done" for event in finished.values()), finished
print(f"queue positions: {positions} (monotonic, reached {positions[-1]})")
session.close()

# --- crash recovery: one retry, then E_RENDER_CRASH ------------------------

session = Session()
listed = session.call("printers.list")
assert listed["ok"], listed
printers = listed["result"]
assert [printer["name"] for printer in printers] == ["fake-printer"], printers
assert printers[0]["isDefault"] is True, printers
print("printer list: only the fake backend printer (host default never consulted)")
session.close()

session = Session()
ids = []
for text, delay in (("healthy-before", 100), ("fake-render-crash", None), ("healthy-after", 100)):
    reply = session.call("doc.print", doc_text(text, delay))
    assert reply["ok"], reply
    ids.append(reply["result"]["jobId"])
finished = session.wait_done(ids)
assert finished[ids[1]]["status"] == "done", finished[ids[1]]
assert all(event["status"] == "done" for event in finished.values()), finished
print("crash recovery: the crashed job was retried once and completed")

always = session.call("doc.print", doc_text("fake-render-crash-always"))
assert always["ok"], always
always_id = always["result"]["jobId"]
finished = session.wait_done([always_id])
failed = finished[always_id]
assert failed["status"] == "failed", failed
assert failed["error"]["code"] == "E_RENDER_CRASH", failed
after = session.call("doc.print", doc_text("after-the-crash"))
assert after["ok"], after
finished = session.wait_done([after["result"]["jobId"]])
assert finished[after["result"]["jobId"]]["status"] == "done", finished
print("crash recovery: two crashes -> E_RENDER_CRASH, then the pool keeps serving")
session.close()

# --- mock export through the fake renderer ---------------------------------

session = Session()
exported = session.call(
    "doc.exportPdf",
    {"document": doc_text("pool export"), "path": "pool/out.pdf"},
)
assert exported["ok"], exported
assert exported["result"]["path"] == "exports/pool/out.pdf", exported
session.close()
print(f"export: {exported['result']['path']} ({exported['result']['bytes']} bytes)")
PY

pdf="$data/exports/pool/out.pdf"
[[ -f $pdf ]] || fail "the mock export did not write $pdf"
head -c 5 "$pdf" | grep -q '%PDF-' || fail "the exported file is not a PDF"
printf 'exported file: %s (%s bytes)\n' "$pdf" "$(stat -f '%z' "$pdf")"

if ! kill -0 "$serve_pid" 2>/dev/null; then
  fail "the serve process died during the run"
fi
printf 'serve process still alive: pid %s\n' "$serve_pid"

"$bin" --data-dir "$data" logs --stage queue --json >"$data/queue.json"
jq -e 'length > 0 and (map(select(.queue_pos != null)) | length > 0)' "$data/queue.json" >/dev/null \
  || fail "no queue trace event carried queue_pos"
printf 'queue trace events: %s (with queue_pos)\n' "$(jq 'length' "$data/queue.json")"


# The stamp assertion only proves something when this run used its own
# --target-dir. A caller-provided PRINTKIT_BIN may have come from any build,
# and an unrelated cargo/rustc writing the shared target would legitimately
# change the stamp; in both cases report SKIP instead of a false PASS/FAIL.
concurrent_build_running() {
  # Deterministic hooks for both sides of the acceptance test itself.
  case ${PRINTKIT_TEST_FORCE_CONCURRENT_BUILD:-} in
    1) return 0 ;;
    0) return 1 ;;
  esac
  command -v pgrep >/dev/null 2>&1 || return 1
  pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1
}
stamp_checked=0
stamp_skipped=0
cups_skipped=0
if [[ -n ${PRINTKIT_BIN:-} ]]; then
  printf 'SKIP: shared target/debug/printkit stamp (PRINTKIT_BIN provided; no isolated build)\n'
  stamp_skipped=1
elif [[ $bin != "$cargo_target/debug/printkit" ]]; then
  printf 'SKIP: shared target/debug/printkit stamp (binary is not the isolated build)\n'
  stamp_skipped=1
elif concurrent_build_running; then
  printf 'SKIP: shared target/debug/printkit stamp assertion (concurrent cargo/rustc running; before %s)\n' \
    "${root_stamp:-absent}"
  stamp_skipped=1
else
  if [[ -e $root_bin ]]; then
    after=$(stat -f '%m %z' "$root_bin" 2>/dev/null || stat -c '%Y %s' "$root_bin")
    [[ $after == "$root_stamp" ]] \
      || fail "the fake-render build replaced shared target/debug/printkit"
  else
    [[ ! -e $root_bin ]] || fail "the fake-render build left shared target/debug/printkit"
  fi
  printf 'shared target/debug/printkit stamp: unchanged (%s)\n' "${root_stamp:-absent}"
  stamp_checked=1
fi
[[ $stamp_checked == 1 ]] || printf 'SKIP (stamp not asserted)\n'

# The fake backend owns printing, so the host spooler must stay unreachable:
# CUPS is pointed at a closed port, and no real queue/job can be consulted.
if command -v lpstat >/dev/null 2>&1; then
  if lpstat -p >/dev/null 2>&1 || lpstat -o >/dev/null 2>&1; then
    fail "CUPS answered although CUPS_SERVER points at a closed port"
  fi
  printf 'CUPS closed: lpstat cannot reach the spooler (no real queue consulted)\n'
else
  printf 'SKIP: CUPS closed check (lpstat is not installed)\n'
  cups_skipped=1
fi
if ((stamp_skipped == 1)); then
  printf 'render pool e2e: PASS-WITH-SKIP (stamp not asserted)\n'
elif ((cups_skipped == 1)); then
  printf 'render pool e2e: PASS-WITH-SKIP (CUPS closed check not asserted)\n'
else
  printf 'render pool e2e: OK\n'
fi

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and this script
# keeps its serve process alive until the last check, so the trap is the only
# thing that stops it and removes the temporary target/data directories.
exit 0
