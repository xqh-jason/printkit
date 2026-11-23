#!/usr/bin/env bash
# WebSocket admission/idle regression: an unpaired Origin must not get a 101,
# one Origin cannot take every slot, idle authenticated sockets release their
# budget, and a long job started over the socket keeps it alive past the idle
# window even while it emits no frames.
#
# The fake-render host honours a `fake-render-delay:<ms>` marker in the
# rendered text, so the slow-job case needs no printer or WebView.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

if [[ -n ${PRINTKIT_BIN:-} ]]; then
  bin=$PRINTKIT_BIN
else
  # The slow-job assertion needs the fake renderer's delay marker; rebuilding
  # unconditionally also avoids a binary another gate left without the feature.
  cargo build -q -p printkit --features fake-render
  bin=$root/target/debug/printkit
fi

idle_secs=${PRINTKIT_WS_IDLE_SECS:-2}

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
serve_pid=
cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

PRINTKIT_WS_IDLE_SECS=$idle_secs "$bin" --data-dir "$data" --port "$port" serve >"$work/serve.log" 2>&1 &
serve_pid=$!

python3 - "$port" "$data/admin-token" "$idle_secs" <<'PY'
import base64
import http.client
import json
import os
import socket
import struct
import sys
import time

port = int(sys.argv[1])
token_path = sys.argv[2]
idle_secs = float(sys.argv[3])
base = f"http://127.0.0.1:{port}"
origin = base

deadline = time.time() + 20
while True:
    try:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
        conn.request("GET", "/admin/session")
        conn.getresponse().read()
        break
    except OSError:
        if time.time() > deadline:
            raise SystemExit("serve did not start")
        time.sleep(0.1)


def request(method, path, body=None, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    payload = json.dumps(body).encode() if body is not None else None
    hdrs = dict(headers or {})
    if payload is not None:
        hdrs["Content-Type"] = "application/json"
    conn.request(method, path, body=payload, headers=hdrs)
    response = conn.getresponse()
    data = response.read()
    return response.status, response.getheaders(), data


status, headers, _ = request("GET", "/admin/session")
cookies = {}
csrf = None
for name, value in headers:
    if name.lower() == "set-cookie":
        pair = value.split(";", 1)[0]
        key, val = pair.split("=", 1)
        cookies[key] = val
        if key == "pk_csrf":
            csrf = val
if csrf is None:
    raise SystemExit("no pk_csrf cookie")

with open(token_path, encoding="utf-8") as handle:
    admin = handle.read().strip()
cookie_header = "; ".join(f"{key}={value}" for key, value in cookies.items())
status, headers, _ = request(
    "POST",
    "/admin/login",
    {"token": admin},
    {"Origin": origin, "X-CSRF-Token": csrf, "Cookie": cookie_header},
)
if status != 200:
    raise SystemExit(f"admin login failed: {status}")
for name, value in headers:
    if name.lower() == "set-cookie" and value.startswith("pk_admin="):
        cookies["pk_admin"] = value.split(";", 1)[0].split("=", 1)[1]
cookie_header = "; ".join(f"{key}={value}" for key, value in cookies.items())

for business in ("http://a.example", "http://b.example"):
    status, _, body = request(
        "POST",
        "/admin/origins",
        {"origin": business},
        {"Origin": origin, "X-CSRF-Token": csrf, "Cookie": cookie_header},
    )
    if status != 200:
        raise SystemExit(f"cannot add {business}: {status} {body!r}")


def handshake(origin_header=None):
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    request_text = (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    )
    if origin_header:
        request_text += f"Origin: {origin_header}\r\n"
    request_text += "\r\n"
    sock.sendall(request_text.encode())
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            return sock, None
        data += chunk
    status = int(data.split(b"\r\n", 1)[0].split(b" ")[1])
    return sock, status


sock, status = handshake("http://evil.example")
if status != 403:
    raise SystemExit(f"unpaired Origin received {status}, expected 403")
sock.close()

hello = json.dumps({"type": "hello", "protocol": 1, "sdk": "idle-probe"}).encode()


def send_text(sock, text):
    mask = os.urandom(4)
    length = len(text)
    if length < 126:
        header = bytes([0x81, 0x80 | length])
    elif length < 65536:
        header = bytes([0x81, 0x80 | 126]) + struct.pack("!H", length)
    else:
        header = bytes([0x81, 0x80 | 127]) + struct.pack("!Q", length)
    sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(text)))


