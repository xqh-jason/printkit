#!/usr/bin/env bash
# macOS raw-channel acceptance against a virtual IPP printer.
#
# A small IPP printer (Python, embedded below) accepts raw jobs and stores the
# document it receives. The script creates a CUPS queue for it and checks that
# ESC/POS-style bytes arrive byte for byte through both entry points: the CLI
# `printkit raw` and the service method `raw.send`. No device and no paper are
# involved; this proves the byte path, not a printed label.
#
# Usage: fake_raw_mac.sh
#   PRINTKIT_BIN  binary to exercise (default: target/debug/printkit)
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"

[[ $(uname -s) == Darwin ]] || {
  echo "fake_raw_mac.sh: macOS only" >&2
  exit 1
}
for tool in lpadmin lpstat ipptool jq curl python3 shasum; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "fake_raw_mac.sh: $tool is missing" >&2
    exit 1
  }
done

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
work=
queue=
hold_queue=
sink_pid=
hold_pid=
serve_pid=

fail() {
  echo "fake_raw_mac.sh: $1" >&2
  exit 1
}

cleanup() {
  local status=$?
  for pid in "$serve_pid" "$sink_pid" "$hold_pid"; do
    if [ -n "$pid" ]; then
      kill "$pid" >/dev/null 2>&1 || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if [[ -n $queue ]] && ! cups_test_remove_queue "$queue"; then
    status=1
  fi
  if [[ -n $hold_queue ]] && ! cups_test_remove_queue "$hold_queue"; then
    status=1
  fi
  [[ -z $work ]] || rm -rf "$work"
  cups_test_lock_release || status=1
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# The lock covers the build as well as CUPS: the three acceptance scripts
# rebuild target/debug/printkit with different feature sets, so an unlocked
# build would replace the binary a parallel run is executing.
cups_test_lock_acquire \
  || fail "could not acquire the CUPS test lock within ${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-60}s"
cargo build -q -p printkit

work=$(mktemp -d)
queue="printkit-raw-$$"
hold_queue="printkit-rawhold-$$"
data="$work/data"
mkdir -p "$data" "$work/raw" "$work/hold"
# These tests assert trace stages; opt in observability for the temporary data only.
printf 'version = 1\nsettled = 2\npreset = "core"\n[[plugins]]\nid = "observability"\nenabled = true\n[[plugins]]\nid = "records"\nenabled = true\n' >"$data/plugins.toml"

sink_port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
hold_port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"

# --- virtual IPP printer ---------------------------------------------------

cat >"$work/raw_sink.py" <<'PY'
"""Minimal IPP printer that stores every document it receives, verbatim.

The CUPS ipp backend talks Print-Job / Create-Job + Send-Document; job state
and Cancel-Job are answered so the client's cancellation reaches the device.
`hold` keeps every job in the processing state until Cancel-Job arrives.
"""
import os
import socket
import struct
import sys
import threading

TAG_OPERATION = 0x01
TAG_END = 0x03

TAG_INTEGER = 0x21
TAG_BOOLEAN = 0x22
TAG_ENUM = 0x23
TAG_CHARSET = 0x47
TAG_LANGUAGE = 0x48
TAG_KEYWORD = 0x44
TAG_URI = 0x45
TAG_MIMETYPE = 0x49

PRINT_JOB = 0x0002
VALIDATE_JOB = 0x0004
CREATE_JOB = 0x0005
SEND_DOCUMENT = 0x0006
CANCEL_JOB = 0x0008
GET_JOB = 0x0009
GET_JOBS = 0x000A
GET_PRINTER = 0x000B
CLOSE_JOB = 0x003B

PENDING = 3
PROCESSING = 5
CANCELED = 7
COMPLETED = 9


def attr(tag, name, value):
    if isinstance(name, str):
        name = name.encode()
    if isinstance(value, str):
        value = value.encode()
    out = bytearray()
    out.append(tag)
    out += struct.pack(">H", len(name))
    out += name
    out += struct.pack(">H", len(value))
    out += value
    return bytes(out)


def attr_multi(tag, name, values):
    """One 1setOf attribute: the name appears only on the first value."""
    out = bytearray()
    for index, value in enumerate(values):
        out.append(tag)
        if index == 0:
            out += struct.pack(">H", len(name))
            out += name.encode()
        else:
            out += struct.pack(">H", 0)
        out += struct.pack(">H", len(value))
        out += value
    return bytes(out)


def integer(value):
    return struct.pack(">i", value)


def parse_attrs(data):
    groups = []
    current = None
    offset = 0
    while offset < len(data):
        tag = data[offset]
        offset += 1
        if tag == TAG_END:
            return groups, offset
        if tag < 0x10:
            current = (tag, [])
            groups.append(current)
            continue
        if current is None:
            return groups, offset
        name_length = struct.unpack(">H", data[offset:offset + 2])[0]
        offset += 2
        name = data[offset:offset + name_length].decode(errors="replace")
        offset += name_length
        value_length = struct.unpack(">H", data[offset:offset + 2])[0]
        offset += 2
        value = data[offset:offset + value_length]
        offset += value_length
        current[1].append((tag, name, value))
    return groups, offset


def find_attr(groups, name):
    for _, attrs in groups:
        for tag, key, value in attrs:
            if key == name:
                return tag, value
    return None


def read_request(conn):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = conn.recv(65536)
        if not chunk:
            return None
        data += chunk
    head, rest = data.split(b"\r\n\r\n", 1)
    lines = head.decode("latin-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        key, _, value = line.partition(":")
        headers[key.strip().lower()] = value.strip()
    return headers, rest


def read_body(conn, headers, rest):
    if headers.get("transfer-encoding", "").lower() == "chunked":
        body = bytearray()
        buf = rest
        while True:
            while b"\r\n" not in buf:
                buf += conn.recv(65536)
            line, buf = buf.split(b"\r\n", 1)
            size = int(line.split(b";")[0], 16)
            if size == 0:
                while b"\r\n" not in buf:
                    buf += conn.recv(65536)
                return bytes(body)
            while len(buf) < size + 2:
                buf += conn.recv(65536)
            body += buf[:size]
            buf = buf[size + 2:]
    length = int(headers.get("content-length", "0"))
    body = bytearray(rest)
    while len(body) < length:
        chunk = conn.recv(65536)
        if not chunk:
            break
        body += chunk
    return bytes(body[:length])


class Sink:
    def __init__(self, port, directory, hold):
        self.port = port
        self.directory = directory
        self.hold = hold
        self.lock = threading.Lock()
        self.next_job = 1
        self.jobs = {}
        self.current = None

    def save(self, job_id, document):
        path = os.path.join(self.directory, "job-%d.bin" % job_id)
        with open(path, "wb") as fh:
            fh.write(document)
        state = PROCESSING if self.hold else COMPLETED
        self.jobs[job_id] = state
        print("received job %d: %d bytes" % (job_id, len(document)), flush=True)


def response(request_id, status, groups=b"", version=(2, 0)):
    out = bytearray()
    out += struct.pack(">BBHI", version[0], version[1], status, request_id)
    out += bytes([TAG_OPERATION])
    out += attr(TAG_CHARSET, "attributes-charset", b"utf-8")
    out += attr(TAG_LANGUAGE, "attributes-natural-language", b"en")
    out += groups
    out += bytes([TAG_END])
    return bytes(out)


def job_attrs(sink, job_id, state):
    groups = bytes([0x02])
    groups += attr(TAG_URI, "job-uri", "ipp://127.0.0.1:%d/job/%d" % (sink.port, job_id))
    groups += attr(TAG_INTEGER, "job-id", integer(job_id))
    groups += attr(TAG_ENUM, "job-state", integer(state))
    if state == COMPLETED:
        groups += attr(TAG_KEYWORD, "job-state-reasons", b"job-completed-successfully")
    elif state == PROCESSING:
        groups += attr(TAG_KEYWORD, "job-state-reasons", b"job-printing")
    elif state == CANCELED:
        groups += attr(TAG_KEYWORD, "job-state-reasons", b"job-canceled-by-user")
    return groups


def handle(conn, sink):
    request = read_request(conn)
    if request is None:
        return
    headers, rest = request
    if headers.get("expect", "").lower() == "100-continue":
        conn.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
    body = read_body(conn, headers, rest)
    if len(body) < 8:
        conn.sendall(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
        return
    major, minor, op, request_id = struct.unpack(">BBHI", body[:8])
    groups, offset = parse_attrs(body[8:])

    def reply(status, body_groups=b""):
        return response(request_id, status, body_groups, (major, minor))
    document = body[8 + offset:]

    if op in (PRINT_JOB, SEND_DOCUMENT):
        with sink.lock:
            if op == PRINT_JOB:
                job_id = sink.next_job
                sink.next_job += 1
            else:
                found = find_attr(groups, "job-id")
                job_id = int.from_bytes(found[1], "big") if found else sink.current
            sink.save(job_id, document)
            state = sink.jobs[job_id]
        data = reply(0x0000, job_attrs(sink, job_id, state))
    elif op == CREATE_JOB:
        with sink.lock:
            job_id = sink.next_job
            sink.next_job += 1
            sink.current = job_id
            sink.jobs[job_id] = PROCESSING if sink.hold else COMPLETED
        data = reply(0x0000, job_attrs(sink, job_id, PENDING))
    elif op == GET_JOB:
        found = find_attr(groups, "job-id")
        job_id = int.from_bytes(found[1], "big")
        with sink.lock:
            state = sink.jobs.get(job_id, COMPLETED)
        data = reply(0x0000, job_attrs(sink, job_id, state))
    elif op == CANCEL_JOB:
        found = find_attr(groups, "job-id")
        job_id = int.from_bytes(found[1], "big")
        with sink.lock:
            sink.jobs[job_id] = CANCELED
        open(os.path.join(sink.directory, "cancelled-%d" % job_id), "w").close()
        print("cancelled job %d" % job_id, flush=True)
        data = reply(0x0000, job_attrs(sink, job_id, CANCELED))
    elif op == GET_PRINTER:
        groups = bytes([0x04])
        groups += attr(TAG_URI, "printer-uri-supported", "ipp://127.0.0.1:%d/ipp/print" % sink.port)
        groups += attr(TAG_KEYWORD, "uri-security-supported", b"none")
        groups += attr(TAG_KEYWORD, "uri-authentication-supported", b"none")
        groups += attr(TAG_KEYWORD, "printer-name", b"printkit-raw")
        groups += attr(TAG_ENUM, "printer-state", integer(3))
        groups += attr(TAG_KEYWORD, "printer-state-reasons", b"none")
        groups += attr(TAG_BOOLEAN, "printer-is-accepting-jobs", b"\x01")
        groups += attr(TAG_INTEGER, "queued-job-count", integer(0))
        groups += attr(TAG_MIMETYPE, "document-format-default", b"application/octet-stream")
        groups += attr(TAG_MIMETYPE, "document-format-supported", b"application/octet-stream")
        # The CUPS ipp backend only polls Get-Job-Attributes (and therefore
        # only waits for completion) when this list names that operation.
        groups += attr_multi(
            TAG_ENUM,
            "operations-supported",
            [integer(operation) for operation in (2, 4, 5, 6, 8, 9, 10, 11, 59)],
        )
        data = reply(0x0000, groups)
    elif op == GET_JOBS:
        groups = b""
        with sink.lock:
            for job_id, state in sink.jobs.items():
                groups += job_attrs(sink, job_id, state)
        data = reply(0x0000, groups)
    else:
        data = reply(0x0000)

    conn.sendall(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nConnection: close\r\n"
        b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n" + data
    )


def main(port, directory, hold):
    sink = Sink(port, directory, hold)
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", port))
    listener.listen(16)
    print("listening on %d" % port, flush=True)
    while True:
        conn, _ = listener.accept()
        try:
            handle(conn, sink)
        except Exception as error:  # keep serving after a malformed request
            print("request failed: %s" % error, flush=True)
        finally:
            conn.close()


if __name__ == "__main__":
    main(int(sys.argv[1]), sys.argv[2], sys.argv[3] == "hold")
PY

cat >"$work/raw.ppd" <<'EOF'
*PPD-Adobe: "4.3"
*FormatVersion: "4.3"
*FileVersion: "1.0"
*LanguageVersion: English
*LanguageEncoding: ISOLatin1
*PCFileName: "PKRAW.PPD"
*Manufacturer: "Printkit"
*ModelName: "VirtualRaw"
*ShortNickName: "Printkit VirtualRaw"
*NickName: "Printkit VirtualRaw"
*1284DeviceID: "MFG:Printkit;MDL:VirtualRaw;"
*OpenUI *PageSize: PickOne
*DefaultPageSize: A4
*PageSize A4/A4: "<</PageSize[595 842]>>setpagedevice"
*CloseUI: *PageSize
EOF

python3 "$work/raw_sink.py" "$sink_port" "$work/raw" complete >"$work/sink.log" 2>&1 &
sink_pid=$!
python3 "$work/raw_sink.py" "$hold_port" "$work/hold" hold >"$work/hold.log" 2>&1 &
hold_pid=$!
for _ in $(seq 1 100); do
  grep -q "listening" "$work/sink.log" 2>/dev/null && grep -q "listening" "$work/hold.log" 2>/dev/null && break
  sleep 0.05
done
grep -q "listening" "$work/sink.log" || fail "the raw sink did not start"
grep -q "listening" "$work/hold.log" || fail "the hold sink did not start"

lpadmin -p "$queue" -E -v "ipp://127.0.0.1:$sink_port/ipp/print" -P "$work/raw.ppd" \
  || fail "lpadmin could not create $queue"
lpadmin -p "$hold_queue" -E -v "ipp://127.0.0.1:$hold_port/ipp/print" -P "$work/raw.ppd" \
  || fail "lpadmin could not create $hold_queue"
lpstat -p "$queue" >/dev/null || fail "CUPS does not know $queue"
lpstat -p "$hold_queue" >/dev/null || fail "CUPS does not know $hold_queue"
printf 'virtual IPP printer: %s -> ipp://127.0.0.1:%s/ipp/print\n' "$queue" "$sink_port"

# --- payload ---------------------------------------------------------------

python3 - "$work/payload.bin" <<'PY'
import sys

# A label job in the shapes the raw channel exists for: an ESC/POS reset and
# raster command, TSPL and ZPL headers, and every byte value so a text
# round-trip anywhere in the chain shows up as a hash mismatch.
payload = bytearray()
payload += b"\x1b@"                      # ESC/POS initialize
payload += b"printkit raw acceptance\n"
payload += b"\x1d\x76\x30\x00\x08\x00"    # GS v 0 raster header
payload += bytes(range(256))
payload += b"SIZE 60 mm, 40 mm\nGAP 2 mm, 0\n"
payload += b"^XA^FO40,40^A0N,30,30^FDprintkit^FS^XZ\n"
payload += b"! 0 200 200 210 1\n"
payload += b"\x1b\x64\x03"                # ESC/POS feed 3 lines
open(sys.argv[1], "wb").write(payload)
PY
expected=$(shasum -a 256 "$work/payload.bin" | awk '{ print $1 }')
bytes=$(wc -c <"$work/payload.bin" | tr -d ' ')
printf 'payload: %s bytes, sha256 %s\n' "$bytes" "$expected"

new_file() {
  local directory=$1
  local before=$2
  local after
  for _ in $(seq 1 200); do
    after=$(ls "$directory" 2>/dev/null || true)
    if [ "$after" != "$before" ]; then
      comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep '\.bin$' | head -1
      return 0
    fi
    sleep 0.05
  done
  return 1
}

# --- CLI -------------------------------------------------------------------

before_cli=$(ls "$work/raw" 2>/dev/null || true)
result=$("$bin" --data-dir "$data" raw -f "$work/payload.bin" -p "$queue" --json)
printf 'CLI raw: %s\n' "$result"
job=$(jq -r '.jobId' <<<"$result")
trace=$(jq -r '.traceId' <<<"$result")
[[ $job != null && $job != 0 ]] || fail "the CLI did not report a spooler job id"
[[ $(jq -r '.bytes' <<<"$result") == "$bytes" ]] || fail "the CLI reported the wrong byte count"

delivered=$(new_file "$work/raw" "$before_cli") || fail "the virtual printer received no document"
actual=$(shasum -a 256 "$work/raw/$delivered" | awk '{ print $1 }')
[[ $actual == "$expected" ]] || fail "the raw bytes changed on the way to the printer"
printf 'CLI raw job %s: bytes identical (%s)\n' "$job" "$actual"

stats=$("$bin" --data-dir "$data" stats --group day --json)
jq -e '.buckets | length == 1 and .[0].count == 1 and .[0].sheets == 1' \
  <<<"$stats" >/dev/null \
  || fail "the CLI raw job is missing from the usage table: $stats"
printf 'usage table: 1 CLI raw submission, 1 sheet\n'

"$bin" --data-dir "$data" logs --trace "$trace" --json \
  | jq -e --argjson job "$job" '
      (map(.stage) | index("render") | not)
      and any(.[]; .stage == "submit" and .fields.spooler_job == $job)
    ' >/dev/null || fail "the raw trace has a render stage or no spooler job id"
printf 'trace %s: recv/validate/submit/result, no render stage\n' "$trace"

# The spooler itself reports the job: that is the "real spooler state" claim.
ipptool -tv -d "job-id=$job" "ipp://localhost:631/printers/$queue" \
  "$work/jobattr.test" >"$work/cli-attrs.txt" 2>&1 || true
cat >"$work/jobattr.test" <<'EOF'
{
  OPERATION Get-Job-Attributes
  GROUP operation-attributes-tag
  ATTR charset attributes-charset utf-8
  ATTR language attributes-natural-language en
  ATTR uri printer-uri $uri
  ATTR integer job-id $job-id
  DISPLAY job-state
  DISPLAY document-format
}
EOF
ipptool -tv -d "job-id=$job" "ipp://localhost:631/printers/$queue" \
  "$work/jobattr.test" >"$work/cli-attrs.txt" 2>&1 || true
grep -q 'job-state (enum) = completed' "$work/cli-attrs.txt" \
  || fail "the spooler does not report the raw job as completed"
grep -q 'document-format (mimeMediaType) = application/vnd.cups-raw' "$work/cli-attrs.txt" \
  || fail "the spooler did not file the job as raw"
printf 'spooler job %s: completed, application/vnd.cups-raw\n' "$job"

# --- service: scopes and raw.send ------------------------------------------

"$bin" --data-dir "$data" --port "$port" serve >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.05
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer on $port"

admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "the admin session did not set a CSRF cookie"

admin_post() {
  local path=$1
  local body=$2
  curl -sS -o "$data/body.json" -w '%{http_code}' \
    -b "$jar" -c "$jar" -H "Origin: $base" -H "X-CSRF-Token: $csrf" \
    -H 'Content-Type: application/json' -X POST --data "$body" "$base$path"
}

login_status=$(admin_post /admin/login "{\"token\":\"$admin\"}")
if [ "$login_status" != 200 ]; then
  cat "$data/body.json" >&2
  tail -5 "$data/serve.log" >&2
  fail "admin login failed with status $login_status"
fi
[[ $(admin_post /admin/origins '{"origin":"https://print.example"}') == 200 ]] \
  || fail "adding an origin failed"

[[ $(admin_post /admin/tokens '{"origin":"https://print.example","nonBrowser":true}') == 201 ]] \
  || fail "issuing a print token failed"
print_token=$(jq -r '.token' "$data/body.json")

[[ $(admin_post /admin/tokens '{"origin":"https://print.example","nonBrowser":true,"raw":true}') == 201 ]] \
  || fail "issuing a raw token failed"
raw_token=$(jq -r '.token' "$data/body.json")

python3 - "$port" "$queue" "$hold_queue" "$work" "$print_token" "$raw_token" <<'PY'
import base64
import json
import os
import socket
import struct
import sys
import time

port = int(sys.argv[1])
queue = sys.argv[2]
hold_queue = sys.argv[3]
work = sys.argv[4]
print_token = sys.argv[5]
raw_token = sys.argv[6]
payload = open(f"{work}/payload.bin", "rb").read()


def connect():
    sock = socket.create_connection(("127.0.0.1", port), timeout=60)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall(
        (
            f"GET /print HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = sock.recv(4096)
        if not chunk:
            raise SystemExit("the server closed during the upgrade")
        buffer += chunk
    head, rest = buffer.split(b"\r\n\r\n", 1)
    if b" 101 " not in head.split(b"\r\n")[0]:
        raise SystemExit(f"upgrade failed: {head!r}")
    return sock, rest


def read_exact(sock, buffer, size):
    while len(buffer) < size:
        chunk = sock.recv(65536)
        if not chunk:
            raise SystemExit("the connection closed early")
        buffer += chunk
    return buffer[:size], buffer[size:]


class Session:
    def __init__(self, token):
        self.sock, self.buffer = connect()
        self.next_id = 1
        self.send({"type": "hello", "protocol": 1, "sdk": "acceptance", "token": token})
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
            frame, self.buffer = read_exact(self.sock, self.buffer, length)
            if mask:
                frame = bytes(byte ^ mask[index % 4] for index, byte in enumerate(frame))
            if opcode == 1:
                return json.loads(frame)
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


def wait_status(session, job_id, wanted, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        reply = session.call("job.status", {"jobId": job_id})
        if not reply.get("ok"):
            raise SystemExit(f"job.status failed: {reply}")
        if reply["result"]["status"] == wanted:
            return reply["result"]
        if time.monotonic() > deadline:
            raise SystemExit(f"job {job_id} never reached {wanted}: {reply['result']}")
        time.sleep(0.05)


encoded = base64.b64encode(payload).decode()


def documents(directory):
    return {name for name in os.listdir(directory) if name.endswith(".bin")}


def markers(directory):
    return {name for name in os.listdir(directory) if name.startswith("cancelled-")}


def wait_new(directory, before, pattern, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        after = documents(directory) if pattern == "document" else markers(directory)
        new = after - before
        if new:
            return new.pop()
        if time.monotonic() > deadline:
            raise SystemExit(f"nothing new arrived in {directory}")
        time.sleep(0.05)


print_session = Session(print_token)
denied = print_session.call("raw.send", {"printer": queue, "bytes": encoded})
assert not denied["ok"], denied
assert denied["error"]["code"] == "E_AUTH", denied
print("print token -> raw.send: E_AUTH")
print_session.close()

raw_session = Session(raw_token)
before = documents(f"{work}/raw")
reply = raw_session.call("raw.send", {"printer": queue, "bytes": encoded})
assert reply["ok"], reply
job_id = reply["result"]["jobId"]
name = wait_new(f"{work}/raw", before, "document")
assert open(f"{work}/raw/{name}", "rb").read() == payload, "the service raw bytes changed"
status = wait_status(raw_session, job_id, "done")
print(f"raw token -> raw.send: job {job_id} done, bytes identical ({name})")

# A held job stays in the spooler, so Cancel-Job must reach the device.
before = documents(f"{work}/hold")
held = raw_session.call("raw.send", {"printer": hold_queue, "bytes": encoded})
assert held["ok"], held
held_id = held["result"]["jobId"]
wait_new(f"{work}/hold", before, "document")
wait_status(raw_session, held_id, "done")
seen = markers(f"{work}/hold")
cancel = raw_session.call("job.cancel", {"jobId": held_id})
assert cancel["ok"], cancel
marker = wait_new(f"{work}/hold", seen, "marker")
print(f"job.cancel -> device received {marker} for {held_id}")
raw_session.close()
PY

# The held queue no longer lists the cancelled job.
if lpstat -W not-completed -o "$hold_queue" 2>/dev/null | grep -q "$hold_queue-"; then
  fail "the cancelled raw job is still in the spooler queue"
fi
printf 'cancelled raw job left the spooler queue\n'

printf 'PASS: raw bytes are identical through the CLI and the service, E_AUTH without the raw scope, real spooler cancel\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
