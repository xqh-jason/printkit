#!/usr/bin/env bash
# One-command development launcher for the CLI service and the Vue surfaces.
#
# The service uses a throwaway data directory and loopback port so a developer
# session never touches the installed application's state.
set -euo pipefail
cd "$(dirname "$0")/.."

# The dev data directory is fixed: cleanup below may stop only the sidecar
# recorded/owning this directory, never another user's service.
DATA=/tmp/printkit-dev
PORT="${PRINTKIT_DEV_PORT:-19080}"
WEB_PORT=1420
REPO_ROOT="$(pwd -P)"
LOCK_FILE="$DATA/serve.lock"

usage() {
  cat <<'USAGE'
usage: ./scripts/dev.sh <command>

commands:
  setup    Check required tools and install desktop dependencies
  serve    Run printkit serve with the development data directory and port
  desktop  Install deps, build the debug sidecar, run tauri dev with the dev dir/port
  stop     Send SIGTERM to the pid recorded in serve.lock
  help     Show this message

environment:
  PRINTKIT_DEV_PORT  loopback port   (default: 19080; use another port if 19080 is external)

The service binds only 127.0.0.1 and serves JSON. Ctrl-C stops serve and desktop.
USAGE
}

die() {
  echo "dev: $*" >&2
  exit 1
}

tool_or_die() {
  local tool="$1" hint="$2"
  command -v "$tool" >/dev/null 2>&1 || die "missing $tool - $hint"
}

check_tool() {
  local tool="$1" hint="$2"
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "dev: missing $tool - $hint" >&2
    return 1
  fi
}

setup() {
  local failed=0
  check_tool cargo "install Rust from https://rustup.rs" || failed=1
  check_tool node "install Node.js 22 from https://nodejs.org" || failed=1
  check_tool pnpm "enable pnpm with: corepack enable" || failed=1
  check_tool pdftotext "install poppler with: brew install poppler" || failed=1
  check_tool jq "install jq with: brew install jq" || failed=1
  if ((failed != 0)); then
    echo "dev: install the tools above, then rerun ./scripts/dev.sh setup" >&2
    return 1
  fi

  echo "dev: installing apps/desktop dependencies"
  pnpm --dir apps/desktop install --frozen-lockfile
  echo "dev: dependencies are ready"
}

lock_field() {
  local field="$1"
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg field "$field" '.[$field] // empty' "$LOCK_FILE" 2>/dev/null || true
  elif command -v python3 >/dev/null 2>&1; then
    python3 - "$LOCK_FILE" "$field" <<'PY' 2>/dev/null || true
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    print(json.load(handle).get(sys.argv[2], ""))
PY
  else
    echo "dev: install jq or python3 to read $LOCK_FILE" >&2
    return 1
  fi
}

run_serve() {
  tool_or_die cargo "install Rust from https://rustup.rs"
  clean_dev_state || exit 1

  local pid="" port=""
  if [[ -e "$LOCK_FILE" ]]; then
    pid="$(lock_field pid)" || exit 1
    port="$(lock_field port)" || exit 1
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
      echo "dev: printkit serve is already running for $DATA (pid $pid, port ${port:-unknown})."
      echo "dev: stop it with ./scripts/dev.sh stop"
      return 1
    fi
    echo "dev: stale $LOCK_FILE (pid ${pid:-unknown} is not running); the next serve reclaims it."
  fi

  echo "dev: data:  $DATA"
  echo "dev: port:  $PORT"
  echo
  exec cargo run -p printkit -- --data-dir "$DATA" --port "$PORT" serve
}

port_pid() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null | head -n 1 || true
  fi
}

process_cwd() {
  local pid="$1"
  lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1
}

wait_for_exit() {
  local pid="$1" attempt
  for ((attempt = 0; attempt < 30; attempt++)); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  return 1
}

