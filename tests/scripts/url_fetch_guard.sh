#!/usr/bin/env bash
# The URL guard refuses the loopback fixture by default, and only a debug build
# honours the test opt-in. A release binary must keep refusing it.
set -euo pipefail
cd "$(dirname "$0")/../.."

# A fixed fixture port collides with other sessions' test servers; ask the OS
# for a free one instead. The guard only cares that the page is loopback.
PORT=$(python3 - <<'PY'
import socket

with socket.socket() as probe:
    probe.bind(("127.0.0.1", 0))
    print(probe.getsockname()[1])
PY
)
cargo build -q -p printkit
cargo build -q --release -p printkit
# `python3 -m http.server` calls socket.getfqdn() between bind() and listen();
# macOS 15+ can stall that lookup for ~35 s (local network privacy,
# actions/runner-images#14409), so the socket is bound but not listening and a
# readiness probe sees ECONNREFUSED. The helper patches the lookup so the
# fixture starts listening immediately; its output is saved for a failure.
server_log=$(mktemp)
(
  cd tests/fixtures
  exec python3 ../scripts/http_fixture_server.py "$PORT" --bind 127.0.0.1
) >"$server_log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; rm -f "$server_log"' EXIT

ready=false
for _ in $(seq 1 50); do
  if python3 - "$PORT" <<'PY'
import socket, sys
s = socket.socket()
s.settimeout(0.1)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
PY
  then
    ready=true
    break
  fi
  sleep 0.1
done
[ "$ready" = true ] || {
  echo "url_fetch_guard: the fixture server did not start" >&2
  cat "$server_log" >&2
  exit 1
}

DATA_DIR="$(mktemp -d)"
OUT="$(mktemp)"
# The canonical fixture names port 8899; this run may hold a different free
# port, so feed the guard a copy that names the port it is actually serving.
GUARD_INPUT="$(mktemp)"
sed "s/127\\.0\\.0\\.1:8899/127.0.0.1:$PORT/" tests/fixtures/url.json >"$GUARD_INPUT"
trap 'kill "$server" 2>/dev/null || true; rm -rf "$DATA_DIR" "$OUT" "$GUARD_INPUT"; rm -f "$server_log"' EXIT

# 1. Debug build, no opt-in: the loopback fixture is refused.
set +e
./target/debug/printkit --data-dir "$DATA_DIR" pdf -i "$GUARD_INPUT" -o "$OUT" \
  >/tmp/pk-guard.out 2>/tmp/pk-guard.err
status=$?
set -e
[ "$status" -ne 0 ] || {
  echo "url_fetch_guard: default build accepted the loopback fixture" >&2
  exit 1
}
grep -q "E_UNSUPPORTED" /tmp/pk-guard.err || {
  echo "url_fetch_guard: refusal did not report E_UNSUPPORTED" >&2
  cat /tmp/pk-guard.err >&2
  exit 1
}

# 2. Debug build with the opt-in: the fixture renders.
PRINTKIT_ALLOW_PRIVATE_FETCH=1 ./target/debug/printkit --data-dir "$DATA_DIR" \
  pdf -i "$GUARD_INPUT" -o "$OUT" >/dev/null 2>&1
[ -s "$OUT" ] || {
  echo "url_fetch_guard: the opt-in did not render the fixture" >&2
  exit 1
}

# 3. Release build ignores the opt-in: the same request is still refused.
set +e
PRINTKIT_ALLOW_PRIVATE_FETCH=1 ./target/release/printkit --data-dir "$DATA_DIR" \
  pdf -i "$GUARD_INPUT" -o "$OUT" >/tmp/pk-guard.out 2>/tmp/pk-guard.err
status=$?
set -e
[ "$status" -ne 0 ] || {
  echo "url_fetch_guard: release build honoured the test-only opt-in" >&2
  exit 1
}
grep -q "E_UNSUPPORTED" /tmp/pk-guard.err || {
  echo "url_fetch_guard: release refusal did not report E_UNSUPPORTED" >&2
  cat /tmp/pk-guard.err >&2
  exit 1
}

echo "url_fetch_guard: ok (debug refused, debug opt-in rendered, release refused)"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
