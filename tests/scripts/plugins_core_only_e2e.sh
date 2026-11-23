#!/usr/bin/env bash
# Core-only hard criteria: all optional plugins off, printing still works,
# observability methods/routes/CLI are unavailable, no rows or log files grow.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"

cups_test_lock_acquire \
  || { printf 'FAIL: could not acquire the CUPS test lock within %ss\n' "${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-180}" >&2; exit 1; }
trap 'cups_test_lock_release' EXIT
target_dir=${PRINTKIT_FAKE_TARGET_DIR:-$root/target/fake-render}
bin=${PRINTKIT_BIN:-$target_dir/debug/printkit}
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  # Fake-render builds live outside target/debug so they never replace the
  # binary the CUPS acceptance scripts execute.
  CARGO_TARGET_DIR="$target_dir" cargo build -q -p printkit --features serve,fake-render
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
serve_pid=
cleanup() {
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$data"
  cups_test_lock_release
}
trap cleanup EXIT
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$1"; }

printf 'version = 1\npreset = "core"\n' >"$data/plugins.toml"

# Script-token bootstrap.
"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer"
admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "no CSRF cookie"
post() {
  local path=$1 body=$2
  curl -sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" -d "$body" "$base$path"
}
[[ $(post /admin/login "{\"token\":\"$admin\"}") == 200 ]] || fail "admin login failed"
[[ $(post /admin/origins '{"origin":"https://core.test"}') == 200 ]] || fail "origin add failed"
[[ $(post /admin/tokens '{"origin":"https://core.test","admin":true,"nonBrowser":true}') == 201 ]] \
  || fail "token issue failed"
token=$(jq -r .token "$data/body.json")

# --- core printing and CLI ---------------------------------------------------
"$bin" pdf --data-dir "$data" -i tests/fixtures/minimal.json -o "$data/minimal.pdf" >/dev/null
[[ -s "$data/minimal.pdf" ]] || fail "pdf produced no file"

print_params=$(jq -c '. + {requestId:"core-print"}' tests/fixtures/minimal.json)
python3 tests/scripts/plugins_client.py "$port" "$token" doc.print "$print_params" >"$data/print.json"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/print.json" >/dev/null \
  || fail "doc.print failed with all plugins off"
batch_params=$(jq -c '{docs:[.], printer:"fake-printer", copies:1, requestId:"core-batch"}' tests/fixtures/minimal.json)
python3 tests/scripts/plugins_client.py "$port" "$token" doc.printBatch "$batch_params" >"$data/batch.json"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/batch.json" >/dev/null \
  || fail "doc.printBatch failed with all plugins off"
pdf_b64=$(base64 <"$data/minimal.pdf" | tr -d '\n')
pdf_params=$(jq -cn --arg bytes "$pdf_b64" '{bytes:$bytes, printer:"fake-printer", copies:1}')
python3 tests/scripts/plugins_client.py "$port" "$token" doc.printPdf "$pdf_params" >"$data/printpdf.json"
jq -e '.response.ok == true and (.response.result.jobId | type == "string")' "$data/printpdf.json" >/dev/null \
  || fail "doc.printPdf failed with all plugins off"
python3 tests/scripts/plugins_client.py "$port" "$token" raw.send '{"printer":"fake-printer","bytes":"AAE="}' >"$data/raw.json"
# fake-printer advertises no raw capability, so the concrete refusal proves
# the method reached the printer check instead of being gated by a plugin.
jq -e '.response.ok == false and .response.error.code == "E_RAW_UNSUPPORTED"' \
  "$data/raw.json" >/dev/null || fail "raw.send did not fail with E_RAW_UNSUPPORTED"
