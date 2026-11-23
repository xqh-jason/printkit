#!/usr/bin/env bash
# End-to-end pairing check: admin login, origin whitelist, token issuance,
# WebSocket handshake, refusal codes and a real doc.exportPdf round trip.
#
# The WebSocket client is a small Python program because no installed client
# can set the `Origin` header, and the handshake cases exist precisely to
# exercise that header.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

profile=${PROFILE:-debug}
bin=${PRINTKIT_BIN:-$root/target/$profile/printkit}
if [[ ! -x $bin ]]; then
  cargo build -p printkit --features serve
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
ws_out="$data/ws.jsonl"
serve_pid=

cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$data"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

printf 'version = 1\nsettled = 2\npreset = "core"\n[[plugins]]\nid = "observability"\nenabled = true\n' >"$data/plugins.toml"
"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!

for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer on $port"
status=$(curl -sS -o /dev/null -w '%{http_code}' -H 'Host: evil.example' "$base/admin/session")
[[ $status == 403 ]] || fail "a non-loopback Host reached /admin: $status"
status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'Host: evil.example' "$base/print/batch")
[[ $status == 403 ]] || fail "a non-loopback Host reached /print/batch: $status"
printf 'host guard: evil Host refused with 403\n'
status=$(curl -sS -D "$data/notfound.headers" -o "$data/notfound.json" -w '%{http_code}' "$base/no-such-route")
[[ $status == 404 ]] || fail "unknown route returned $status, expected 404"
grep -qi 'content-type: application/json' "$data/notfound.headers" \
  || fail "404 body is not JSON"
jq -e '.ok == false and .error.code == "E_NOT_FOUND"' "$data/notfound.json" >/dev/null \
  || fail "404 body is not the shared error envelope"
status=$(curl -sS -D "$data/method.headers" -o "$data/method.json" -w '%{http_code}' -X OPTIONS "$base/admin/login")
[[ $status == 405 ]] || fail "OPTIONS returned $status, expected 405"
[[ $(tr -d '\r' <"$data/method.headers" | grep -ci 'content-type: application/json') -gt 0 ]] \
  || fail "405 body is not JSON"
curl -sS -D "$data/session.headers" -o /dev/null "$base/admin/session"
tr -d '\r' <"$data/session.headers" | grep -qi 'cache-control: no-store' \
  || fail "/admin/session lacks Cache-Control: no-store"
printf 'error envelope: 404/405 JSON, admin no-store\n'



jq -e --argjson port "$port" '.port == $port and .pid > 0' "$data/serve.lock" >/dev/null \
  || fail "serve.lock does not describe this run"
if "$bin" serve --data-dir "$data" --port "$port" >"$data/second.log" 2>&1; then
  fail "a second serve claimed the same data directory"
fi
grep -q E_ALREADY_RUNNING "$data/second.log" \
  || fail "the second serve did not report E_ALREADY_RUNNING"

# --- admin token file and CSRF bootstrap ------------------------------------

token_mode=$(stat -f '%Lp' "$data/admin-token")
[[ $token_mode == 600 ]] || fail "admin-token mode is $token_mode, expected 600"

admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "the admin session did not set a CSRF cookie"
origin_header="$base"

admin_post() {
  local path=$1 body=$2 csrf_header=$3
  local args=(-sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar"
    -H "Origin: $origin_header" -H 'Content-Type: application/json')
  if [[ -n $csrf_header ]]; then
    args+=(-H "X-CSRF-Token: $csrf_header")
  fi
  curl "${args[@]}" -X POST --data "$body" "$base$path"
}

status=$(admin_post /admin/login "{\"token\":\"$admin\"}" "$csrf")
[[ $status == 200 ]] || fail "login returned $status, expected 200"

status=$(admin_post /admin/origins '{"origin":"https://app.example"}' "$csrf")
[[ $status == 200 ]] || fail "adding an origin returned $status, expected 200"

status=$(admin_post /admin/origins '{"origin":"https://second.example"}' "$csrf")
[[ $status == 200 ]] || fail "adding a second origin returned $status, expected 200"

status=$(admin_post /admin/origins '{"origin":"https://third.example"}' "")
[[ $status == 403 ]] || fail "a write without CSRF returned $status, expected 403"
printf 'write without CSRF: %s\n' "$status"
code=$(jq -r '.error.code' "$data/body.json")
[[ $code == E_CSRF ]] || fail "a write without CSRF returned $code, expected E_CSRF"
printf 'write without CSRF code: %s\n' "$code"

