#!/usr/bin/env bash
# Realtime acceptance: subscribe, print, disconnect mid-render, reconnect with
# the last sequence number and verify that the seven stages arrive once each.
#
# Usage: trace_subscribe.sh <data-dir>
# The data directory must come from `mock_print.sh` run with
# PRINTKIT_KEEP_DATA=1; that run writes the print and administrator tokens
# next to the database instead of deleting the directory on exit.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

data=${1:?usage: trace_subscribe.sh <data-dir>}
bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
admin_token=$(tr -d '\n' <"$data/admin-ws-token")
print_token=$(tr -d '\n' <"$data/print-token")
port=$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
)
serve_pid=

cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

"$bin" --data-dir "$data" --port "$port" serve >"$data/trace-serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$port/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
curl -fsS -o /dev/null "http://127.0.0.1:$port/admin/session"

python3 - "$port" "$admin_token" "$print_token" <<'PY'
import base64
import json
import os
import socket
import struct
import sys
import time

port = int(sys.argv[1])
admin_token = sys.argv[2]
print_token = sys.argv[3]


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self, token):
        sock = socket.create_connection(("127.0.0.1", port), timeout=30)
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
        self.send({"type": "hello", "protocol": 1, "sdk": "trace-test", "token": token})
        if self.message()["type"] != "welcome":
            raise SystemExit("handshake refused")

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

    def message(self, timeout=30):
        self.sock.settimeout(timeout)
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
                return json.loads(payload)
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

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def document(delay_ms):
    text = f"trace test fake-render-delay:{delay_ms}"
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
                        "data": {"text": text},
                    }
                ],
            }
        ],
    }


subscriber = Session(admin_token)
ack = subscriber.call("trace.subscribe", {})
if not ack.get("ok"):
    raise SystemExit(f"trace.subscribe failed: {ack}")

printer = Session(print_token)
reply = printer.call("doc.print", document(600))
if not reply.get("ok"):
    raise SystemExit(f"doc.print failed: {reply}")
job_id = reply["result"]["jobId"]

first = []
target = None
deadline = time.monotonic() + 15
while time.monotonic() < deadline:
    event = subscriber.message(timeout=5)
    if event.get("event") != "trace":
        continue
    if event.get("stage") == "recv":
        target = event["traceId"]
    if target and event.get("traceId") == target:
        first.append(event)
    if event.get("traceId") == target and event.get("stage") == "queue":
        break
if [event["stage"] for event in first] != ["recv", "auth", "validate", "queue"]:
    raise SystemExit(f"unexpected first half: {[event['stage'] for event in first]}")
last = first[-1]["seq"]
subscriber.close()

while True:
    event = printer.message(timeout=30)
    if event.get("jobId") == job_id and event.get("status") in ("done", "failed"):
        break
if event.get("status") != "done":
    raise SystemExit(f"job failed: {event}")

reconnected = Session(admin_token)
ack = reconnected.call("trace.subscribe", {"since": last})
if not ack.get("ok"):
    raise SystemExit(f"reconnect failed: {ack}")
second = []
deadline = time.monotonic() + 15
while time.monotonic() < deadline:
    event = reconnected.message(timeout=5)
    if event.get("event") == "trace" and event.get("traceId") == target:
        second.append(event)
    if event.get("event") == "trace" and event.get("traceId") == target and event.get("stage") == "result":
        break

all_events = first + second
sequences = [event["seq"] for event in all_events]
if sequences != sorted(sequences):
    raise SystemExit(f"sequences are not sorted: {sequences}")
if len(sequences) != len(set(sequences)):
    raise SystemExit(f"duplicate events after reconnect: {sequences}")
if [event["stage"] for event in all_events] != [
    "recv",
    "auth",
    "validate",
    "queue",
    "render",
    "submit",
    "result",
]:
    raise SystemExit(f"unexpected stages: {[event['stage'] for event in all_events]}")
render = next(event for event in all_events if event["stage"] == "render")
if not isinstance(render["fields"].get("html_ms"), int):
    raise SystemExit(f"html_ms is not a number: {render}")
if not isinstance(render["fields"].get("pdf_ms"), int):
    raise SystemExit(f"pdf_ms is not a number: {render}")
submit = next(event for event in all_events if event["stage"] == "submit")
if not isinstance(submit["fields"].get("spooler_job"), int):
    raise SystemExit(f"spooler_job is not a number: {submit}")
queue = next(event for event in all_events if event["stage"] == "queue")
if not isinstance(queue.get("queuePos"), int) or queue["queuePos"] < 1:
    raise SystemExit(f"queue_pos is missing: {queue}")

print("first connection:", [event["stage"] for event in first])
print("after reconnect:", [event["stage"] for event in second])
print("sequence range:", sequences[0], "-", sequences[-1], "no duplicates")
print("render fields:", render["fields"])
print("submit fields:", submit["fields"])
print("trace subscribe: OK")
reconnected.close()
printer.close()
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
