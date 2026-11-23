#!/usr/bin/env bash
# Plugin P0 e2e: admin API, lease/inUse, 404/E_UNSUPPORTED, capabilities,
# sequence continuity and data retention.
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

# Start with observability off so the sink seeds from an existing JSONL file.
mkdir -p "$data/logs"
printf 'version = 1\npreset = "core"\n' >"$data/plugins.toml"
today=$(date -u +%Y-%m-%d)
seed_file="$data/logs/printkit-$today.jsonl"
printf '%s\n' '{"seq":100,"trace_id":"seed","stage":"recv","at":"2026-11-11T00:00:00Z","ok":true}' >"$seed_file"
seed_hash=$(shasum -a 256 "$seed_file" | awk '{print $1}')
seed_size=$(wc -c <"$seed_file")

PRINTKIT_PLUGIN_LEASE_TTL_SECS=2 "$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer"

[[ $(shasum -a 256 "$seed_file" | awk '{print $1}') == "$seed_hash" ]] \
  || fail "observability-off startup changed the JSONL file"
[[ $(wc -c <"$seed_file") == "$seed_size" ]] || fail "JSONL size changed"
[[ ! -e "$seed_file.gz" ]] || fail "observability-off startup compressed the log"
ok "startup with observability off seeds without rotating"

# --- administrator bootstrap -------------------------------------------------
admin=$(tr -d '\n' <"$data/admin-token")
curl -fsS -c "$jar" -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "no CSRF cookie"

admin_post() {
  local path=$1 body=$2
  local status
  status=$(curl -sS -o "$data/body.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" -d "$body" "$base$path")
  printf '%s' "$status"
}

status=$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" -c "$jar" \
  -H "Origin: $base" -H 'Content-Type: application/json' \
  -H "X-CSRF-Token: $csrf" -d "{\"token\":\"$admin\"}" "$base/admin/login")
[[ $status == 200 ]] || fail "admin login returned $status"
admin_post /admin/origins '{"origin":"https://plugins.test"}' >/dev/null
admin_post /admin/tokens '{"origin":"https://plugins.test","admin":true,"nonBrowser":true}' >/dev/null
token=$(jq -r .token "$data/body.json")
[[ -n $token && $token != null ]] || fail "no admin token issued"

# --- list --------------------------------------------------------------------
curl -fsS -b "$jar" "$base/admin/plugins" >"$data/plugins.json"
jq -e '.ok and (.plugins | length == 2)' "$data/plugins.json" >/dev/null \
  || fail "GET /admin/plugins did not list both plugins"
jq -e '.plugins[] | select(.id == "observability") | .state == "off" and .active == false' \
  "$data/plugins.json" >/dev/null || fail "observability was not off"
jq -e '.plugins[] | select(.id == "observability")
  | .permissions.bridge == {"selfConfig": false, "printers": false, "recordsSummary": false}' \
  "$data/plugins.json" >/dev/null || fail "GET /admin/plugins omitted the bridge permissions"
ok "GET /admin/plugins lists on/off state"

# --- enable observability and seed continues ---------------------------------
status=$(admin_post /admin/plugins '{"id":"observability","on":true}')
[[ $status == 200 ]] || fail "enabling observability returned $status"
status=$(admin_post /admin/plugins '{"id":"records","on":true}')
[[ $status == 200 ]] || fail "enabling records returned $status"
python3 tests/scripts/plugins_client.py "$port" bad-token welcome >"$data/bad.json" 2>/dev/null || true
sleep 0.3
events=$("$bin" logs --data-dir "$data" --json)
jq -e 'map(.seq) | max > 100' <<<"$events" >/dev/null \
  || fail "new event did not continue above the seeded seq"
jq -e 'map(.seq) | sort == .' <<<"$events" >/dev/null \
  || fail "new events are not in ascending seq order"
ok "observability off/on keeps seq above the existing floor"

# One fake print seeds a usage row; the HTTP list must expose nextCursor.
print_params=$(jq -c '. + {requestId:"e2e-records"}' tests/fixtures/minimal.json)
python3 tests/scripts/plugins_client.py "$port" "$token" doc.print "$print_params" >"$data/print.json"
jq -e '.response.ok == true' "$data/print.json" >/dev/null || fail "seed doc.print failed"
next_cursor=null
for _ in $(seq 1 50); do
  curl -fsS -b "$jar" "$base/records/list?limit=1" >"$data/page.json"
  next_cursor=$(jq -r '.nextCursor // null' "$data/page.json")
  [[ $next_cursor != null ]] && break
  sleep 0.1
done
[[ $next_cursor != null ]] || fail "records list never returned nextCursor"
cursor_page=$(curl -sS -b "$jar" "$base/records/list?limit=1&cursor=$next_cursor")
jq -e '.records | length == 0' <<<"$cursor_page" >/dev/null \
  || fail "cursor page was not empty after the last row"
