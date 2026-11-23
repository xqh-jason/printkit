#!/usr/bin/env bash
# macOS preview-window acceptance: a real WKWebView window, page PNG export,
# window lifecycle, the 8-window limit and the single-instance CLI path.
#
# The snapshot comes from the window's own web view, so it is also the proof
# that the exported image and the printed document share one layout. The screen
# may be locked while this runs; the window is still created and its web view
# still answers `preview.pages`.
#
# Usage: preview_e2e.sh
#   PRINTKIT_BIN  binary to exercise (default: target/debug/printkit)
# When PRINTKIT_GUI_E2E=true (the CI/release gate), a missing GUI session or
# tool fails instead of silently skipping. Local manual runs may still SKIP.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

required=${PRINTKIT_GUI_E2E:-}
[[ $(uname -s) == Darwin ]] || {
  if [[ $required == true ]]; then
    printf 'FAIL: PRINTKIT_GUI_E2E=true requires macOS\n' >&2
    exit 1
  fi
  printf 'SKIP: preview windows are macOS-only in this build\n' >&2
  exit 0
}
for tool in curl python3 sips pdftoppm jq; do
  command -v "$tool" >/dev/null || {
    if [[ $required == true ]]; then
      printf 'FAIL: PRINTKIT_GUI_E2E=true requires %s\n' "$tool" >&2
      exit 1
    fi
    printf 'SKIP: %s is missing\n' "$tool" >&2
    exit 0
  }
done

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
cargo build -q -p printkit

work=$(mktemp -d)
data=$work/data
mkdir -p "$data"
free_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}
port=$(free_port)
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"
serve_pid=
own_pid=
lock_pid=

cleanup() {
  for pid in "$lock_pid" "$own_pid" "$serve_pid"; do
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

# Default core must still refuse observability; this run then opts in through
# a temporary plugins.toml, exactly like the other e2e scripts.
set +e
"$bin" --data-dir "$data" logs --json >"$work/core-logs.out" 2>"$work/core-logs.err"
core_logs_rc=$?
set -e
[[ $core_logs_rc == 4 ]] || fail "default core logs exited $core_logs_rc (want 4)"
grep -q E_UNSUPPORTED "$work/core-logs.err" \
  || fail "default core logs did not report E_UNSUPPORTED"
if [[ -e "$data/plugins.toml" ]]; then
  fail "refusing to overwrite an existing plugins.toml"
fi
cat >"$data/plugins.toml" <<'PLUGINS'
version = 1
settled = 2
preset = "core"
[[plugins]]
id = "observability"
enabled = true
PLUGINS

"$bin" --data-dir "$data" --port "$port" serve >"$work/serve.log" 2>&1 &
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
  local path=$1 body=$2 status
  status=$(curl -sS -o "$work/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" \
    -X POST --data "$body" "$base$path")
  printf '%s' "$status"
}

[[ $(admin_post /admin/login "{\"token\":\"$admin\"}") == 200 ]] || fail "admin login failed"
[[ $(admin_post /admin/origins '{"origin":"https://preview.example"}') == 200 ]] \
  || fail "adding the origin failed"
[[ $(admin_post /admin/tokens '{"origin":"https://preview.example","nonBrowser":true}') == 201 ]] \
  || fail "issuing the script token failed"
token=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$work/body.json")
[[ -n $token ]] || fail "the service returned no token"

# --- WebSocket lifecycle: open, export, close, E_NOT_FOUND -----------------

PK_TOKEN=$token PK_BASE=$base \
PK_DOC=$root/tests/fixtures/minimal.json PK_PNG=$work/page.png \
python3 - <<'PY'
import base64, json, os, socket, struct, time

base = os.environ["PK_BASE"]
token = os.environ["PK_TOKEN"]
host, port = base.removeprefix("http://").split(":")
port = int(port)