status=$(curl -sS -o "$data/body.json" -w '%{http_code}' \
  -H "Origin: $origin_header" -H 'Content-Type: application/json' \
  -X POST --data '{"origin":"https://fourth.example"}' "$base/admin/origins")
[[ $status == 401 ]] || fail "a write without a session returned $status, expected 401"
code=$(jq -r '.error.code' "$data/body.json")
[[ $code == E_AUTH ]] || fail "a write without a session returned $code, expected E_AUTH"
printf 'write without session code: %s\n' "$code"

origin_header="https://evil.example"
status=$(admin_post /admin/origins '{"origin":"https://fifth.example"}' "$csrf")
[[ $status == 403 ]] || fail "a write from a bad origin returned $status, expected 403"
code=$(jq -r '.error.code' "$data/body.json")
[[ $code == E_ORIGIN ]] || fail "a write from a bad origin returned $code, expected E_ORIGIN"
printf 'write from a bad origin code: %s\n' "$code"
origin_header="$base"

status=$(admin_post /admin/tokens '{"origin":"https://app.example"}' "$csrf")
[[ $status == 201 ]] || fail "issuing a token returned $status, expected 201"
token=$(jq -r '.token' "$data/body.json")
[[ $token == pk_* ]] || fail "the issued token has no pk_ prefix"

status=$(admin_post /admin/tokens '{"origin":"https://app.example","nonBrowser":true}' "$csrf")
[[ $status == 201 ]] || fail "issuing a script token returned $status, expected 201"
script_token=$(jq -r '.token' "$data/body.json")

# --- WebSocket handshake, refusal paths and exportPdf ------------------------

python3 - "$port" "$token" "$script_token" "$root/tests/fixtures/minimal.json" >"$ws_out" <<'PY'
import base64
import json
import os
import socket
import struct
import sys

port = int(sys.argv[1])
token = sys.argv[2]
script_token = sys.argv[3]
with open(sys.argv[4], encoding="utf-8") as handle:
    document = json.load(handle)


def connect(origin=None):
    sock = socket.create_connection(("127.0.0.1", port), timeout=30)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    )
    if origin is not None:
        request += f"Origin: {origin}\r\n"
    request += "\r\n"
    sock.sendall(request.encode())
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = sock.recv(4096)
        if not chunk:
            raise SystemExit("the server closed during the upgrade")
        buffer += chunk
    head, rest = buffer.split(b"\r\n\r\n", 1)
    if b" 101 " not in head.split(b"\r\n")[0]:
        raise SystemExit("upgrade failed: " + head.decode("utf-8", "replace"))
    return sock, rest


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