ok "HTTP records list returns nextCursor for load-more"

python3 tests/scripts/plugins_client.py "$port" "$token" records.summary null >"$data/summary.json"
jq -e '.response.ok == true and .response.result.total >= 1' "$data/summary.json" >/dev/null \
  || fail "records.summary did not see the row"
python3 tests/scripts/plugins_client.py "$port" "$token" records.stats \
  '{"groupBy":"printer"}' >"$data/stats.json"
jq -e '.response.ok == true and (.response.result.buckets | length) >= 1' "$data/stats.json" >/dev/null \
  || fail "records.stats did not see the row"
summary_http=$(curl -fsS -b "$jar" "$base/records/summary")
jq -e '.total >= 1' <<<"$summary_http" >/dev/null || fail "HTTP records summary did not see the row"
summary_printer=$(curl -fsS -b "$jar" "$base/records/summary?printer=fake-printer")
jq -e '.total >= 1 and .failed == 0' <<<"$summary_printer" >/dev/null \
  || fail "summary printer filter did not match the printed row"
summary_status=$(curl -fsS -b "$jar" "$base/records/summary?status=failed")
jq -e '.total == 0' <<<"$summary_status" >/dev/null \
  || fail "summary status=failed leaked done rows"
summary_future=$(curl -fsS -b "$jar" "$base/records/summary?from=2099-01-01")
jq -e '.total == 0' <<<"$summary_future" >/dev/null \
  || fail "summary from filter did not exclude old rows"
summary_past=$(curl -fsS -b "$jar" "$base/records/summary?to=2000-01-01")
jq -e '.total == 0' <<<"$summary_past" >/dev/null \
  || fail "summary to filter did not exclude new rows"
summary_tag=$(curl -fsS -b "$jar" "$base/records/summary?tags=no-such-tag")
jq -e '.total == 0' <<<"$summary_tag" >/dev/null \
  || fail "summary tag filter was ignored"
ok "records.list/stats/summary and summary printer/status/tag/from/to filters work through the plugin domain"
curl -fsS -b "$jar" "$base/admin/plugins" >"$data/records-active.json"
jq -e '.plugins[]
  | select(.id == "records")
  | .requiresServices == ["records.store", "records.usage"]
    and (.usageDropped.droppedAfterClose | type == "number")
    and (.usageDropped.droppedFull | type == "number")'   "$data/records-active.json" >/dev/null \
  || fail "records admin view does not expose service requirements and drop counters"
ok "admin view exposes requiresServices and usage drop counters"

# --- observability methods while on -----------------------------------------
python3 tests/scripts/plugins_client.py "$port" "$token" trace.subscribe null >"$data/subscribe.json"
jq -e '.response.ok == true' "$data/subscribe.json" >/dev/null \
  || fail "trace.subscribe failed while observability was on"
status=$(admin_post /admin/logs/export '{}')
[[ $status == 200 ]] || fail "logs.export returned $status while on"
[[ -s "$data/body.json" ]] || fail "logs.export returned an empty archive"
[[ $(python3 -c 'import sys; print(open(sys.argv[1], "rb").read(2), end="")' "$data/body.json") == "b'PK'" ]] \
  || fail "logs.export body is not a zip archive"

# --- lease blocks disabling --------------------------------------------------
status=$(admin_post /admin/plugin-lease '{"id":"observability","op":"acquire","leaseId":"desk-1"}')
[[ $status == 200 ]] || fail "lease acquire returned $status"
jq -e '.inUse == 1' "$data/body.json" >/dev/null || fail "lease did not count inUse"
status=$(admin_post /admin/plugins '{"id":"observability","on":false,"confirmInUse":true}')
[[ $status == 409 ]] || fail "confirmInUse bypassed a live lease ($status)"
jq -e '.error.code == "E_UNSUPPORTED" and .error.detail.reason == "in_use"' "$data/body.json" >/dev/null \
  || fail "disable conflict body is not in_use"
status=$(admin_post /admin/plugin-lease '{"id":"observability","op":"release","leaseId":"desk-1"}')
[[ $status == 200 ]] || fail "lease release returned $status"
status=$(admin_post /admin/plugin-lease '{"id":"observability","op":"acquire","leaseId":"desk-ttl"}')
[[ $status == 200 ]] || fail "TTL lease acquire returned $status"
sleep 3
latest=$("$bin" logs --data-dir "$data" --json | jq 'map(.seq) | max // 1')
python3 tests/scripts/plugins_client.py "$port" "$token" hold \
  "{\"since\":$latest,\"ready\":\"$data/trace-ready\",\"flag\":\"$data/trace-flag\"}" \
  >"$data/trace-hold.json" &
trace_pid=$!
for _ in $(seq 1 100); do
  [[ -e "$data/trace-ready" ]] && break
  sleep 0.05
