#!/usr/bin/env python3
"""One WebSocket request for the plugin e2e scripts.

Usage: plugins_client.py <port> <token> <method> [params-json]
Prints {"welcome": ..., "response": ...} on stdout.
"""
import base64
import json
import os
import time
import socket
import struct
import sys

port = int(sys.argv[1])
token = sys.argv[2]
method = sys.argv[3]
params = json.loads(sys.argv[4]) if len(sys.argv) > 4 else None


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("connection closed")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            (
                f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        buffer = b""
        while b"\r\n\r\n" not in buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise SystemExit("closed during upgrade")
            buffer += chunk
        head, self.buffer = buffer.split(b"\r\n\r\n", 1)
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise SystemExit("upgrade failed")
        self.send({"type": "hello", "protocol": 1, "sdk": "plugins-e2e", "token": token})
        self.welcome = self.message()

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
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        self.sock.sendall(bytes(header) + mask + masked)

    def message(self):
        while True:
            while len(self.buffer) < 2:
                self.buffer += self.sock.recv(65536)
            b1, b2 = self.buffer[0], self.buffer[1]
            length = b2 & 0x7F
            offset = 2
            if length == 126:
                while len(self.buffer) < 4:
                    self.buffer += self.sock.recv(65536)
                length = struct.unpack("!H", self.buffer[2:4])[0]
                offset = 4
            elif length == 127:
                while len(self.buffer) < 10:
                    self.buffer += self.sock.recv(65536)
                length = struct.unpack("!Q", self.buffer[2:10])[0]
                offset = 10
            while len(self.buffer) < offset + length:
                self.buffer += self.sock.recv(65536)
            payload = self.buffer[offset : offset + length]
            self.buffer = self.buffer[offset + length :]
            if b1 & 0x0F == 1:
                return json.loads(payload)
            if b1 & 0x0F == 8:
                raise SystemExit("server closed")


session = Session()
if method == "welcome":
    print(json.dumps({"welcome": session.welcome}))
    sys.exit(0)
if method == "hold":
    since = params["since"]
    ready = params["ready"]
    flag = params["flag"]
    session.send(
        {"id": "hold-subscribe", "method": "trace.subscribe", "params": {"since": since}}
    )
    while True:
        response = session.message()
        if response.get("id") == "hold-subscribe":
            break
    if not response.get("ok"):
        raise SystemExit(f"trace.subscribe failed: {response}")
    with open(ready, "w", encoding="utf-8") as handle:
        handle.write("ready")
    while not os.path.exists(flag):
        time.sleep(0.05)
    print(json.dumps({"ok": True}))
    sys.exit(0)
if method == "sequence":
    responses = []
    window = None
    for index, step in enumerate(params):
        step_params = step.get("params")
        if isinstance(step_params, dict) and step_params.get("windowId") == "$window":
            step_params = dict(step_params)
            step_params["windowId"] = window
        session.send(
            {
                "id": f"plugins-e2e-{index + 2}",
                "method": step["method"],
                "params": step_params,
            }
        )
        request_id = f"plugins-e2e-{index + 2}"
        while True:
            response = session.message()
            if response.get("id") == request_id:
                break
        responses.append(response)
        if window is None and isinstance(response.get("result"), dict):
            window = response["result"].get("windowId")
    print(
        json.dumps(
            {"welcome": session.welcome, "responses": responses}, ensure_ascii=False
        )
    )
    sys.exit(0)
session.send({"id": "plugins-e2e-1", "method": method, "params": params})
print(json.dumps({"welcome": session.welcome, "response": session.message()}, ensure_ascii=False))