def upgrade_status(origin):
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        f"Origin: {origin}\r\n\r\n"
    )
    sock.sendall(request.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = sock.recv(4096)
        if not chunk:
            break
        head += chunk
    sock.close()
    return int(head.split(b"\r\n", 1)[0].split(b" ")[1])


def send_text(sock, payload):
    data = payload.encode()
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


def recv_text(sock, buffer):
    while True:
        head, buffer = read_exact(sock, buffer, 2)
        opcode = head[0] & 0x0F
        masked = head[1] & 0x80
        length = head[1] & 0x7F
        if length == 126:
            extra, buffer = read_exact(sock, buffer, 2)
            length = struct.unpack("!H", extra)[0]
        elif length == 127:
            extra, buffer = read_exact(sock, buffer, 8)
            length = struct.unpack("!Q", extra)[0]
        mask = b""
        if masked:
            mask, buffer = read_exact(sock, buffer, 4)
        payload, buffer = read_exact(sock, buffer, length)
        if masked:
            payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        if opcode == 1:
            return payload.decode(), buffer
        if opcode == 8:
            raise SystemExit("the server closed the connection")


def emit(case, message):
    print(json.dumps({"case": case, "message": message}))


def handshake(hello, origin=None):
    sock, buffer = connect(origin)
    send_text(sock, json.dumps(hello))
    message, buffer = recv_text(sock, buffer)
    return sock, buffer, json.loads(message)


untokened = {"type": "hello", "protocol": 1, "sdk": "pairing-e2e"}

# A token bound to app.example still cannot be used from second.example.
sock, buffer, message = handshake(
    {**untokened, "token": token, "origin": "https://second.example"},
    "https://second.example",
)
emit("origin", message)

# No token: whitelisted browser and local script both print; an unlisted
# origin is refused before the upgrade.
sock, buffer, message = handshake(
    {**untokened, "origin": "https://app.example"}, "https://app.example"
)
emit("empty-print", message)
sock.close()

status = upgrade_status("https://evil.example")
emit("empty-origin", "HTTP " + str(status))



local, local_buffer, message = handshake(untokened)
emit("local-print", message)

# A print-only session never reaches raw.send.
send_text(local, json.dumps({"id": "r0", "method": "raw.send", "params": {}}))
message, local_buffer = recv_text(local, local_buffer)
emit("local-raw", json.loads(message))
local.close()

# An unknown token is still an auth refusal, with a trace id.
sock, buffer, message = handshake({**untokened, "token": "pk_unknown"})
emit("unknown", message)
sock.close()

# A script token is refused when the browser sends an Origin header.
sock, buffer, message = handshake(
    {**untokened, "token": script_token, "origin": "https://app.example"},
    "https://app.example",
)
emit("nonbrowser", message)
sock.close()

sock, buffer, message = handshake(
    {**untokened, "token": token, "origin": "https://app.example"},
    "https://app.example",
)
emit("welcome", message)

send_text(sock, json.dumps({"id": "u1", "method": "not.a.method"}))
message, buffer = recv_text(sock, buffer)
emit("unsupported", json.loads(message))

# The print token has no raw scope, so the frozen method table must refuse
# raw.send before any dispatch happens.
send_text(sock, json.dumps({"id": "r1", "method": "raw.send", "params": {}}))
message, buffer = recv_text(sock, buffer)
emit("scope", json.loads(message))

send_text(
    sock,
    json.dumps(
        {
            "id": "x1",
            "method": "doc.exportPdf",
            "params": {"document": document, "path": "e2e/out.pdf"},
        }
    ),
)
message, buffer = recv_text(sock, buffer)
emit("export", json.loads(message))

send_text(
    sock,
    json.dumps(
        {
            "id": "x2",
            "method": "doc.exportPdf",
            "params": {"document": document, "path": "../escape.pdf"},
        }
    ),
)
message, buffer = recv_text(sock, buffer)
emit("escape", json.loads(message))
sock.close()
PY

origin_trace=$(jq -r 'select(.case == "origin") | .message.error.detail.traceId' "$ws_out")
unknown_trace=$(jq -r 'select(.case == "unknown") | .message.error.detail.traceId' "$ws_out")

jq -e 'select(.case == "origin") | .message.type == "rejected" and .message.error.code == "E_ORIGIN"' "$ws_out" >/dev/null \
  || fail "the unlisted origin was not rejected with E_ORIGIN"
jq -e 'select(.case == "empty-print") | .message.type == "welcome" and .message.granted == ["print"]' "$ws_out" >/dev/null \
  || fail "a whitelisted browser without a token did not receive the print scope"
jq -e 'select(.case == "empty-origin") | .message == "HTTP 403"' "$ws_out" >/dev/null \
  || fail "an empty token from an unlisted origin was not refused before the upgrade"
jq -e 'select(.case == "local-print") | .message.type == "welcome" and .message.granted == ["print"]' "$ws_out" >/dev/null \
  || fail "a local handshake without a token did not receive the print scope"
jq -e 'select(.case == "local-raw") | .message.ok == false and .message.error.code == "E_AUTH"' "$ws_out" >/dev/null \
  || fail "a local no-token session reached raw.send"
jq -e 'select(.case == "nonbrowser") | .message.type == "rejected" and .message.error.code == "E_ORIGIN"' "$ws_out" >/dev/null \
  || fail "a script token was accepted from a browser origin"
jq -e 'select(.case == "unknown") | .message.type == "rejected" and .message.error.code == "E_AUTH"' "$ws_out" >/dev/null \
  || fail "an unknown token was not rejected with E_AUTH"
jq -e 'select(.case == "welcome") | .message.type == "welcome" and .message.granted == ["print"]' "$ws_out" >/dev/null \
  || fail "the welcome frame does not grant exactly [print]"
jq -e 'select(.case == "unsupported") | .message.ok == false and .message.error.code == "E_UNSUPPORTED"' "$ws_out" >/dev/null \
  || fail "an unimplemented method did not return E_UNSUPPORTED"
jq -e 'select(.case == "export") | .message.ok == true and (.message.result.path | startswith("exports/")) and (.message.result.url | startswith("http://127.0.0.1:")) and (.message.result.expiresAt > 0)' "$ws_out" >/dev/null \
  || fail "doc.exportPdf did not return an exports/ path and a signed URL"
export_url=$(jq -r 'select(.case == "export") | .message.result.url' "$ws_out")
jq -e 'select(.case == "escape") | .message.ok == false and .message.error.code == "E_SCHEMA"' "$ws_out" >/dev/null \
  || fail "an escaping export path did not return E_SCHEMA"
jq -e 'select(.case == "scope") | .message.ok == false and .message.error.code == "E_AUTH"' "$ws_out" >/dev/null \
  || fail "a print token reached raw.send without the raw scope"

[[ -f $data/exports/e2e/out.pdf ]] || fail "the exported PDF is missing"
# `GET /exports` is a capability URL: a bare path is refused before any read.
curl -sS -o "$data/export-unsigned.json" -w '%{http_code} %{content_type}' \
  "$base/exports/e2e/out.pdf" >"$data/export-unsigned.status"
grep -qx '403 application/json; charset=utf-8' "$data/export-unsigned.status" \
  || fail "an unsigned export URL was not refused with 403 JSON"
jq -e '.ok == false and .error.code == "E_AUTH"' "$data/export-unsigned.json" >/dev/null \
  || fail "an unsigned export URL did not return E_AUTH"

curl -sS -o "$data/export-403.json" -w '%{http_code} %{content_type}' \
  -H 'Host: evil.example' "$export_url" >"$data/export-403.status"
grep -qx '403 application/json; charset=utf-8' "$data/export-403.status" \
  || fail "a foreign Host export did not return 403 JSON"
jq -e '.ok == false and .error.code == "E_ORIGIN"' "$data/export-403.json" >/dev/null \
  || fail "a foreign Host export did not return the JSON error body"

curl -sS -o "$data/export-ok.pdf" -w '%{http_code} %{content_type}' \
  "$export_url" >"$data/export-ok.status"
grep -qx '200 application/pdf' "$data/export-ok.status" \
  || fail "a successful export is not application/pdf"

curl -sS -I "$export_url" >"$data/export-head.txt"
head_status=$(awk 'NR == 1 { print $2 }' "$data/export-head.txt")
[[ $head_status == 200 ]] || fail "HEAD export returned $head_status"
grep -qi '^content-length: [1-9]' "$data/export-head.txt" \
  || fail "HEAD export did not report a content length"

# Exactly one export ran a renderer. The escaping request fails before the
# host is called, so a second render event would mean the guard is bypassed.
renders=$( "$bin" logs --data-dir "$data" --json | jq '[.[] | select(.stage == "render")] | length' )
[[ $renders -eq 1 ]] || fail "expected exactly one render event, found $renders"

for pair in "origin:$origin_trace:E_ORIGIN" "unknown:$unknown_trace:E_AUTH"; do
  IFS=: read -r name trace code <<<"$pair"
  [[ -n $trace && $trace != null ]] || fail "the $name refusal carries no traceId"
  "$bin" logs --data-dir "$data" --trace "$trace" --json >"$data/$name.json"
  jq -e --arg code "$code" 'length >= 1 and any(.[]; .stage == "auth" and .code == $code)' "$data/$name.json" >/dev/null \
    || fail "no auth-stage $code event under trace $trace"
done

printf 'admin-token mode: %s\n' "$token_mode"
printf 'handshake granted: print token %s, empty browser %s, local %s\n' \
  "$(jq -c 'select(.case == "welcome") | .message.granted' "$ws_out")" \
  "$(jq -c 'select(.case == "empty-print") | .message.granted' "$ws_out")" \
  "$(jq -c 'select(.case == "local-print") | .message.granted' "$ws_out")"
printf 'refusals: E_ORIGIN trace=%s, E_AUTH trace=%s\n' "$origin_trace" "$unknown_trace"
printf 'export path: %s\n' "$(jq -r 'select(.case == "export") | .message.result.path' "$ws_out")"
printf 'escape refused: %s, render events: %s\n' \
  "$(jq -r 'select(.case == "escape") | .message.error.code' "$ws_out")" "$renders"
printf 'raw.send without raw scope: %s\n' \
  "$(jq -r 'select(.case == "scope") | .message.error.code' "$ws_out")"

# --- shutdown ---------------------------------------------------------------

status=$(admin_post /admin/shutdown '{}' "")
[[ $status == 403 ]] || fail "shutdown without CSRF returned $status, expected 403"
printf 'shutdown without CSRF: %s\n' "$status"

# Deleting an origin must stop an already-connected socket from printing.
status=$(admin_post /admin/origins '{"origin":"https://revoked.example"}' "$csrf")
[[ $status == 200 ]] || fail "adding the revocable origin returned $status"
status=$(admin_post /admin/tokens '{"origin":"https://revoked.example"}' "$csrf")
[[ $status == 201 ]] || fail "issuing the revocable token returned $status"
revoked_token=$(jq -r '.token' "$data/body.json")
admin_cookie=$(awk '$6 == "pk_admin" { print $7 }' "$jar")
python3 - "$port" "$revoked_token" "$admin_cookie" "$csrf" "$base" "$root/tests/fixtures/minimal.json" <<'PY'
import base64, json, os, socket, struct, sys, urllib.request

port, token, admin_cookie, csrf, base, document_path = (
    int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6]
)
document = json.load(open(document_path))
sock = socket.create_connection(("127.0.0.1", port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
sock.sendall(
    (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        "Origin: https://revoked.example\r\n\r\n"
    ).encode()
)
buffer = b""
while b"\r\n\r\n" not in buffer:
    buffer += sock.recv(4096)
head, buffer = buffer.split(b"\r\n\r\n", 1)
assert b" 101 " in head.split(b"\r\n")[0], head

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

def receive():
    global buffer
    while len(buffer) < 2:
        buffer += sock.recv(65536)
    length = buffer[1] & 0x7F
    offset = 2
    if length == 126:
        while len(buffer) < 4:
            buffer += sock.recv(65536)
        length = struct.unpack("!H", buffer[2:4])[0]
        offset = 4
    elif length == 127:
        while len(buffer) < 10:
            buffer += sock.recv(65536)
        length = struct.unpack("!Q", buffer[2:10])[0]
        offset = 10
    while len(buffer) < offset + length:
        buffer += sock.recv(65536)
    payload = buffer[offset : offset + length]
    buffer = buffer[offset + length :]
    return json.loads(payload)

send({"type": "hello", "protocol": 1, "sdk": "pairing", "token": token})
receive()  # welcome
request = urllib.request.Request(
    f"{base}/admin/origins",
    data=json.dumps({"origin": "https://revoked.example"}).encode(),
    method="DELETE",
    headers={
        "Origin": base,
        "Content-Type": "application/json",
        "Cookie": f"pk_admin={admin_cookie}; pk_csrf={csrf}",
        "X-CSRF-Token": csrf,
    },
)
with urllib.request.urlopen(request, timeout=10) as response:
    if response.status != 200:
        raise SystemExit(f"origin delete returned {response.status}")
document["requestId"] = "after-origin-delete"
send({"id": "after-delete", "method": "doc.print", "params": document})
reply = receive()
if reply.get("ok") or reply.get("error", {}).get("code") != "E_ORIGIN":
    raise SystemExit(f"a removed origin still printed: {reply!r}")
send({"id": "after-delete-info", "method": "sys.info", "params": None})
info = receive()
if info.get("ok") or info.get("error", {}).get("code") != "E_ORIGIN":
    raise SystemExit(f"a removed origin still read sys.info: {info!r}")
print("removed origin blocked print and sys.info on an existing socket: E_ORIGIN")
PY

status=$(admin_post /admin/shutdown '{}' "$csrf")
[[ $status == 202 ]] || fail "shutdown returned $status, expected 202"
jq -e '.drained == true and .inflight == 0' "$data/body.json" >/dev/null \
  || fail "shutdown did not report a drained service"
printf 'shutdown with CSRF: %s (drained)\n' "$status"

for _ in $(seq 1 100); do
  if ! kill -0 "$serve_pid" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if kill -0 "$serve_pid" 2>/dev/null; then
  fail "serve did not exit after the shutdown request"
fi
if ! wait "$serve_pid"; then
  fail "serve did not exit cleanly after the shutdown request"
fi
serve_pid=
[[ ! -e $data/serve.lock ]] || fail "serve.lock survived a clean shutdown"

printf 'pairing e2e: OK\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
