#!/usr/bin/env bash
# S2 regression: template storage and the /admin/templates surface are gone.
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

"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$base/admin/session" 2>/dev/null && break
  sleep 0.1
done
curl -fsS -o /dev/null "$base/admin/session" || fail "serve did not answer"

version=$(sqlite3 "$data/printkit.db" 'PRAGMA user_version')
[[ $version == 11 ]] || fail "user_version is $version, expected 11"
[[ $(sqlite3 "$data/printkit.db" "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='template'") == 0 ]] \
  || fail "template table still exists in a new database"
ok "new database is version 11 without a template table"

status=$(curl -sS -o "$data/route.json" -w '%{http_code}' "$base/admin/templates")
[[ $status == 404 ]] || fail "unauthenticated /admin/templates returned $status"
jq -e '.error.code == "E_NOT_FOUND"' "$data/route.json" >/dev/null \
  || fail "removed /admin/templates body is not E_NOT_FOUND"

jar="$data/cookies.txt"
curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
admin=$(tr -d '\n' <"$data/admin-token")
status=$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" -c "$jar" \
  -H "Origin: $base" -H 'Content-Type: application/json' \
  -H "X-CSRF-Token: $csrf" -d "{\"token\":\"$admin\"}" "$base/admin/login")
[[ $status == 200 ]] || fail "admin login returned $status"
status=$(curl -sS -o "$data/authed.json" -w '%{http_code}' -b "$jar" "$base/admin/templates")
[[ $status == 404 ]] || fail "authenticated /admin/templates returned $status"
jq -e '.error.code == "E_NOT_FOUND"' "$data/authed.json" >/dev/null \
  || fail "authenticated /admin/templates body is not E_NOT_FOUND"
ok "/admin/templates is absent for authenticated and anonymous callers"

admin_post() {
  local path=$1 body=$2
  curl -sS -o "$data/post.json" -w '%{http_code}' -b "$jar" -c "$jar" \
    -H "Origin: $base" -H 'Content-Type: application/json' \
    -H "X-CSRF-Token: $csrf" -d "$body" "$base$path"
}
[[ $(admin_post /admin/origins '{"origin":"https://removed.test"}') == 200 ]] \
  || fail "origin add failed"
[[ $(admin_post /admin/tokens '{"origin":"https://removed.test","admin":true,"nonBrowser":true}') == 201 ]] \
  || fail "token issue failed"
token=$(jq -r .token "$data/post.json")

for method in template.save template.load template.list; do
  case $method in
    template.save) params=$(jq -c '{name:"gone",document:.}' tests/fixtures/minimal.json) ;;
    template.load) params='{"templateId":"tpl-1"}' ;;
    *) params='null' ;;
  esac
  python3 tests/scripts/plugins_client.py "$port" "$token" "$method" "$params" >"$data/$method.json"
  jq -e '.response.error.code == "E_UNSUPPORTED"' "$data/$method.json" >/dev/null \
    || fail "$method was not E_UNSUPPORTED"
done

for path in /admin/templates/42; do
  status=$(curl -sS -o "$data/path.json" -w '%{http_code}' -b "$jar" "$base$path")
  [[ $status == 404 ]] || fail "GET $path returned $status"
  jq -e '.error.code == "E_NOT_FOUND"' "$data/path.json" >/dev/null \
    || fail "GET $path body is not E_NOT_FOUND"
done
status=$(curl -sS -o "$data/path.json" -w '%{http_code}' -X DELETE -b "$jar" "$base/admin/templates/42")
[[ $status == 404 ]] || fail "DELETE /admin/templates/42 returned $status"
status=$(admin_post /admin/templates '{}')
[[ $status == 404 ]] || fail "POST /admin/templates returned $status"
jq -e '.error.code == "E_NOT_FOUND"' "$data/post.json" >/dev/null \
  || fail "POST /admin/templates body is not E_NOT_FOUND"

if grep -qE '"template(\.|")' packages/sdk-ts/src/methods.ts; then
  fail "SDK methods still list template.*"
fi
if grep -q 'Template' packages/sdk-ts/src/methods.ts; then
  fail "SDK methods still mention Template"
fi
ok "WS template methods and admin template paths are gone; SDK has no template methods"

echo "template_e2e: ok (removed feature regression)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
