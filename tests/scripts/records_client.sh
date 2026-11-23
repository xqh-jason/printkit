#!/usr/bin/env bash
# Call one observability method over the WebSocket and print the JSON response.
#
# Usage: records_client.sh <port> <token> <method> [params-json]
# The token must be a script token (issued with nonBrowser=true), because the
# handshake here sends no Origin header.
set -euo pipefail

if [[ $# -lt 3 ]]; then
  printf 'usage: %s <port> <token> <method> [params-json]\n' "$0" >&2
  exit 2
fi

port=$1
token=$2
method=$3
params=${4:-null}

python3 - "$port" "$token" "$method" "$params" <<'PY'
import base64
import json
import os
import socket
import struct
import sys

port = int(sys.argv[1])
token = sys.argv[2]
method = sys.argv[3]
params = json.loads(sys.argv[4])


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self):
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
        self.send({"type": "hello", "protocol": 1, "sdk": "records-client", "token": token})
        welcome = self.message()
        if welcome.get("type") != "welcome":
            raise SystemExit(f"handshake refused: {welcome}")
        self.events = []

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

    def call(self, name, arguments):
        self.send({"id": "call-1", "method": name, "params": arguments})
        while True:
            reply = self.message()
            if reply.get("id") == "call-1":
                return reply


session = Session()
reply = session.call(method, params)
if method == "doc.print" and reply.get("ok"):
    job_id = reply["result"]["jobId"]
    session.sock.settimeout(30.0)
    terminal = None
    while terminal is None:
        event = session.message()
        if event.get("type") == "event" and event.get("jobId") == job_id:
            if event.get("status") in ("done", "failed"):
                terminal = event
    print(json.dumps({"response": reply, "job": terminal}, indent=2))
elif method == "trace.subscribe":
    # Collect the stream for a short window: the acknowledgement alone does
    # not prove that events arrive.
    live = []
    session.sock.settimeout(2.0)
    try:
        while len(live) < 200:
            message = session.message()
            if message.get("type") == "event" and message.get("event") == "trace":
                live.append(message)
    except (socket.timeout, TimeoutError):
        pass
    print(json.dumps({"response": reply, "events": live}, indent=2))
else:
    print(json.dumps(reply, indent=2))
PY