def connect():
    sock = socket.create_connection((host, port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall(
        (
            f"GET /print HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = sock.recv(4096)
        if not chunk:
            raise SystemExit("the connection closed during the upgrade")
        buffer += chunk
    head, buffer = buffer.split(b"\r\n\r\n", 1)
    if b"101" not in head.split(b"\r\n")[0]:
        raise SystemExit(f"the upgrade was refused: {head!r}")
    return sock, buffer

def read_exact(sock, buffer, amount):
    while len(buffer) < amount:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the service closed the connection")
        buffer += chunk
    return buffer[:amount], buffer[amount:]

class Session:
    def __init__(self):
        self.sock, self.buffer = connect()
        self.next_id = 1
        self.sock.settimeout(15)
        self.send({"type": "hello", "protocol": 1, "sdk": "acceptance", "token": token})
        if self.message().get("type") != "welcome":
            raise SystemExit("the handshake was rejected")

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
            length = head[1] & 0x7F
            if length == 126:
                extra, self.buffer = read_exact(self.sock, self.buffer, 2)
                length = struct.unpack("!H", extra)[0]
            elif length == 127:
                extra, self.buffer = read_exact(self.sock, self.buffer, 8)
                length = struct.unpack("!Q", extra)[0]
            mask = b""
            if head[1] & 0x80:
                mask, self.buffer = read_exact(self.sock, self.buffer, 4)
            payload, self.buffer = read_exact(self.sock, self.buffer, length)
            if mask:
                payload = bytes(
                    byte ^ mask[index % 4] for index, byte in enumerate(payload)
                )
            if opcode == 1:
                return json.loads(payload)
            if opcode == 8:
                raise SystemExit("the service closed the connection")

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

session = Session()
document = json.load(open(os.environ["PK_DOC"]))
opened = session.call("doc.preview", document)
assert opened["ok"], opened
window = opened["result"]["windowId"]
print(f"doc.preview windowId={window}")

exported = session.call("preview.pages", {"windowId": window, "pageIndex": 0, "dpi": 150})
assert exported["ok"], exported
png = base64.b64decode(exported["result"]["png"])
assert png.startswith(b"\x89PNG\r\n\x1a\n"), "the page is not a PNG"
with open(os.environ["PK_PNG"], "wb") as handle:
    handle.write(png)
print(f"preview.pages bytes={len(png)}")

closed = session.call("preview.close", {"windowId": window})
assert closed["ok"], closed
missing = session.call("preview.pages", {"windowId": window, "pageIndex": 0, "dpi": 150})
assert not missing["ok"], missing
assert missing["error"]["code"] == "E_NOT_FOUND", missing
again = session.call("preview.close", {"windowId": window})
assert not again["ok"], again
assert again["error"]["code"] == "E_NOT_FOUND", again
unknown = session.call("preview.pages", {"windowId": "999999", "pageIndex": 0, "dpi": 150})
assert not unknown["ok"], unknown
assert unknown["error"]["code"] == "E_NOT_FOUND", unknown
print("closed window answers E_NOT_FOUND")
session.sock.close()
PY

# --- doc.preview traces the validation and render stages it shares ---------

trace_found=
for _ in $(seq 1 50); do
  "$bin" --data-dir "$data" logs --json >"$work/traces.json" 2>/dev/null || true
  if jq -e '
      group_by(.trace_id)
      | map(select(
          length == 5
          and (map(.stage) | sort) == ["auth", "recv", "render", "result", "validate"]
          and any(.[]; .stage == "recv" and .fields.document_sha256 != null and .fields.bytes != null)
          and any(.[]; .stage == "render" and .pages == 1 and .fields.html_ms != null)
          and any(.[]; .stage == "result" and .ok and .pages == 1)
        ))
      | length >= 1
    ' "$work/traces.json" >/dev/null 2>&1; then
    trace_found=1
    break
  fi
  sleep 0.05
done
[[ -n $trace_found ]] || fail "doc.preview wrote no recv/auth/validate/render/result trace"
printf 'preview trace: recv/auth/validate/render/result, no queue or submit\n'

# --- the window snapshot and the printed page share one layout -------------

"$bin" --data-dir "$work/pdf-data" pdf -i tests/fixtures/minimal.json -o "$work/minimal.pdf" \
  >"$work/pdf.log" 2>&1 || fail "the reference PDF failed"
pdftoppm -r 150 -png -singlefile "$work/minimal.pdf" "$work/pdfpage"
[[ -s $work/pdfpage.png ]] || fail "pdftoppm produced no page"

PK_PAGE=$work/page.png PK_PDF_PAGE=$work/pdfpage.png python3 - <<'PY'
import os, sys

page = os.environ["PK_PAGE"]
pdf = os.environ["PK_PDF_PAGE"]
try:
    from PIL import Image, ImageChops, ImageStat
except ImportError:
    from struct import unpack
    def size(path):
        with open(path, "rb") as handle:
            head = handle.read(24)
        return unpack(">II", head[16:24])
    width_a, height_a = size(page)
    width_b, height_b = size(pdf)
    assert abs(width_a - width_b) <= 1 and abs(height_a - height_b) <= 1, (
        f"page {width_a}x{height_a} vs PDF {width_b}x{height_b}"
    )
    print(f"page {width_a}x{height_a} matches PDF {width_b}x{height_b} (PIL absent)")
    sys.exit(0)

a = Image.open(page).convert("L")
b = Image.open(pdf).convert("L")
assert abs(a.width - b.width) <= 1 and abs(a.height - b.height) <= 1, (a.size, b.size)
b = b.crop((0, 0, min(a.width, b.width), min(a.height, b.height)))
a = a.crop((0, 0, b.width, b.height))
difference = ImageChops.difference(a, b)
mean = ImageStat.Stat(difference).mean[0]
ink_a = a.point(lambda value: 255 if value < 250 else 0).getbbox()
ink_b = b.point(lambda value: 255 if value < 250 else 0).getbbox()
assert mean <= 1.0, f"mean pixel difference {mean}"
assert ink_a is not None and ink_b is not None, "one image has no ink"
for left, right in zip(ink_a, ink_b):
    assert abs(left - right) <= 2, f"ink boxes differ: {ink_a} vs {ink_b}"
print(f"preview PNG {a.width}x{a.height} vs PDF: mean diff {mean:.3f}, ink {ink_a}")
PY

# --- eight windows fit, the ninth is refused -------------------------------

for index in $(seq 1 8); do
  "$bin" --data-dir "$data" --port "$port" preview -i tests/fixtures/minimal.json --json \
    >"$work/window-$index.json" 2>"$work/window-$index.err" \
    || fail "forwarded preview $index failed (see $work/window-$index.err)"
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["windowId"])' \
    "$work/window-$index.json" >/dev/null
done
printf 'eight forwarded previews opened\n'

set +e
"$bin" --data-dir "$data" --port "$port" preview -i tests/fixtures/minimal.json --json \
  >"$work/window-9.json" 2>"$work/window-9.err"
ninth=$?
set -e
[[ $ninth == 3 ]] || fail "the ninth preview exited $ninth, expected 3"
grep -q '"code":"E_TOO_LARGE"' "$work/window-9.err" \
  || fail "the ninth preview did not report E_TOO_LARGE"
printf 'ninth preview refused with E_TOO_LARGE\n'

# --- a busy lock that cannot be reached reports E_CONNECT -------------------

connect_dir=$work/connect
mkdir -p "$connect_dir"
dead_port=$(free_port)
# The single-instance lock is the OS lock on serve.lock.lock; a lock-shaped
# JSON file alone no longer means "another serve owns this directory". Hold
# the exclusive lock in a helper so forwarding — not own mode — is exercised.
printf '{"pid":%s,"port":%s}\n' "$$" "$dead_port" >"$connect_dir/serve.lock"
printf 'unused-token\n' >"$connect_dir/admin-token"
python3 - "$connect_dir/serve.lock.lock" <<'PY' &
import fcntl, sys, time
handle = open(sys.argv[1], "a+")
fcntl.flock(handle, fcntl.LOCK_EX)
time.sleep(30)
PY
lock_pid=$!
sleep 0.3
set +e
"$bin" --data-dir "$connect_dir" --port "$dead_port" preview -i tests/fixtures/minimal.json --json \
  >"$work/connect.out" 2>"$work/connect.err"
connect_status=$?
set -e
kill "$lock_pid" 2>/dev/null || true
wait "$lock_pid" 2>/dev/null || true
lock_pid=
[[ $connect_status == 3 ]] || fail "an unreachable lock exited $connect_status, expected 3"
grep -q '"code":"E_CONNECT"' "$work/connect.err" \
  || fail "an unreachable lock did not report E_CONNECT"
grep -q 'start `printkit serve` first' "$work/connect.err" \
  || fail "E_CONNECT did not point at printkit serve"
printf 'busy lock without a listener reports E_CONNECT\n'

own_dir=$work/own
own_port=$(free_port)
own_url="http://127.0.0.1:$own_port"
"$bin" --data-dir "$own_dir" --port "$own_port" preview -i tests/fixtures/minimal.json --json \
  >"$work/own.log" 2>&1 &
own_pid=$!
for _ in $(seq 1 100); do
  if grep -q 'listening on' "$work/own.log" 2>/dev/null \
    && curl -fsS -o /dev/null "$own_url/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
grep -q '"windowId"' "$work/own.log" || fail "the implicit serve printed no windowId"
grep -q 'listening on' "$work/own.log" || fail "the implicit serve did not start"
[[ -s $own_dir/serve.lock ]] || fail "the implicit serve did not hold serve.lock"
curl -fsS -o /dev/null "$own_url/admin/session" || fail "the implicit serve did not answer"
kill "$own_pid"
wait "$own_pid" 2>/dev/null || true
own_pid=
printf 'implicit preview started its own serve\n'

printf 'PASS: preview window, PNG export, trace, lifecycle, window cap and CLI paths\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
