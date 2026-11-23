#!/usr/bin/env bash
# macOS print acceptance against a virtual IPP printer.
#
# Creates an `ippeveprinter` queue with `lpadmin`, submits through the CLI and
# the service, and reads the job attributes back from `cupsd`. No real printer
# and no paper are involved: this proves the submission chain (spooler job id,
# one job per batch, capability bits, cancellation), not the printed sheet.
#
# Usage: fake_printer_mac.sh
#   PRINTKIT_BIN  binary to exercise (default: target/debug/printkit)
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"

[[ $(uname -s) == Darwin ]] || {
  printf 'SKIP: this script drives CUPS on macOS\n' >&2
  exit 0
}
for tool in ippeveprinter lpadmin lpstat ipptool jq curl python3; do
  command -v "$tool" >/dev/null || {
    printf 'SKIP: %s is missing\n' "$tool" >&2
    exit 0
  }
done

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
work=
queue=
ippeve_pid=
serve_pid=

cleanup() {
  local status=$?
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  if [[ -n $ippeve_pid ]] && kill -0 "$ippeve_pid" 2>/dev/null; then
    kill "$ippeve_pid" 2>/dev/null || true
    wait "$ippeve_pid" 2>/dev/null || true
  fi
  if [[ -n $queue ]] && ! cups_test_remove_queue "$queue"; then
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

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

# The lock covers the build as well as CUPS: the three acceptance scripts
# rebuild target/debug/printkit with different feature sets, so an unlocked
# build would replace the binary a parallel run is executing.
cups_test_lock_acquire \
  || fail "could not acquire the CUPS test lock within ${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-60}s"
cargo build -q -p printkit

work=$(mktemp -d)
queue="printkit-ipp-$$"
spool="$work/spool"
data="$work/data"
mkdir -p "$spool" "$data"
# These tests assert trace stages; opt in observability for the temporary data only.
printf 'version = 1\nsettled = 2\npreset = "core"\n[[plugins]]\nid = "observability"\nenabled = true\n[[plugins]]\nid = "records"\nenabled = true\n' >"$data/plugins.toml"

ipp_port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"

# --- virtual IPP printer ---------------------------------------------------

# PID reuse could otherwise adopt a queue left by a previously killed run.
cups_test_remove_queue "$queue" || fail "could not remove stale queue $queue"
ippeveprinter -p "$ipp_port" -d "$spool" -k -s 60000,0 -v "$queue" \
  >"$work/ippeve.log" 2>&1 &
ippeve_pid=$!
for _ in $(seq 1 100); do
  if python3 - "$ipp_port" <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.2)
s.close()
PY
  then
    break
  fi
  sleep 0.1
done
kill -0 "$ippeve_pid" 2>/dev/null || fail "ippeveprinter did not start"

lpadmin -p "$queue" -E -v "ipp://127.0.0.1:$ipp_port/ipp/print" -m everywhere \
  || fail "lpadmin could not create $queue"
for _ in $(seq 1 100); do
  if lpstat -p "$queue" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
lpstat -p "$queue" >/dev/null || fail "CUPS does not know $queue"
printf 'virtual IPP printer: %s -> ipp://127.0.0.1:%s/ipp/print\n' "$queue" "$ipp_port"

spooler_ids() {
  lpstat -W all -o "$queue" 2>/dev/null | awk 'NF { print $1 }' | sort
}

