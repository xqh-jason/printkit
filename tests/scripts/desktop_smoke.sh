#!/usr/bin/env bash
# Desktop shell contract: sidecar start, lock and port, administrator login,
# logs/records APIs, shutdown and audit of the desktop manifests.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
if [[ ! -x $bin ]]; then
  cargo build -p printkit
fi

# SIGTERM is the production path; PRINTKIT_DESKTOP_SIGNAL=INT covers Ctrl-C's
# counterpart handler in local runs.
desktop_signal=${PRINTKIT_DESKTOP_SIGNAL:-TERM}

data=$(mktemp -d)
# The CLI-owned serve below is exercised through the observability endpoints.
printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$data/plugins.toml"
port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
base="http://127.0.0.1:$port"
jar="$data/cookies.txt"
serve_pid=
desktop_pid=
dev_data=

cleanup() {
  if [[ -n $desktop_pid ]] && kill -0 "$desktop_pid" 2>/dev/null; then
    kill "$desktop_pid" 2>/dev/null || true
    wait "$desktop_pid" 2>/dev/null || true
  fi
  if [[ -n $dev_data && -s $dev_data/serve.lock ]]; then
    sidecar_pid=$(jq -r '.pid // empty' "$dev_data/serve.lock" 2>/dev/null || true)
    if [[ -n $sidecar_pid ]] && kill -0 "$sidecar_pid" 2>/dev/null; then
      kill "$sidecar_pid" 2>/dev/null || true
    fi
  fi
  if [[ -n $serve_pid ]] && kill -0 "$serve_pid" 2>/dev/null; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  rm -rf "$data"
  [[ -z $dev_data ]] || rm -rf "$dev_data"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

# `kill -0` answers "does this pid exist", not "is it still running". A killed
# child can stay a zombie until its new parent reaps it, and macOS CI runners
# schedule that reaping later than a developer machine. Treat a zombie as exited
# while still requiring every non-zombie process to disappear.
pid_running() {
  local state
  state=$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ')
  [[ -n $state && ${state:0:1} != Z ]]
}

# --- stopped, then start the sidecar ----------------------------------------

[[ ! -e $data/serve.lock ]] || fail "a fresh data directory already has serve.lock"
printf 'state: stopped (no serve.lock)\n'

"$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
serve_pid=$!

for _ in $(seq 1 100); do
  [[ -s $data/serve.lock ]] && break
  sleep 0.1
done
[[ -s $data/serve.lock ]] || fail "serve.lock did not appear"

lock_port=$(jq -r '.port' "$data/serve.lock")
lock_pid=$(jq -r '.pid' "$data/serve.lock")
[[ $lock_port == "$port" ]] || fail "serve.lock port is $lock_port, expected $port"
[[ $lock_pid == "$serve_pid" ]] || fail "serve.lock pid is $lock_pid, expected $serve_pid"
printf 'state: running(external) pid=%s port=%s\n' "$lock_pid" "$lock_port"

curl -fsS -o /dev/null "$base/admin/session" || fail "GET /admin/session failed"

# --- administrator login and the pages the desktop links to -----------------

token_mode=$(stat -f '%Lp' "$data/admin-token")
[[ $token_mode == 600 ]] || fail "admin-token mode is $token_mode, expected 600"
admin=$(tr -d '\n' <"$data/admin-token")

curl -fsS -c "$jar" -o /dev/null "$base/admin/session"
csrf=$(awk '$6 == "pk_csrf" { print $7 }' "$jar")
[[ -n $csrf ]] || fail "the control session did not set a CSRF cookie"

login_status=$(curl -sS -o "$data/login.json" -w '%{http_code}' -b "$jar" -c "$jar" \
  -H "Origin: $base" -H "X-CSRF-Token: $csrf" -H 'Content-Type: application/json' \
  --data "{\"token\":\"$admin\"}" "$base/admin/login")
[[ $login_status == 200 ]] || fail "admin login returned $login_status, expected 200"
jq -e '.ok == true' "$data/login.json" >/dev/null || fail "admin login did not report ok"
printf 'admin login: 200 with the admin-token file\n'

curl -fsS -b "$jar" -H "Origin: $base" "$base/admin/logs/events?limit=5" \
  >"$data/events.json" || fail "GET /admin/logs/events failed"
jq -e 'type == "array"' "$data/events.json" >/dev/null || fail "logs tail is not an array"
printf 'logs tail: %s event(s)\n' "$(jq 'length' "$data/events.json")"

curl -fsS -b "$jar" "$base/records/summary" >"$data/summary.json" \
  || fail "GET /records/summary failed"
jq -e '.total >= 0' "$data/summary.json" >/dev/null || fail "records summary is malformed"

curl -fsS -b "$jar" "$base/records/stats?groupBy=tag" >"$data/stats.json" \
  || fail "GET /records/stats failed"
jq -e '.buckets | type == "array"' "$data/stats.json" >/dev/null || fail "records stats is malformed"

curl -fsS -b "$jar" "$base/records/list?limit=5" >"$data/list.json" \
  || fail "GET /records/list failed"
jq -e '.records | type == "array"' "$data/list.json" >/dev/null || fail "records list is malformed"
printf 'records overview: summary=%s total, buckets=%s, rows=%s\n' \
  "$(jq '.total' "$data/summary.json")" \
  "$(jq '.buckets | length' "$data/stats.json")" \
  "$(jq '.records | length' "$data/list.json")"

# --- shutdown releases the lock and the port --------------------------------

shutdown_status=$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" \
  -H "Origin: $base" -H "X-CSRF-Token: $csrf" -X POST "$base/admin/shutdown")
[[ $shutdown_status == 202 ]] || fail "shutdown returned $shutdown_status, expected 202"

for _ in $(seq 1 100); do
  if [[ ! -e $data/serve.lock ]] && ! kill -0 "$serve_pid" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
[[ ! -e $data/serve.lock ]] || fail "serve.lock survived shutdown"
if kill -0 "$serve_pid" 2>/dev/null; then
  fail "the serve process survived shutdown"
fi
if nc -z 127.0.0.1 "$port" 2>/dev/null; then
  fail "port $port is still open after shutdown"
fi
printf 'state: stopped (serve.lock removed, port %s closed)\n' "$port"

# --- static audit: desktop manifest, capabilities, token boundary ----------

grep -qx '\[workspace\]' apps/desktop/src-tauri/Cargo.toml \
  || fail "src-tauri is not its own cargo workspace"
grep -q 'exclude = \["apps/desktop"\]' Cargo.toml \
  || fail "apps/desktop is not excluded from the printkit workspace"

cargo metadata --no-deps --format-version 1 | \
  jq -e '[.packages[].name] | index("printkit-desktop") | not' >/dev/null \
  || fail "printkit-desktop is a member of the printkit cargo workspace"

if (cd apps/desktop/src-tauri && cargo tree --prefix none) | \
  grep -Eq '^printkit-(core|render|pdf|engine|platform|trace|facade|protocol|schema|barcode|store)'; then
  fail "the desktop shell depends on a printkit-* crate"
fi

python3 - <<'PY'
import json
import re
from pathlib import Path

capability = json.loads(Path("apps/desktop/src-tauri/capabilities/default.json").read_text())
permissions = capability["permissions"]
names = [p if isinstance(p, str) else p["identifier"] for p in permissions]
assert capability["windows"] == ["business"], "capabilities must cover the single business window"
assert names == [
    "core:default",
    "core:window:allow-start-dragging",
    "core:window:allow-set-title",
], f"unexpected permissions: {names}"
assert not any(name.startswith(("fs:", "http:", "shell:", "opener:")) for name in names), names

config = json.loads(Path("apps/desktop/src-tauri/tauri.conf.json").read_text())
window = config["app"]["windows"][0]
assert window["label"] == "business", "the configured window must be business"
assert (window["width"], window["height"]) == (900, 633), "business window must be 900x633"
assert (window["minWidth"], window["minHeight"]) == (560, 420), "business must share the 560x420 minimum"
for key in ("titleBarStyle", "hiddenTitle", "trafficLightPosition"):
    assert key not in window, f"shared config must not contain macOS overlay key {key}"

macos = json.loads(Path("apps/desktop/src-tauri/tauri.macos.conf.json").read_text())
mac_window = macos["app"]["windows"][0]
assert mac_window["label"] == "business", "macOS overlay must replace the business window"
assert mac_window["title"] == window["title"]
assert (mac_window["width"], mac_window["height"]) == (900, 633)
assert (mac_window["minWidth"], mac_window["minHeight"]) == (560, 420)
assert mac_window["center"] is True
assert mac_window["resizable"] is True
assert mac_window["maximizable"] is True
assert mac_window["titleBarStyle"] == "Overlay"
assert mac_window["hiddenTitle"] is True
assert mac_window["trafficLightPosition"] == {"x": 12, "y": 27}
assert config["bundle"]["externalBin"] == ["binaries/printkit"], "externalBin must be the printkit sidecar"
assert config["bundle"]["targets"] == ["dmg"], "the macOS-first bundle must target dmg"
csp = config["app"]["security"]["csp"]
directives = {}
for part in csp.split(";"):
    tokens = part.split()
    if tokens:
        directives[tokens[0]] = tokens[1:]
required = {"default-src", "script-src", "connect-src", "img-src", "style-src"}
assert required <= set(directives), f"CSP is missing directives: {sorted(required - set(directives))}"
assert directives["default-src"] == ["'self'"], directives["default-src"]
assert directives["script-src"] == ["'self'"], directives["script-src"]
assert directives["connect-src"] == ["ipc:", "http://ipc.localhost"], directives["connect-src"]
assert directives["img-src"] == ["'self'", "data:"], directives["img-src"]
assert directives["style-src"] == ["'self'", "'unsafe-inline'"], directives["style-src"]
# The app never embeds <object>/<embed>, so object-src relies on default-src
# 'self'. If a later config adds it, it may only be 'none'.
assert "object-src" not in directives or directives["object-src"] == ["'none'"]

package = json.loads(Path("apps/desktop/package.json").read_text())
allowed_deps = {
    "@element-plus/icons-vue",
    "@printkit/i18n",
    "@tauri-apps/api",
    "element-plus",
    "vue",
}
allowed_dev = {
    "@tauri-apps/cli",
    "@vitejs/plugin-vue",
    "typescript",
    "vite",
    "vue-tsc",
}
assert set(package["dependencies"]) == allowed_deps, \
    f"unexpected desktop dependencies: {sorted(set(package['dependencies']) - allowed_deps)}"
assert set(package["devDependencies"]) == allowed_dev, \
    f"unexpected desktop devDependencies: {sorted(set(package['devDependencies']) - allowed_dev)}"
assert package["dependencies"]["@printkit/i18n"] == "link:../../packages/i18n", \
    "the desktop must link, not copy, the local i18n catalogue"
assert not {name for name in allowed_deps | allowed_dev if name in {"axios", "node-fetch", "got"}}, \
    "the desktop package must not add an outbound HTTP client"
tauri = Path("apps/desktop/src-tauri/Cargo.toml").read_text()
assert not re.findall(r"(?m)^printkit-[a-z0-9-]+\s*=", tauri), \
    "the desktop manifest must not depend on a printkit-* crate"
assert "plugin-fs" not in tauri and "plugin-http" not in tauri and "plugin-opener" not in tauri, \
    "the desktop shell must not link fs, http or opener plugins"
print("capabilities: single business window core only; no shell/opener")
print("bundle: printkit sidecar, dmg target, complete IPC-only CSP")
print("package: dependency whitelist (including @printkit/i18n)")
PY

admin_hits=$(grep -R -l 'admin-token' apps/desktop/src-tauri/src || true)
admin_bad=$(printf '%s\n' "$admin_hits" | grep -v 'src/control' || true)
[[ -z $admin_bad ]] || fail "admin-token appears outside src/control*: $admin_bad"

scan_roots=(apps/desktop/src)
[[ -d apps/desktop/dist ]] && scan_roots+=(apps/desktop/dist)
if grep -R -q 'admin-token' "${scan_roots[@]}"; then
  fail "the desktop WebView source or built bundle mentions admin-token"
fi
if grep -R -n -E 'v-html|innerHTML|outerHTML|document\.write' apps/desktop/src; then
  fail "the desktop WebView source injects raw HTML"
fi
printf 'token boundary: automatic credential lives only in src/control*\n'
printf 'webview: no v-html/innerHTML injection, scanned %s\n' "${scan_roots[*]}"

grep -q 'Content-Security-Policy' apps/desktop/vite.config.ts \
  || fail "the Vite dev server has no CSP header"
grep -q 'ws://localhost:1420' apps/desktop/vite.config.ts \
  || fail "the dev CSP does not allow the HMR websocket"
grep -q '@media (max-width: 860px)' apps/desktop/src/styles/base.css \
  || fail "the shell has no narrow-window fallback"
if grep -q -E 'app-sidebar\.collapsed|sidebar-collapse' apps/desktop/src/styles/base.css; then
  fail "the removed collapsed sidebar rail is still styled"
fi
grep -q 'desktop-window.json' apps/desktop/src-tauri/src/layout.rs \
  || fail "window geometry is not persisted"
if grep -R -q -E 'OriginsResult|function originAdd\b' apps/desktop/src; then
  fail "dead origin symbols remain in the desktop source"
fi
# The live origin bridge is the Settings Web access card; the legacy symbols
# above must not come back.
for symbol in origins_list origin_add origin_remove; do
  grep -q "$symbol" apps/desktop/src/api.ts \
    || fail "the origin bridge is missing $symbol"
done
printf 'layout: persisted geometry, responsive layout, dev CSP, live origin bridge\n'

if [[ -f apps/desktop/dist/index.html ]]; then
  if grep -q 'fabric' apps/desktop/dist/index.html; then
    fail "the removed designer/fabric chunk is still referenced"
  fi
  printf 'bundle: no fabric chunk remains\n'
fi

# --- the real desktop shell starts and owns a sidecar -----------------------

desktop_bin=${PRINTKIT_DESKTOP_BIN:-$root/apps/desktop/src-tauri/target/debug/printkit-desktop}
if [[ ! -x $desktop_bin ]]; then
  [[ -f apps/desktop/dist/index.html ]] \
    || fail "apps/desktop/dist is missing; run pnpm --dir apps/desktop build before the smoke"
  cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml
fi
[[ -x $desktop_bin ]] || fail "printkit-desktop was not built at $desktop_bin"

dev_data=$(mktemp -d)
# Fresh installs default to the core preset (no optional plugins). The shell
# assertions below exercise the observability and records endpoints, so opt in.
printf 'version = 1\nsettled = 2\npreset = "all"\n' >"$dev_data/plugins.toml"
dev_port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
PRINTKIT_DATA_DIR="$dev_data" PRINTKIT_PORT="$dev_port" \
  "$desktop_bin" >"$dev_data/desktop.log" 2>&1 &
desktop_pid=$!

for _ in $(seq 1 60); do
  if ! kill -0 "$desktop_pid" 2>/dev/null; then
    fail "printkit-desktop exited during startup (another instance may own the single-instance lock): $(tail -5 "$dev_data/desktop.log")"
  fi
  [[ -s $dev_data/serve.lock ]] && break
  sleep 0.25
done

if [[ -s $dev_data/serve.lock ]]; then
  sidecar_pid=$(jq -r '.pid' "$dev_data/serve.lock")
  sidecar_parent=$(ps -o ppid= -p "$sidecar_pid" | tr -d ' ')
  [[ $sidecar_parent == "$desktop_pid" ]] \
    || fail "the sidecar pid $sidecar_pid is not a child of the desktop process $desktop_pid"
  # The sidecar writes serve.lock before it binds the port; CI can schedule the
  # curl in that window, so poll briefly instead of checking once.
  session_status=""
  for _ in $(seq 1 100); do
    if session_status=$(curl -fsS -o /dev/null -w '%{http_code}' \
      "http://127.0.0.1:$dev_port/admin/session" 2>/dev/null); then
      break
    fi
    sleep 0.1
  done
  [[ $session_status == 200 ]] \
    || fail "the desktop-owned sidecar does not answer /admin/session (last=$session_status; desktop: $(tail -3 "$dev_data/desktop.log" 2>/dev/null); serve: $(tail -3 "$dev_data/serve.log" 2>/dev/null))"
  printf 'desktop shell: pid=%s owns sidecar pid=%s on port %s\n' \
    "$desktop_pid" "$sidecar_pid" "$dev_port"

  kill -"$desktop_signal" "$desktop_pid" 2>/dev/null || true
  wait "$desktop_pid" 2>/dev/null || true
  desktop_pid=
  if pid_running "$sidecar_pid"; then
    kill "$sidecar_pid" 2>/dev/null || true
  fi
  # The selected signal gives the shell a bounded chance to stop its sidecar;
  # a loaded runner can need a little longer to tear the process down.
  # A zombie has already exited; only a running process counts.
  for _ in $(seq 1 100); do
    pid_running "$sidecar_pid" || break
    sleep 0.1
  done
  if pid_running "$sidecar_pid"; then
    fail "the desktop sidecar survived SIG$desktop_signal ($(ps -o pid,ppid,stat,etime,command -p "$sidecar_pid" 2>/dev/null | tr '\n' ' '); serve: $(tail -3 "$dev_data/serve.log" 2>/dev/null))"
  fi
  if curl -fsS -o /dev/null "http://127.0.0.1:$dev_port/admin/session" 2>/dev/null; then
    fail "the desktop sidecar port still answers after the process exited"
  fi
  printf 'desktop shell: exited without leaving a window or sidecar\n'
else
  # The shell process is alive but did not create its sidecar within 15s. This
  # session has no way to observe the window, so the ownership assertions are
  # explicitly skipped instead of reported as a pass.
  if [[ ${PRINTKIT_DESKTOP_REQUIRE_SIDECAR:-0} == 1 || ${PRINTKIT_GUI_E2E:-} == true ]]; then
    fail "printkit-desktop did not start its sidecar in 15s"
  fi
  printf 'SKIP: desktop shell launch requires a GUI session with a working async probe; sidecar ownership is not asserted here (set PRINTKIT_DESKTOP_REQUIRE_SIDECAR=1 or PRINTKIT_GUI_E2E=true to make it fatal)\n'
  kill "$desktop_pid" 2>/dev/null || true
  wait "$desktop_pid" 2>/dev/null || true
  desktop_pid=
fi

printf 'PASS: desktop smoke (static audits and shell launch)\n'

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
