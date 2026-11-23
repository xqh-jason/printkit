#!/usr/bin/env bash
# JSONL retention: observability off must not write, rotate, compress or delete;
# on must continue the seeded sequence and resume normal logging.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"
cups_test_lock_acquire \
  || { printf 'FAIL: could not acquire the CUPS test lock within %ss\n' "${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-180}" >&2; exit 1; }
trap 'cups_test_lock_release' EXIT
bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  cargo build -q -p printkit --features serve,fake-render
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

mkdir -p "$data/logs"
printf 'version = 1\npreset = "core"\n' >"$data/plugins.toml"
today=$(date -u +%Y-%m-%d)
old_day=$(date -u -v-40d +%Y-%m-%d 2>/dev/null || date -u -d '40 days ago' +%Y-%m-%d)
yesterday=$(date -u -v-1d +%Y-%m-%d 2>/dev/null || date -u -d '1 day ago' +%Y-%m-%d)
seed="$data/logs/printkit-$today.jsonl"
finished="$data/logs/printkit-$yesterday.jsonl"
expired="$data/logs/printkit-$old_day.jsonl.gz"
printf '%s\n' '{"seq":100,"trace_id":"seed","stage":"recv","at":"2026-11-11T00:00:00Z","ok":true}' >"$seed"
printf '%s\n' '{"seq":90,"trace_id":"finished","stage":"recv","at":"2026-11-11T00:00:00Z","ok":true}' >"$finished"
printf 'expired' >"$expired"
snapshot() {
  find "$data/logs" -type f -print0 | sort -z | xargs -0 shasum -a 256
}

before=$(snapshot)
before_mtime=$(stat -f '%m' "$seed" 2>/dev/null || stat -c '%Y' "$seed")
PRINTKIT_LOG=info "$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer"
sleep 0.3
after_off=$(snapshot)
[[ $before == "$after_off" ]] || fail "observability off changed the log directory"
after_mtime=$(stat -f '%m' "$seed" 2>/dev/null || stat -c '%Y' "$seed")
[[ $before_mtime == "$after_mtime" ]] || fail "observability off changed the seed file mtime"
gzip_count=$(find "$data/logs" -name '*.gz' | wc -l | tr -d ' ')
[[ $gzip_count == 1 ]] || fail "observability off created or deleted gzip files ($gzip_count)"
ok "off startup writes nothing, rotates nothing and keeps mtimes"

# Turn on, trigger one auth event, and verify continuity.
admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
post() {
  local path=$1 body=$2
  curl -sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" -d "$body" "$base$path"
}
[[ $(post /admin/login "{\"token\":\"$admin\"}") == 200 ]] || fail "admin login failed"
status=$(post /admin/plugins '{"id":"observability","on":true}')
[[ $status == 200 ]] || fail "enabling observability returned $status"
python3 tests/scripts/plugins_client.py "$port" bad-token welcome >/dev/null 2>&1 || true
sleep 0.3
events=$("$bin" logs --data-dir "$data" --json)
jq -e 'map(.seq) | max > 100' <<<"$events" >/dev/null || fail "sequence did not continue above 100"
jq -e 'map(.seq) as $s | ($s | sort) == $s and ($s | unique | length) == ($s | length)' \
  <<<"$events" >/dev/null || fail "JSONL is not ascending and unique"
jq -e 'map(select(.stage == "auth")) | length >= 1' <<<"$events" >/dev/null \
  || fail "no auth event was written after enabling observability"
ok "on resumes writing with a strictly larger, unique sequence"

echo "plugins_jsonl_retention: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