stop_previous_desktop() {
  local pid command cwd
  # Stop the old Tauri CLI first: killing only the app leaves its Vite child
  # holding 1420, and a second `tauri dev` would then fail on the port.
  for pid in $(pgrep -f 'tauri dev' 2>/dev/null || true); do
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    cwd="$(process_cwd "$pid")"
    [[ "$command" == *"apps/desktop"* && ( "$cwd" == "$REPO_ROOT" || "$cwd" == "$REPO_ROOT/apps/desktop" ) ]] || continue
    echo "dev: stopping the previous Tauri dev CLI (pid $pid)"
    kill -TERM "$pid" 2>/dev/null || true
    if ! wait_for_exit "$pid"; then
      echo "dev: Tauri dev CLI pid $pid did not exit; stop it manually" >&2
      return 1
    fi
  done

  # A release app with the same Tauri identifier owns the single-instance
  # socket, so a dev launch would hand its args to the installed app and exit
  # without opening the compact debug window. Only stop the matching bundle.
  for pid in $(pgrep -f 'printkit desktop.app/Contents/MacOS/printkit-desktop' 2>/dev/null || true); do
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    [[ "$command" == *"/printkit desktop.app/Contents/MacOS/printkit-desktop"* ]] || continue
    bundle="${command%%/Contents/MacOS/printkit-desktop*}"
    bundle_id="$(plutil -extract CFBundleIdentifier raw "$bundle/Contents/Info.plist" 2>/dev/null || true)"
    [[ "$bundle_id" == "dev.printkit.desktop" ]] || continue
    echo "dev: stopping the previous installed printkit desktop (pid $pid)"
    kill -TERM "$pid" 2>/dev/null || true
    if ! wait_for_exit "$pid"; then
      echo "dev: installed printkit desktop pid $pid did not exit; quit it manually" >&2
      return 1
    fi
  done

  for pid in $(pgrep -f 'target/debug/printkit-desktop' 2>/dev/null || true); do
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    [[ "$command" == *"target/debug/printkit-desktop"* ]] || continue
    echo "dev: stopping the previous dev app (pid $pid) before starting a fresh one"
    kill -TERM "$pid" 2>/dev/null || true
    if ! wait_for_exit "$pid"; then
      echo "dev: dev app pid $pid did not exit; send SIGKILL or close it manually" >&2
      return 1
    fi
  done
}

ensure_web_port() {
  local attempt pid command cwd
  for ((attempt = 0; attempt < 50; attempt++)); do
    pid="$(port_pid "$WEB_PORT")"
    [[ -z "$pid" ]] && return 0

    command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    cwd="$(process_cwd "$pid")"
    if [[ "$cwd" == "$REPO_ROOT/apps/desktop" || "$command" == *"$REPO_ROOT/apps/desktop"* ]]; then
      echo "dev: stopping the stale Vite server on port $WEB_PORT (pid $pid)"
      kill -TERM "$pid" 2>/dev/null || true
      if ! wait_for_exit "$pid"; then
        echo "dev: Vite pid $pid did not exit; send SIGKILL or stop it manually" >&2
        return 1
      fi
      continue
    fi

    # The listener may be exiting after its parent was stopped; give the
    # socket a moment to disappear before diagnosing an unrelated owner.
    if [[ -z "$command" && -z "$cwd" ]]; then
      sleep 0.1
      continue
    fi

    die "port $WEB_PORT is in use by pid $pid (${command:-unknown}); stop that process and rerun"
  done

  pid="$(port_pid "$WEB_PORT")"
  die "port $WEB_PORT is still in use by pid ${pid:-unknown}; stop that process and rerun"
}

is_dev_serve() {
  local pid="$1" command
  command="$(ps -ww -p "$pid" -o command= 2>/dev/null || true)"
  [[ "$command" == *"printkit"* && "$command" == *" serve "* && "$command" == *"--data-dir $DATA"* ]]
}

stop_dev_serve() {
  local pid="$1" reason="$2"
  echo "dev: stopping $reason (pid $pid)"
  kill -TERM "$pid" 2>/dev/null || true
  if ! wait_for_exit "$pid"; then
    echo "dev: pid $pid did not exit after SIGTERM; sending SIGKILL"
    kill -KILL "$pid" 2>/dev/null || true
    wait_for_exit "$pid" || die "dev-owned sidecar pid $pid is still alive"
  fi
}

