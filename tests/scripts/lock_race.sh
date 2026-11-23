#!/usr/bin/env bash
# Single-instance lock regression: concurrent starts on one data directory.
#
# Two `serve` processes race for the same data directory. Exactly one may reach
# "listening"; the other must exit with E_ALREADY_RUNNING. The loser must never
# delete or replace the winner's diagnostic `serve.lock`.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
if [[ ! -x $bin ]]; then
  cargo build -q -p printkit
fi

rounds=${LOCK_RACE_ROUNDS:-50}
work=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$work"
}
trap cleanup EXIT

free_port() {
  python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

fail() {
  echo "lock_race: FAIL: $*" >&2
  echo "--- A log ---" >&2
  cat "$work/a.log" >&2 || true
  echo "--- B log ---" >&2
  cat "$work/b.log" >&2 || true
  echo "--- lock ---" >&2
  cat "$work/data/serve.lock" >&2 || true
  exit 1
}

echo "lock_race: $rounds rounds, $bin"
for i in $(seq 1 "$rounds"); do
  data="$work/data"
  rm -rf "$data"
  mkdir -p "$data"
  port_a=$(free_port)
  port_b=$(free_port)
  : >"$work/a.log"
  : >"$work/b.log"

  "$bin" serve --data-dir "$data" --port "$port_a" >"$work/a.log" 2>&1 &
  pid_a=$!
  pids+=("$pid_a")
  # Start the peer inside the winner's create/lock window.
  "$bin" serve --data-dir "$data" --port "$port_b" >"$work/b.log" 2>&1 &
  pid_b=$!
  pids+=("$pid_b")

  # Give both processes time to either listen or refuse.
  for _ in $(seq 1 30); do
    listening_a=$(grep -c 'listening on' "$work/a.log" || true)
    listening_b=$(grep -c 'listening on' "$work/b.log" || true)
    if [[ $listening_a -eq 1 || $listening_b -eq 1 ]]; then
      sleep 0.2
      break
    fi
    sleep 0.1
  done

  listening_a=$(grep -c 'listening on' "$work/a.log" || true)
  listening_b=$(grep -c 'listening on' "$work/b.log" || true)
  total=$((listening_a + listening_b))
  [[ $total -eq 1 ]] || fail "round $i: $total processes reached listening (A=$listening_a B=$listening_b)"

  if [[ $listening_a -eq 1 ]]; then
    winner_pid=$pid_a
    loser_pid=$pid_b
    loser_log="$work/b.log"
  else
    winner_pid=$pid_b
    loser_pid=$pid_a
    loser_log="$work/a.log"
  fi

  # The loser must refuse explicitly, not crash or start silently.
  if ! kill -0 "$loser_pid" 2>/dev/null; then
    grep -q 'E_ALREADY_RUNNING' "$loser_log" \
      || fail "round $i: loser exited without E_ALREADY_RUNNING"
  else
    fail "round $i: loser is still running with pid $loser_pid"
  fi

  # The loser's Drop must not remove the winner's lock.
  lock="$data/serve.lock"
  [[ -f $lock ]] || fail "round $i: serve.lock disappeared after the loser exited"
  jq -e --argjson pid "$winner_pid" '.pid == $pid' "$lock" >/dev/null \
    || fail "round $i: serve.lock does not name the winner $winner_pid"

  kill "$winner_pid" 2>/dev/null || true
  wait "$winner_pid" 2>/dev/null || true
done

echo "lock_race: ok ($rounds/$rounds rounds, one listener each)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
