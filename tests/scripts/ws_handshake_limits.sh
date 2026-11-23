#!/usr/bin/env bash
# WebSocket handshake limits: eight clients that never send `hello` hold all
# slots, the next handshake is refused, the first-frame timeout releases the
# slots, a normal handshake passes, and an oversized frame closes the
# transport.
set -euo pipefail
cd "$(dirname "$0")/../.."

bin=${PRINTKIT_BIN:-$PWD/target/debug/printkit}
if [[ ! -x $bin ]]; then
  cargo build -q -p printkit
fi

work=$(mktemp -d)
data=$work/data
mkdir -p "$data"
serve_pid=
cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)

"$bin" --data-dir "$data" --port "$port" serve >"$work/serve.log" 2>&1 &
serve_pid=$!

python3 - "$port" <<'PY'
import base64
import os
import socket
import struct
import sys
import time

port = int(sys.argv[1])


def connect_http():
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    sock.sendall(
        b"GET /admin/session HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    data = b""
    while True:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    return data.startswith(b"HTTP/1.1")


deadline = time.time() + 20
while True:
    try:
        if connect_http():
            break
    except OSError:
        pass
    if time.time() > deadline:
        raise SystemExit("serve did not start")
    time.sleep(0.1)


def handshake():
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(request.encode())
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            return sock, None
        data += chunk
    status = int(data.split(b"\r\n", 1)[0].split(b" ")[1])
    return sock, status


silent = []
for _ in range(8):
    sock, status = handshake()
    if status != 101:
        raise SystemExit(f"a normal handshake failed: {status}")
    silent.append(sock)

# Let the upgraded connections acquire their slots.
time.sleep(1.0)
over, status = handshake()
if status != 429:
    raise SystemExit(f"a ninth handshake was accepted: {status}")
over.close()

# The first frame has a 10 s deadline; after it, the slots are free.
time.sleep(11.5)
sock, status = handshake()
if status != 101:
    raise SystemExit(f"the slot was not released after the hello timeout: {status}")

# A frame header above the 32 MiB frame limit must close the transport.
header = bytearray([0x81, 0x80 | 127])
header += struct.pack("!Q", 32 * 1024 * 1024 + 1)
header += os.urandom(4)  # mask
sock.sendall(bytes(header))
sock.settimeout(5)
try:
    sock.recv(4096)
except (socket.timeout, ConnectionError):
    raise SystemExit("the oversized frame did not close the connection")
try:
    sock.close()
except OSError:
    pass
for client in silent:
    try:
        client.close()
    except OSError:
        pass

print("ws_handshake_limits: ok")
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