done
[[ -e "$data/trace-ready" ]] || fail "trace subscription never started"
status=$(admin_post /admin/plugins '{"id":"observability","on":false}')
[[ $status == 409 ]] || fail "live trace subscription did not block disable ($status)"
jq -e '.error.code == "E_UNSUPPORTED" and .error.detail.reason == "in_use"' "$data/body.json" >/dev/null \
  || fail "trace subscription conflict is not in_use"
curl -fsS -b "$jar" "$base/admin/plugins" >"$data/plugins.json"
jq -e '.plugins[] | select(.id == "observability") | .inUseDetail.ws >= 1' "$data/plugins.json" >/dev/null \
  || fail "inUseDetail.ws did not count the live subscription"
touch "$data/trace-flag"
wait "$trace_pid" || fail "trace subscription client failed"
# The socket close is handled on the connection task; give it one tick to
# decrement the subscription count before the disable request.
sleep 0.5
status=$(admin_post /admin/plugins '{"id":"observability","on":false,"confirmDataCount":true}')
[[ $status == 200 ]] || fail "disable after subscriptions closed returned $status"
status=$(admin_post /admin/plugins '{"id":"records","on":false,"confirmDataCount":true}')
[[ $status == 200 ]] || fail "disabling records returned $status"
ok "live trace subscriptions block disable; after close and lease TTL expiry disable succeeds"

# --- 404 gating --------------------------------------------------------------
unauth_status=$(curl -sS -o "$data/unauth.json" -w '%{http_code}' "$base/records/list")
[[ $unauth_status == 404 ]] || fail "unauthenticated /records/list returned $unauth_status"
jq -e '.error.code == "E_NOT_FOUND"' "$data/unauth.json" >/dev/null \
  || fail "unauthenticated records body leaked E_UNSUPPORTED"
auth_status=$(curl -sS -o "$data/logs.json" -w '%{http_code}' -b "$jar" "$base/admin/logs/events")
[[ $auth_status == 404 ]] || fail "disabled /admin/logs/events returned $auth_status"
jq -e '.error.code == "E_UNSUPPORTED" and .error.detail.plugin == "observability"' \
  "$data/logs.json" >/dev/null || fail "disabled logs body is not E_UNSUPPORTED"
python3 tests/scripts/plugins_client.py "$port" "$token" records.list null >"$data/records.json"
jq -e '.response.error.code == "E_UNSUPPORTED"
  and .response.error.detail.plugin == "records"' "$data/records.json" >/dev/null \
  || fail "records.list was not E_UNSUPPORTED"
for method in trace.subscribe trace.tail logs.export; do
  if [[ $method == trace.tail ]]; then
    params='{"traceId":"missing"}'
  else
    params='null'
  fi
  python3 tests/scripts/plugins_client.py "$port" "$token" "$method" "$params" >"$data/$method.json"
  jq -e --arg method "$method" '.response.error.code == "E_UNSUPPORTED"
    and .response.error.detail.plugin == "observability"' "$data/$method.json" >/dev/null \
    || fail "$method was available while observability was off"
done
export_status=$(admin_post /admin/logs/export '{}')
[[ $export_status == 404 ]] || fail "disabled POST /admin/logs/export returned $export_status"
jq -e '.error.code == "E_UNSUPPORTED" and .error.detail.plugin == "observability"' \
  "$data/body.json" >/dev/null || fail "disabled export route is not E_UNSUPPORTED"
ok "trace.subscribe/tail/logs.export and export route are gated while off"
ok "disabled plugin routes return 404 + E_UNSUPPORTED (unauth: E_NOT_FOUND)"

# --- unknown id and presets --------------------------------------------------
status=$(admin_post /admin/plugins '{"id":"no-such-plugin","on":true}')
[[ $status == 404 ]] || fail "unknown plugin returned $status"
jq -e '.error.code == "E_NOT_FOUND"' "$data/body.json" >/dev/null \
  || fail "unknown plugin body is not E_NOT_FOUND"
status=$(admin_post /admin/plugins '{"preset":"all","confirmDataCount":true}')
[[ $status == 200 ]] || fail "preset all returned $status"
jq -e '.preset == "all"
  and (.plugins[] | select(.id == "observability") | .state == "off")
  and (.plugins[] | select(.id == "records") | .state == "off")
  and ([.plugins[] | select(.state == "on")] | length == 0)' \
  "$data/body.json" >/dev/null || fail "preset all did not preserve explicit rows"
status=$(admin_post /admin/plugins '{"preset":"core","confirmDataCount":true}')
[[ $status == 200 ]] || fail "preset core returned $status"
jq -e '.preset == "core" and ([.plugins[] | select(.state == "on")] | length == 0)' \
  "$data/body.json" >/dev/null || fail "preset core did not apply to row-less plugins"
ok "preset all/core applies one preflighted write"

echo "plugins_e2e: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