wait_completed() {
  local id=$1
  for _ in $(seq 1 300); do
    if lpstat -W completed -o "$queue" 2>/dev/null | grep -q "^$queue-$id "; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

cat >"$work/jobattr.test" <<'EOF'
{
    OPERATION Get-Job-Attributes
    GROUP operation-attributes-tag
    ATTR charset attributes-charset utf-8
    ATTR language attributes-natural-language en
    ATTR uri printer-uri $uri
    ATTR integer job-id $job-id
    DISPLAY job-state
    DISPLAY job-impressions-completed
    DISPLAY media
    DISPLAY sides
    DISPLAY document-format
}
EOF

# --- one A4 one-sided job, with the submit stage ---------------------------

before=$(spooler_ids)
result=$("$bin" --data-dir "$data" print -i tests/fixtures/minimal.json \
  -p "$queue" -n 1 --json)
printf 'CLI print: %s\n' "$result"
job=$(jq -r '.jobId' <<<"$result")
trace=$(jq -r '.traceId' <<<"$result")
[[ $(jq -r '.pages' <<<"$result") == 1 ]] || fail "one page expected"
[[ $(jq -r '.copies' <<<"$result") == 1 ]] || fail "one copy expected"
[[ $job != 0 && $job != null ]] || fail "no spooler job id"

"$bin" --data-dir "$data" logs --trace "$trace" --json \
  | jq -e --argjson job "$job" \
    'any(.[]; .stage == "submit" and .fields.spooler_job == $job)' >/dev/null \
  || fail "the submit stage does not carry the spooler job id"
printf 'trace %s: submit carries spooler_job=%s\n' "$trace" "$job"

wait_completed "$job" || fail "job $job never completed"
after=$(spooler_ids)
new_jobs=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep -c . || true)
[[ $new_jobs == 1 ]] || fail "one CLI print produced $new_jobs spooler jobs"

ipptool -tv -d "job-id=$job" "ipp://localhost:631/printers/$queue" \
  "$work/jobattr.test" >"$work/attrs.txt" 2>&1
grep -q 'job-state (enum) = completed' "$work/attrs.txt" \
  || fail "the spooler does not report a completed job"
grep -q 'media (keyword) = iso_a4_210x297mm' "$work/attrs.txt" \
  || fail "the job is not A4"
grep -q 'sides (keyword) = one-sided' "$work/attrs.txt" \
  || fail "the job is not one-sided"
grep -q 'document-format (mimeMediaType) = application/pdf' "$work/attrs.txt" \
  || fail "the job did not carry PDF"
grep -q 'job-impressions-completed (integer) = 1' "$work/attrs.txt" \
  || fail "the spooler did not report one impression"
printf 'spooler job %s: A4, one-sided, application/pdf, completed, 1 impression\n' "$job"

# --- copies and one-job batches -------------------------------------------

before=$(spooler_ids)
result=$("$bin" --data-dir "$data" print -i tests/fixtures/minimal.json \
  -p "$queue" -n 2 --json)
printf 'CLI print -n 2: %s\n' "$result"
[[ $(jq -r '.copies' <<<"$result") == 2 ]] || fail "two copies expected"
[[ $(jq -r '.sheets' <<<"$result") == 2 ]] || fail "two sheets expected"
after=$(spooler_ids)
new_jobs=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep -c . || true)
[[ $new_jobs == 1 ]] || fail "-n 2 produced $new_jobs spooler jobs"

python3 - "$work" <<'PY'
import json, sys
work = sys.argv[1]
doc = json.load(open("tests/fixtures/minimal.json"))
page2 = json.loads(json.dumps(doc["pages"][0]))
page2["items"][0]["id"] = "second"
page2["items"][0]["data"]["text"] = "printkit batch page two"
doc["pages"].append(page2)
json.dump([doc, doc], open(f"{work}/batch.json", "w"))
json.dump([doc for _ in range(120)], open(f"{work}/batch120.json", "w"))
bad = json.loads(json.dumps(doc))
bad["pages"][0]["items"][0]["unknownField"] = 1
json.dump([doc for _ in range(49)] + [bad] + [doc for _ in range(50)],
          open(f"{work}/batch-bad50.json", "w"))
PY

before=$(spooler_ids)
result=$("$bin" --data-dir "$data" print -i "$work/batch.json" --batch \
  -p "$queue" --json)
