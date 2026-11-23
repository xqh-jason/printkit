#!/usr/bin/env bash
# SIGTERM and Ctrl-C (SIGINT) must drain like /admin/shutdown: stop accepting
# jobs, wait for in-flight work, log the result, and remove serve.lock.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

# fake-render gives every job a controllable in-flight delay, and a throwaway
# target keeps the shared target/debug/printkit free of the test build.
cargo_target=$(mktemp -d)
bin=${PRINTKIT_BIN:-$cargo_target/debug/printkit}
root_bin=$root/target/debug/printkit
root_stamp=
if [[ -e $root_bin ]]; then
  root_stamp=$(stat -f '%m %z' "$root_bin" 2>/dev/null || stat -c '%Y %s' "$root_bin")
fi
if [[ -z ${PRINTKIT_BIN:-} ]]; then
  cargo build -q -p printkit --features fake-render --target-dir "$cargo_target"
fi
[[ -x $bin ]] || { echo "FAIL: no signal-test binary at $bin" >&2; exit 1; }

work=$(mktemp -d)
pid=
cleanup() {
  if [[ -n ${pid:-} ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -rf "$work" "$cargo_target"
}
trap cleanup EXIT

write_batch() {
  local path=$1 delay=$2
  python3 - "$path" "$delay" <<'PY'
import json
import sys

path, delay = sys.argv[1], sys.argv[2]
document = {
    "version": 1,
    "pages": [{
        "spec": {"size": "A4"},
        "items": [{
            "id": "body",
            "type": "text",
            "frame": {"x": 20, "y": 20, "width": 160, "height": 12},
            "style": {"fontSize": 8, "lineHeight": 1.4, "color": "#000000", "align": "left"},
            "data": {"text": f"drain test fake-render-delay:{delay}"},
        }],
    }],
}
meta = json.dumps({"copies": 1}).encode()
body = json.dumps(document).encode()
payload = (
    b"--b\r\nContent-Type: application/json\r\n\r\n" + meta + b"\r\n"
    b"--b\r\nContent-Type: application/json\r\n\r\n" + body + b"\r\n"
    b"--b--\r\n"
)
with open(path, "wb") as handle:
    handle.write(payload)
PY
}

submit_batch() {
  local data=$1 base=$2 path=$3
  curl -sS -o "$data/submit.json" -w '%{http_code}' -X POST "$base/print/batch" \
    -H 'Content-Type: multipart/mixed; boundary=b' \
    --data-binary @"$path"
}

run_signal() {
  local signal=$1
  local data="$work/$signal"
  mkdir -p "$data"
  local port
  port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
  "$bin" serve --data-dir "$data" --port "$port" >"$data/serve.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 100); do
    [[ -s $data/serve.lock ]] && break
    sleep 0.1
  done
  [[ -s $data/serve.lock ]] || { echo "FAIL: no serve.lock for $signal" >&2; cat "$data/serve.log" >&2; exit 1; }
  for _ in $(seq 1 100); do
    grep -q 'listening on http://127.0.0.1:' "$data/serve.log" && break
    sleep 0.1
  done
  grep -q 'listening on http://127.0.0.1:' "$data/serve.log" \
    || { echo "FAIL: serve never listened for $signal" >&2; cat "$data/serve.log" >&2; exit 1; }

  write_batch "$data/inflight.bin" 5000
  status=$(submit_batch "$data" "http://127.0.0.1:$port" "$data/inflight.bin")
  [[ $status == 200 ]] || { echo "FAIL: $signal in-flight submit returned $status" >&2; cat "$data/submit.json" >&2; exit 1; }
  grep -q '"jobId"' "$data/submit.json" || { echo "FAIL: $signal in-flight submit returned no jobId" >&2; exit 1; }

  kill -"$signal" "$pid"
  # The drain must refuse new work while the delayed render is still in flight.
  write_batch "$data/refused.bin" 0
  refused=
  for _ in $(seq 1 40); do
    status=$(submit_batch "$data" "http://127.0.0.1:$port" "$data/refused.bin" || true)
    if [[ $status == 503 ]]; then
      refused=1
      break
    fi
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.05
  done
  [[ -n $refused ]] || { echo "FAIL: $signal drain accepted a new batch (last status ${status:-none})" >&2; cat "$data/submit.json" >&2; exit 1; }
  grep -q E_UNSUPPORTED "$data/submit.json" \
    || { echo "FAIL: $signal drain refusal did not report E_UNSUPPORTED" >&2; cat "$data/submit.json" >&2; exit 1; }
  printf 'SIG%s: new batch refused during drain (HTTP 503)\n' "$signal"

  for _ in $(seq 1 80); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "FAIL: the service survived SIG$signal" >&2
    exit 1
  fi
  wait "$pid" 2>/dev/null || true
  pid=

  grep -q 'printkit: shutdown drained=' "$data/serve.log" \
    || { echo "FAIL: SIG$signal did not report the drain result" >&2; cat "$data/serve.log" >&2; exit 1; }
  [[ ! -e $data/serve.lock ]] || { echo "FAIL: SIG$signal left serve.lock" >&2; exit 1; }
  if nc -z 127.0.0.1 "$port" 2>/dev/null; then
    echo "FAIL: port $port is still open after SIG$signal" >&2
    exit 1
  fi
  printf 'SIG%s: drained log, listener stopped, lock removed\n' "$signal"
}

run_signal TERM
run_signal INT

if [[ -e $root_bin ]]; then
  after=$(stat -f '%m %z' "$root_bin" 2>/dev/null || stat -c '%Y %s' "$root_bin")
  [[ $after == "$root_stamp" ]] || { echo "FAIL: the fake-render build replaced shared target/debug/printkit" >&2; exit 1; }
else
  [[ ! -e $root_bin ]] || { echo "FAIL: the fake-render build left shared target/debug/printkit" >&2; exit 1; }
fi
cleanup
trap - EXIT
[[ ! -e $work ]] || { echo "FAIL: the temporary work dir was not cleaned up: $work" >&2; exit 1; }
[[ ! -d $cargo_target ]] || { echo "FAIL: the temporary target was not cleaned up" >&2; exit 1; }
if pgrep -f "serve --data-dir $work" >/dev/null 2>&1; then
  echo "FAIL: a serve process from the signal test is still running" >&2
  exit 1
fi
echo "signal_shutdown: ok (SIGTERM and SIGINT drain, refuse new jobs, lock clean, no temp leftovers)"
exit 0