clean_dev_state() {
  local lock_pid="" port_pid="" command
  mkdir -p "$DATA"

  if [[ -e "$LOCK_FILE" ]]; then
    lock_pid="$(lock_field pid)" || exit 1
    if [[ "$lock_pid" =~ ^[0-9]+$ ]] && kill -0 "$lock_pid" 2>/dev/null; then
      if ! is_dev_serve "$lock_pid"; then
        command="$(ps -ww -p "$lock_pid" -o command= 2>/dev/null || true)"
        die "serve.lock pid $lock_pid is not a dev-owned printkit serve: ${command:-unknown}; inspect $LOCK_FILE"
      fi
      stop_dev_serve "$lock_pid" "the previous dev sidecar"
    else
      echo "dev: removing stale $LOCK_FILE (pid ${lock_pid:-unknown} is not running)"
      rm -f "$LOCK_FILE"
    fi
  fi

  port_pid="$(port_pid "$PORT")"
  if [[ -n "$port_pid" ]]; then
    if ! is_dev_serve "$port_pid"; then
      command="$(ps -ww -p "$port_pid" -o command= 2>/dev/null || true)"
      die "port $PORT is held by pid $port_pid (${command:-unknown}), not a dev-owned sidecar; set PRINTKIT_DEV_PORT to another port"
    fi
    stop_dev_serve "$port_pid" "the previous dev sidecar on port $PORT"
  fi

  # A service that removed its lock before exiting can leave one behind after
  # SIGKILL; only remove it when its pid is definitely gone.
  if [[ -e "$LOCK_FILE" ]]; then
    lock_pid="$(lock_field pid)" || exit 1
    if [[ ! "$lock_pid" =~ ^[0-9]+$ ]] || ! kill -0 "$lock_pid" 2>/dev/null; then
      rm -f "$LOCK_FILE"
    fi
  fi
}

desktop() {
  tool_or_die pnpm "enable pnpm with: corepack enable"
  tool_or_die node "install Node.js 20.19+ or 22.12+ from https://nodejs.org"
  tool_or_die cargo "install Rust from https://rustup.rs"
  node -e '
const [major, minor] = process.versions.node.split(".").map(Number);
const ok = (major === 20 && minor >= 19) || (major === 22 && minor >= 12) || major > 22;
process.exit(ok ? 0 : 1);
' || die "Node.js 20.19+ or 22.12+ is required for Vite 7 (found $(node -v))"

  stop_previous_desktop || exit 1
  clean_dev_state || exit 1
  ensure_web_port || exit 1
  pnpm --dir apps/desktop install --frozen-lockfile
  # prepare-sidecar stages the externalBin used by Tauri packaging. Do not run
  # that staged copy in place: on this host macOS provenance can SIGKILL the
  # same bytes with "Code Signature Invalid". The dev shell points at the
  # cargo-built executable instead.
  bash apps/desktop/scripts/prepare-sidecar.sh debug
  dev_sidecar="$PWD/target/debug/printkit"
  [[ -x "$dev_sidecar" ]] || die "missing executable $dev_sidecar; cargo build -p printkit first"
  export PRINTKIT_SIDECAR_PATH="$dev_sidecar"
  export PRINTKIT_DATA_DIR="$DATA"
  export PRINTKIT_PORT="$PORT"
  echo "dev: data:  $DATA"
  echo "dev: port:  $PORT"
  echo "dev: web:   http://localhost:$WEB_PORT"
  echo
  # `tauri dev` starts the single `business` window from tauri.conf.json against
  # the Vite dev server; the sidecar path, data dir and port come from the env
  # exported above. No window or runner arguments are forwarded here.
  exec pnpm --dir apps/desktop tauri dev
}

stop_serve() {
  if [[ ! -e "$LOCK_FILE" ]]; then
    echo "dev: no $LOCK_FILE; printkit serve is not running for $DATA"
    return 0
  fi

  local pid command
  pid="$(lock_field pid)" || exit 1
  if [[ -z "$pid" ]]; then
    echo "dev: $LOCK_FILE has no pid; not stopping anything" >&2
    return 1
  fi
  if [[ ! "$pid" =~ ^[0-9]+$ ]]; then
    echo "dev: $LOCK_FILE has an invalid pid; not stopping anything" >&2
    return 1
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "dev: stale $LOCK_FILE (pid $pid is not running); the next serve reclaims it"
    return 0
  fi

  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  if [[ "$command" != *printkit* ]]; then
    echo "dev: pid $pid is not a printkit process: ${command:-unknown}" >&2
    echo "dev: refusing to stop it; inspect $LOCK_FILE first" >&2
    return 1
  fi

  kill -TERM "$pid"
  for ((attempt = 0; attempt < 50; attempt++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "dev: stopped printkit serve (pid $pid)"
      return 0
    fi
    sleep 0.1
  done
  echo "dev: sent SIGTERM to pid $pid; it is still running" >&2
  return 1
}

case "${1:-help}" in
setup) setup ;;
serve) run_serve ;;
desktop) desktop ;;
stop) stop_serve ;;
help | -h | --help) usage ;;
*)
  usage >&2
  exit 2
  ;;
esac