printf 'CLI print --batch: %s\n' "$result"
[[ $(jq -r '.pages' <<<"$result") == 4 ]] || fail "four pages expected"
after=$(spooler_ids)
new_jobs=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep -c . || true)
[[ $new_jobs == 1 ]] || fail "--batch produced $new_jobs spooler jobs"

# Three successful CLI prints: 1 + 2 + 4 sheets in the usage table.
stats=$("$bin" --data-dir "$data" stats --group day --json)
jq -e '.buckets | length == 1 and .[0].count == 3 and .[0].sheets == 7' \
  <<<"$stats" >/dev/null \
  || fail "the CLI prints are missing from the usage table: $stats"
printf 'usage table: 3 CLI prints, 7 sheets\n'

# --- all-or-nothing batches -----------------------------------------------

before=$(spooler_ids)
if "$bin" --data-dir "$data" print -i "$work/batch120.json" --batch \
  -p "$queue" --json >"$work/too-many.json" 2>&1; then
  fail "120 documents were accepted"
fi
cat "$work/too-many.json"
jq -e '.code == "E_TOO_LARGE"' "$work/too-many.json" >/dev/null \
  || fail "120 documents did not fail with E_TOO_LARGE"

if "$bin" --data-dir "$data" print -i "$work/batch-bad50.json" --batch \
  -p "$queue" --json >"$work/bad-part.json" 2>&1; then
  fail "a batch with an invalid 50th part was accepted"
fi
cat "$work/bad-part.json"
jq -e '.code == "E_SCHEMA" and .detail.pointer == "/docs/49/pages/0/items/0/unknownField"' \
  "$work/bad-part.json" >/dev/null || fail "the invalid part has no exact pointer"
after=$(spooler_ids)
new_jobs=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | grep -c . || true)
[[ $new_jobs == 0 ]] || fail "a rejected batch produced $new_jobs spooler jobs"
printf 'rejected batches: E_TOO_LARGE and E_SCHEMA /docs/49, 0 spooler jobs\n'

# --- service --------------------------------------------------------------

"$bin" --data-dir "$data" --port "$port" serve >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
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
status=$(admin_post /admin/origins '{"origin":"https://print.example"}')
[[ $status == 200 ]] || fail "adding an origin returned $status"
status=$(admin_post /admin/tokens '{"origin":"https://print.example","nonBrowser":true}')
[[ $status == 201 ]] || fail "issuing a print token returned $status"
token=$(jq -r '.token' "$data/body.json")
printf '%s\n' "$token" >"$work/print-token"

python3 - "$work" <<'PY'
import json
pages = 1000
objects = [b"<< /Type /Catalog /Pages 2 0 R >>"]
kids = " ".join(f"{index} 0 R" for index in range(3, 3 + pages))
objects.append(f"<< /Type /Pages /Kids [{kids}] /Count {pages} >>".encode())
objects.extend(
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << >> >>"
    for _ in range(pages)
)
out = bytearray(b"%PDF-1.4\n")
offsets = []
for index, body in enumerate(objects, 1):
    offsets.append(len(out))
    out += f"{index} 0 obj\n".encode() + body + b"\nendobj\n"
xref = len(out)
out += f"xref\n0 {len(objects) + 1}\n".encode()
out += b"0000000000 65535 f \n"
for offset in offsets:
    out += f"{offset:010d} 00000 n \n".encode()
out += (
    f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
    f"startxref\n{xref}\n%%EOF\n"
).encode()
open(f"{__import__('sys').argv[1]}/1000-pages.pdf", "wb").write(out)
PY
pdf_bytes=$(wc -c <"$work/1000-pages.pdf" | tr -d ' ')

python3 - "$port" "$queue" "$work" <<'PY'
import base64
import ctypes
import ctypes.util
import json
import os
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request


def gui_session_available():
    core_graphics = ctypes.CDLL(ctypes.util.find_library("CoreGraphics"))
    core_graphics.CGSessionCopyCurrentDictionary.restype = ctypes.c_void_p
    return bool(core_graphics.CGSessionCopyCurrentDictionary())