steps=$(jq -cn --argjson doc "$(cat tests/fixtures/minimal.json)" '[
  {method:"doc.preview", params:$doc},
  {method:"preview.pages", params:{windowId:"$window", pageIndex:0, dpi:96}},
  {method:"preview.close", params:{windowId:"$window"}}
]')
python3 tests/scripts/plugins_client.py "$port" "$token" sequence "$steps" >"$data/previews.json"
jq -e '.responses[0].ok == true and (.responses[0].result.windowId | type == "string")
  and .responses[1].ok == true and (.responses[1].result.png | length > 0)
  and .responses[2].ok == true' "$data/previews.json" >/dev/null \
  || fail "preview lifecycle failed with all plugins off"
export_params=$(jq -c '{document: ., path:"core/out.pdf"}' tests/fixtures/minimal.json)
python3 tests/scripts/plugins_client.py "$port" "$token" doc.exportPdf "$export_params" >"$data/export.json"
jq -e '.response.ok == true and (.response.result.url | startswith("http://127.0.0.1:"))' "$data/export.json" >/dev/null \
  || fail "doc.exportPdf failed with all plugins off or returned no signed URL"
[[ -s "$data/exports/core/out.pdf" ]] || fail "doc.exportPdf wrote no file"
python3 tests/scripts/plugins_client.py "$port" "$token" printers.list null >"$data/printers.json"
jq -e '.response.ok == true and (.response.result | length >= 1)' "$data/printers.json" >/dev/null \
  || fail "printers.list failed with all plugins off"
python3 tests/scripts/plugins_client.py "$port" "$token" printers.papers \
  '{"name":"fake-printer"}' >"$data/papers.json"
jq -e '.response.ok == true and (.response.result | length >= 1)' "$data/papers.json" >/dev/null \
  || fail "printers.papers failed with all plugins off"
export_url=$(jq -r '.response.result.url' "$data/export.json")
exported=$(curl -sS -o "$data/exported.pdf" -w '%{http_code}' "$export_url")
[[ $exported == 200 ]] || fail "/exports served a signed core export with $exported"
cmp -s "$data/exported.pdf" "$data/exports/core/out.pdf" || fail "/exports returned different bytes"
unsigned=$(curl -sS -o "$data/unsigned.json" -w '%{http_code}' "$base/exports/core/out.pdf")
[[ $unsigned == 403 ]] || fail "unsigned /exports returned $unsigned"
jq -e '.error.code == "E_AUTH"' "$data/unsigned.json" >/dev/null \
  || fail "unsigned /exports body is not E_AUTH"
ok "doc.print/printBatch/printPdf/raw/preview/exportPdf stay available"

# HTTP /print/batch uses the same print scope.
python3 - "$port" "$token" "$data" <<'PY'
import json, sys
from pathlib import Path
import urllib.request, urllib.error