clients_a = []
clients_b = []
for _ in range(4):
    sock, status = handshake("http://a.example")
    if status != 101:
        raise SystemExit(f"paired handshake failed: {status}")
    send_text(sock, hello)
    clients_a.append(sock)


def expect_close(sock):
    sock.settimeout(5)
    data = sock.recv(2)
    if not data:
        return True
    return (data[0] & 0x0F) == 0x8


# The per-Origin budget is 4: the fifth a.example upgrade completes (global
# slots remain) but the server closes it with 1013 before it can send a hello.
sock, status = handshake("http://a.example")
if status != 101:
    raise SystemExit(f"per-origin admission changed the HTTP status: {status}")
if not expect_close(sock):
    raise SystemExit("a fifth connection from one Origin was not closed")
sock.close()

# Fill the remaining global slots from a second paired Origin.
for _ in range(4):
    sock, status = handshake("http://b.example")
    if status != 101:
        raise SystemExit(f"paired handshake failed: {status}")
    send_text(sock, hello)
    clients_b.append(sock)

# Eight authenticated connections now hold every slot; a ninth is refused at
# the HTTP level.
sock, status = handshake("http://a.example")
if status != 429:
    raise SystemExit(f"a ninth handshake got {status}, expected 429")
sock.close()

# Every socket is idle. After the override they close themselves and release
# both the global and per-Origin budgets.
time.sleep(idle_secs + 0.4)
sock, status = handshake("http://a.example")
if status != 101:
    raise SystemExit(f"idle sockets did not release their slots: {status}")
sock.close()
for client in clients_a + clients_b:
    client.close()

# A job started on this socket is in flight: with the accelerated idle window
# it must stay connected past it even though the slow render emits nothing
# between "printing" and "done". The production window is 300 s; the policy,
# not the wall clock, is what this regression pins.
sock, status = handshake("http://a.example")
if status != 101:
    raise SystemExit(f"slow-job handshake failed: {status}")
send_text(sock, hello)


def recv_frame(sock, timeout=20):
    sock.settimeout(timeout)
    head = b""
    while len(head) < 2:
        chunk = sock.recv(2 - len(head))
        if not chunk:
            return None, b""
        head += chunk
    opcode = head[0] & 0x0F
    length = head[1] & 0x7F
    if length == 126:
        raw = b""
        while len(raw) < 2:
            chunk = sock.recv(2 - len(raw))
            if not chunk:
                return None, b""
            raw += chunk
        length = struct.unpack("!H", raw)[0]
    elif length == 127:
        raw = b""
        while len(raw) < 8:
            chunk = sock.recv(8 - len(raw))
            if not chunk:
                return None, b""
            raw += chunk
        length = struct.unpack("!Q", raw)[0]
    payload = b""
    while len(payload) < length:
        chunk = sock.recv(length - len(payload))
        if not chunk:
            return None, b""
        payload += chunk
    return opcode, payload


def recv_reply(sock, request_id, timeout=20):
    while True:
        opcode, payload = recv_frame(sock, timeout)
        if opcode is None or opcode == 0x8:
            return None
        if opcode != 0x1:
            continue
        value = json.loads(payload)
        if value.get("id") == request_id:
            return value


slow_document = {
    "version": 1,
    "pages": [
        {
            "spec": {"size": "A4"},
            "items": [
                {
                    "id": "slow",
                    "type": "text",
                    "frame": {"x": 20, "y": 20, "width": 160, "height": 12},
                    "style": {"fontSize": 8, "lineHeight": 1.4},
                    "data": {"text": "fake-render-delay:5000"},
                }
            ],
        }
    ],
    "output": {"copies": 1, "collate": True, "printer": {"name": "mock"}},
}
send_text(
    sock,
    json.dumps({"id": "slow-print", "method": "doc.print", "params": slow_document}).encode(),
)
reply = recv_reply(sock, "slow-print")
if reply is None or not reply.get("ok"):
    raise SystemExit(f"doc.print was not accepted: {reply!r}")

# Wait well past two accelerated idle windows without sending anything. The
# job is still rendering, so this socket must remain usable.
time.sleep(idle_secs * 2.3)
send_text(sock, json.dumps({"id": "after-idle", "method": "sys.info", "params": {}}).encode())
reply = recv_reply(sock, "after-idle")
if reply is None:
    raise SystemExit("a socket with an in-flight job was closed past the idle window")
if not reply.get("ok"):
    raise SystemExit(f"the socket answered with an error after the idle window: {reply!r}")
sock.close()

print("ws_idle_limits: ok (evil origin refused, per-origin cap, idle release, in-flight keep-alive)")
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