port = int(sys.argv[1])
queue = sys.argv[2]
work = sys.argv[3]
token = open(f"{work}/print-token").read().strip()
doc = json.load(open("tests/fixtures/minimal.json"))


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
        last = status
        time.sleep(0.05)
    raise SystemExit(f"could not connect: {last}")


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
            payload, self.buffer = read_exact(self.sock, self.buffer, length)
            if mask:
                payload = bytes(
                    byte ^ mask[index % 4] for index, byte in enumerate(payload)
                )
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

    def wait(self, job_id, timeout=120):
        deadline = time.monotonic() + timeout
        while True:
            for at, event in self.events:
                if event.get("jobId") == job_id and event.get("status") in (
                    "done",
                    "failed",
                    "cancelled",
                ):
                    return at, event
            if time.monotonic() > deadline:
                raise SystemExit(f"job {job_id} did not finish")
            self.message()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def spooler_ids():
    out = subprocess.run(
        ["lpstat", "-W", "all", "-o", queue], capture_output=True, text=True
    ).stdout
    return {line.split()[0] for line in out.splitlines() if line.strip()}


def active_ids():
    out = subprocess.run(
        ["lpstat", "-W", "not-completed", "-o", queue], capture_output=True, text=True
    ).stdout
    return {line.split()[0] for line in out.splitlines() if line.strip()}


session = Session()
info = session.call("sys.info")
assert info["ok"], info
capabilities = info["result"]["capabilities"]
expected_keys = {
    "duplex",
    "paperSource",
    "cancelSubmitted",
    "cancelRaw",
    "preview",
    "rasterFallback",
    "raw",
    "batch",
    "pdfPassthrough",
    "renderWorkers",
}
assert set(capabilities) == expected_keys, capabilities
assert capabilities == {
    "duplex": False,
    "paperSource": False,
    "cancelSubmitted": True,
    "cancelRaw": True,
    "preview": gui_session_available(),
    "rasterFallback": False,
    "raw": True,
    "batch": True,
    "pdfPassthrough": True,
    "renderWorkers": 1,
}, capabilities
print("sys.info capabilities:", json.dumps(capabilities, sort_keys=True))

before_pdf = spooler_ids()
pdf = base64.b64encode(open(f"{work}/1000-pages.pdf", "rb").read()).decode()
started = time.monotonic()
reply = session.call("doc.printPdf", {"printer": queue, "bytes": pdf})
assert reply["ok"], reply
job_id = reply["result"]["jobId"]
finished, event = session.wait(job_id)
elapsed = finished - started
assert event["status"] == "done", event
assert elapsed <= 5.0, f"printPdf took {elapsed:.3f}s"
print(f"doc.printPdf: 1000 pages in {elapsed:.3f}s, job {job_id} done")
deadline = time.monotonic() + 30
while len(spooler_ids() - before_pdf) < 1:
    if time.monotonic() > deadline:
        raise SystemExit("the printPdf job never reached the spooler")
    time.sleep(0.05)
before = spooler_ids()

subprocess.run(["cupsdisable", queue], check=True, capture_output=True)
before = active_ids()
reply = session.call(
    "doc.print",
    {**doc, "output": {"printer": {"name": queue}, "copies": 1}},
)
assert reply["ok"], reply
cancel_job = reply["result"]["jobId"]
deadline = time.monotonic() + 30
while True:
    after = active_ids()
    added = after - before
    if added:
        break
    if time.monotonic() > deadline:
        raise SystemExit("the cancel candidate never reached the spooler")
    time.sleep(0.05)
cancel_result = session.call("job.cancel", {"jobId": cancel_job})
assert cancel_result["ok"], cancel_result
assert cancel_result["result"] == {}, cancel_result
time.sleep(0.2)
remaining = active_ids() & added
assert not remaining, f"cancelled job still queued: {remaining}"
print(f"job.cancel: cupsCancelJob retracted {sorted(added)} and returned {{}}")
subprocess.run(["cupsenable", queue], check=True, capture_output=True)
session.close()

