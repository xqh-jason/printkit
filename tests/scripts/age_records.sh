#!/usr/bin/env bash
# Retention acceptance: the 90-day and 5000-row branches of `print_record`,
# and the 90-day archive branch of the JSONL files.
#
# Expiry is driven through the real startup path: `serve` trims the table and
# rotates the log directory before it binds the port.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
data=$(mktemp -d)
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

free_port() {
  python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
PY
}

# Start, wait until the router answers, then stop. Each run performs one
# cleanup pass and one log rotation.
run_serve_once() {
  local port
  port=$(free_port)
  "$bin" --data-dir "$data" --port "$port" serve >"$data/serve.log" 2>&1 &
  serve_pid=$!
  for _ in $(seq 1 100); do
    if curl -fsS -o /dev/null "http://127.0.0.1:$port/admin/session" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  kill "$serve_pid" 2>/dev/null || true
  wait "$serve_pid" 2>/dev/null || true
  serve_pid=
}

db="$data/printkit.db"
# Records/log retention is gated by their plugins; opt in for this cleanup test.
printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$data/plugins.toml"

# First run creates and migrates the database.
run_serve_once
[[ -f $db ]] || fail "serve did not create $db"

# --- 90-day branch ---------------------------------------------------------
sqlite3 "$db" <<'SQL'
DELETE FROM print_record;
INSERT INTO print_record
  (trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code)
VALUES
  ('old-1', 1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-100 days'), 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL),
  ('old-2', 2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-100 days'), 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL),
  ('fresh-1', 3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-1 days'), 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL),
  ('fresh-2', 4, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL);
SQL
run_serve_once
after_expiry=$(sqlite3 "$db" "SELECT count(*) FROM print_record")
[[ $after_expiry == 2 ]] || fail "expiry left $after_expiry rows, expected 2"
printf 'expiry: %s row(s) left after the 90-day cut\n' "$after_expiry"

# --- 5000-row branch -------------------------------------------------------
python3 - "$db" <<'PY'
import datetime
import sqlite3
import sys

stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
connection = sqlite3.connect(sys.argv[1])
connection.execute("DELETE FROM print_record")
connection.executemany(
    "INSERT INTO print_record (trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code)"
    " VALUES (?, ?, ?, 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL)",
    [
        (f"overflow-{index}", index, stamp)
        for index in range(5_001)
    ],
)
connection.commit()
PY
run_serve_once
after_overflow=$(sqlite3 "$db" "SELECT count(*) FROM print_record")
[[ $after_overflow == 5000 ]] || fail "row limit left $after_overflow rows, expected 5000"
printf 'row limit: %s row(s) left after inserting 5001\n' "$after_overflow"

# --- JSONL 90-day branch ---------------------------------------------------
mkdir -p "$data/logs"
recent=$(python3 - <<'PY'
import datetime
print((datetime.datetime.now(datetime.timezone.utc).date() - datetime.timedelta(days=2)).isoformat())
PY
)
old=$(python3 - <<'PY'
import datetime
print((datetime.datetime.now(datetime.timezone.utc).date() - datetime.timedelta(days=100)).isoformat())
PY
)
printf '{"seq":1}\n' >"$data/logs/printkit-$recent.jsonl"
printf '{"seq":2}\n' >"$data/logs/printkit-$old.jsonl"
run_serve_once
[[ -f "$data/logs/printkit-$recent.jsonl.gz" ]] || fail "the finished day was not gzipped"
[[ ! -f "$data/logs/printkit-$old.jsonl" && ! -f "$data/logs/printkit-$old.jsonl.gz" ]] \
  || fail "a file older than 90 days survived"
printf 'archive: %s gzipped, %s removed\n' "$recent" "$old"

live_data=$(mktemp -d)
printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$live_data/plugins.toml"
live_port=$(free_port)
PRINTKIT_RECORDS_SWEEP_SECS=1 "$bin" --data-dir "$live_data" --port "$live_port" serve \
  >"$live_data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$live_port/admin/session" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
sleep 0.3
sqlite3 "$live_data/printkit.db" \
  "INSERT INTO print_record (trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code) \
   VALUES ('live-old', 1, '2000-01-01T00:00:00Z', 'https://age.example', NULL, '[]', NULL, 1, 1, 'done', 5, NULL)"
sleep 2.5
live_rows=$(sqlite3 "$live_data/printkit.db" "SELECT count(*) FROM print_record WHERE trace_id='live-old'")
[[ $live_rows == 0 ]] || fail "running sweep left $live_rows expired row(s)"
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=
rm -rf "$live_data"
printf 'running sweep: expired row removed while serve stayed up\n'

printf 'retention: OK\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