port, token, data = int(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
boundary = "COREBOUNDARY"
document = json.loads((Path("tests/fixtures/minimal.json")).read_text())

def part(content_type, payload):
    return f"--{boundary}\r\nContent-Type: {content_type}\r\n\r\n".encode() + payload + b"\r\n"

body = (
    part("application/json", json.dumps({"printer": "fake-printer", "copies": 1}).encode())
    + part("application/json", json.dumps(document).encode())
    + f"--{boundary}--\r\n".encode()
)
request = urllib.request.Request(
    f"http://127.0.0.1:{port}/print/batch",
    data=body,
    method="POST",
    headers={
        "Content-Type": f"multipart/mixed; boundary={boundary}",
        "Authorization": f"Bearer {token}",
    },
)
try:
    with urllib.request.urlopen(request, timeout=30) as response:
        reply = json.load(response)
        assert response.status == 200 and reply["ok"] and reply["jobId"], reply
except urllib.error.HTTPError as error:
    raise SystemExit(f"/print/batch failed: {error.code} {error.read()!r}")
PY
ok "HTTP /print/batch stays available"
set +e
"$bin" logs --data-dir "$data" --json >"$data/logs.out" 2>"$data/logs.err"
logs_rc=$?
"$bin" stats --data-dir "$data" --json >"$data/stats.out" 2>"$data/stats.err"
stats_rc=$?
set -e
[[ $logs_rc == 4 ]] || fail "printkit logs exited $logs_rc (want 4)"
[[ $stats_rc == 4 ]] || fail "printkit stats exited $stats_rc (want 4)"
grep -q E_UNSUPPORTED "$data/logs.err" \
  || fail "printkit logs did not report E_UNSUPPORTED"
grep -q E_UNSUPPORTED "$data/stats.err" \
  || fail "printkit stats did not report E_UNSUPPORTED"
ok "pdf works and logs/stats exit 4 with all plugins off"

# --- rows and log files do not grow ------------------------------------------
[[ -f $data/printkit.db ]] || fail "printkit.db is missing"
[[ -s $data/printkit.db ]] || fail "printkit.db is empty"
rows_before=$(python3 - "$data/printkit.db" <<'PY'
import sqlite3, sys, os
path = sys.argv[1]
if not os.path.exists(path) or os.path.getsize(path) == 0:
    raise SystemExit(f"database missing or empty: {path}")
db = sqlite3.connect(path)
tables = db.execute(
    "select count(*) from sqlite_master where type='table' and name='print_record'"
).fetchone()[0]
if tables != 1:
    raise SystemExit("print_record table is missing")
print(db.execute("select count(*) from print_record").fetchone()[0])
PY
) || fail "cannot inspect printkit.db"
# The records plugin is off, so prints must not add usage rows; the database
# itself must still exist with its schema.
"$bin" pdf --data-dir "$data" -i tests/fixtures/minimal.json -o "$data/minimal2.pdf" >/dev/null
rows_after=$(python3 - "$data/printkit.db" <<'PY'
import sqlite3, sys, os
path = sys.argv[1]
if not os.path.exists(path):
    raise SystemExit(f"database missing: {path}")
print(sqlite3.connect(path).execute("select count(*) from print_record").fetchone()[0])
PY
) || fail "cannot read print_record after the second pdf"
[[ $rows_before == "$rows_after" ]] || fail "print_record grew from $rows_before to $rows_after"
find "$data/logs" -name '*.jsonl' -newer "$data/plugins.toml" -print -quit 2>/dev/null | grep -q . \
  && fail "observability wrote a JSONL file while off"
ok "no print_record rows or JSONL files while observability is off"

# --- protocol/HTTP gates -----------------------------------------------------
python3 tests/scripts/plugins_client.py "$port" "$token" welcome >"$data/welcome.json"
jq -e '.welcome.capabilities == [] and .welcome.pluginFamilies == []' \
  "$data/welcome.json" >/dev/null || fail "core-only welcome still advertises a family"
python3 tests/scripts/plugins_client.py "$port" "$token" records.list null >"$data/records.json"
jq -e '.response.error.code == "E_UNSUPPORTED"
  and .response.error.detail.plugin == "records"' "$data/records.json" >/dev/null \
  || fail "records.list is not E_UNSUPPORTED"
unStatus=$(curl -sS -o "$data/unauth.json" -w '%{http_code}' "$base/records/list")
[[ $unStatus == 404 ]] || fail "unauthenticated records returned $unStatus"
jq -e '.error.code == "E_NOT_FOUND"' "$data/unauth.json" >/dev/null \
  || fail "unauthenticated records did not return E_NOT_FOUND"
authStatus=$(curl -sS -o "$data/rec.json" -w '%{http_code}' -b "$jar" "$base/records/list")
[[ $authStatus == 404 ]] || fail "authenticated records returned $authStatus"
jq -e '.error.code == "E_UNSUPPORTED"' "$data/rec.json" >/dev/null \
  || fail "authenticated records did not return E_UNSUPPORTED"
curl -fsS -b "$jar" "$base/admin/plugins" >"$data/plugins.json"
jq -e '[.plugins[] | select(.state == "on")] | length == 0' "$data/plugins.json" >/dev/null \
  || fail "some plugin is on under preset core"
ok "WS methods, HTTP routes and admin list all report core-only"
echo "plugins_core_only_e2e: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