# --- POST /print/batch -----------------------------------------------------

boundary = "PRINTBOUNDARY"
pdf_part = open(f"{work}/1000-pages.pdf", "rb").read()


def multipart(parts):
    body = bytearray()
    for content_type, payload in parts:
        body += f"--{boundary}\r\nContent-Type: {content_type}\r\n\r\n".encode()
        body += payload + b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    return bytes(body)


def post(body, token_value):
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/print/batch",
        data=body,
        method="POST",
        headers={
            "Content-Type": f"multipart/mixed; boundary={boundary}",
            "Authorization": f"Bearer {token_value}" if token_value else "",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


meta = json.dumps({"printer": queue, "copies": 1}).encode()
document = json.dumps(doc).encode()
tokenless_before = spooler_ids()
status, reply = post(
    multipart([("application/json", meta), ("application/json", document)]), ""
)
assert status == 200 and reply["ok"] and reply["jobId"], (status, reply)
deadline = time.monotonic() + 30
while spooler_ids() == tokenless_before:
    if time.monotonic() > deadline:
        raise SystemExit("the tokenless batch never reached the spooler")
    time.sleep(0.05)
print("POST /print/batch without a token: print scope")

before = spooler_ids()
status, reply = post(
    multipart(
        [
            ("application/json", meta),
            ("application/pdf", pdf_part),
            ("application/json", document),
        ]
    ),
    token,
)
assert status == 200 and reply["ok"] and reply["jobId"], (status, reply)
job_id = reply["jobId"]
print("POST /print/batch:", json.dumps(reply, sort_keys=True))
deadline = time.monotonic() + 30
while True:
    after = spooler_ids()
    added = after - before
    if added:
        break
    if time.monotonic() > deadline:
        raise SystemExit("the HTTP batch never reached the spooler")
    time.sleep(0.05)
assert len(added) == 1, f"one batch produced {len(added)} spooler jobs"
print("POST /print/batch produced one spooler job:", sorted(added))
PY

# --- oversized request -----------------------------------------------------

before_jobs=$(spooler_ids | wc -l | tr -d ' ')
truncate -s $((256 * 1024 * 1024 + 1)) "$work/too-big.bin"
status=$(curl -sS -o "$work/too-big.json" -w '%{http_code}' \
  -H "Authorization: Bearer $token" \
  -H 'Content-Type: multipart/mixed; boundary=PRINTBOUNDARY' \
  --data-binary @"$work/too-big.bin" "$base/print/batch")
cat "$work/too-big.json"
[[ $status == 413 ]] || fail "an oversized batch returned $status"
jq -e '.error.code == "E_TOO_LARGE"' "$work/too-big.json" >/dev/null \
  || fail "an oversized batch did not fail with E_TOO_LARGE"
after_jobs=$(spooler_ids | wc -l | tr -d ' ')
[[ $before_jobs == "$after_jobs" ]] || fail "an oversized batch created a spool job"
if find "$data" -type f -size +1M | grep -q .; then
  fail "an oversized batch left a file on disk"
fi
printf 'oversized request: 413 E_TOO_LARGE, no spool job, no file above 1 MiB\n'

# --- zero-render trace for printPdf ---------------------------------------

"$bin" --data-dir "$data" logs --json \
  | jq -e --argjson bytes "$pdf_bytes" '
      group_by(.trace_id)
      | map(select(any(.[]; .stage == "recv" and .fields.bytes == $bytes)))
      | length == 1
        and (.[0] | map(.stage) | index("render") | not)
        and (.[0] | map(.stage) | index("submit") | type == "number")
    ' >/dev/null || fail "the printPdf trace has a render stage or no submit"
printf 'printPdf trace: recv/validate/submit/result, no render stage\n'

printf 'PASS: virtual IPP submission, capabilities, printPdf, cancellation, batch, HTTP batch\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
