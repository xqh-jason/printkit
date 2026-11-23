#!/usr/bin/env bash
# S1 fail-open seam: a misbehaving hook is contained, and the core print path
# stays unchanged. Hooks are not wired into the print path in S1; this script
# exercises the host runner and a real one-shot print with the test env set.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
source "tests/scripts/lib/cups_test_lock.sh"

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$1"; }

work=
queue=
ippeve_pid=

cleanup() {
  local status=$?
  if [[ -n $ippeve_pid ]] && kill -0 "$ippeve_pid" 2>/dev/null; then
    kill "$ippeve_pid" 2>/dev/null || true
    wait "$ippeve_pid" 2>/dev/null || true
  fi
  if [[ -n $queue ]] && ! cups_test_remove_queue "$queue"; then
    status=1
  fi
  [[ -z $work ]] || rm -rf "$work"
  cups_test_lock_release || status=1
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# The lock covers both builds and CUPS: fake_printer/fake_raw rebuild
# target/debug/printkit without --features serve,fake-render, so an unlocked
# build would replace the binary this script is about to run.
cups_test_lock_acquire \
  || fail "could not acquire the CUPS test lock within ${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-60}s"
cargo build -q -p printkit --features serve,fake-render
bin=${PRINTKIT_BIN:-$root/target/debug/printkit}
for mode in panic sleep warn; do
  out=$(mktemp)
  err=$(mktemp)
  if ! PRINTKIT_TEST_HOOK=$mode cargo run -q -p printkit-plugin-host --example hook_stress >"$out" 2>"$err"; then
    cat "$err" >&2
    rm -f "$out" "$err"
    fail "hook_stress $mode exited non-zero"
  fi
  grep -q 'print-ok' "$out" || { cat "$out" "$err" >&2; rm -f "$out" "$err"; fail "hook_stress $mode lost the print result"; }
  grep -q 'warning' "$err" || { cat "$err" >&2; rm -f "$out" "$err"; fail "hook_stress $mode produced no containment warning"; }
  rm -f "$out" "$err"
done
ok "panic/sleep/warn hooks are contained and the caller still succeeds"

# The debug-only print seam runs the test hook before render/print. Every mode
# must still produce a PDF/print result.
work=$(mktemp -d)
queue="printkit-hook-$$"
ipp_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
spool="$work/spool"
mkdir -p "$spool"
command -v ippeveprinter >/dev/null || fail "ippeveprinter is required for the print gate"
command -v lpadmin >/dev/null || fail "lpadmin is required for the print gate"
ippeveprinter -p "$ipp_port" -d "$spool" -k -s 60000,0 -v "$queue" \
  >"$work/ippeve.log" 2>&1 &
ippeve_pid=$!
for _ in $(seq 1 100); do
  if python3 - "$ipp_port" <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.2)
s.close()
PY
  then
    break
  fi
  sleep 0.1
done
cups_test_remove_queue "$queue" || fail "could not remove stale queue $queue"
lpadmin -p "$queue" -E -v "ipp://127.0.0.1:$ipp_port/ipp/print" -m everywhere \
  || fail "lpadmin could not create the print-gate queue"
for mode in panic sleep warn; do
  PRINTKIT_TEST_HOOK=$mode "$bin" pdf --data-dir "$work" \
    -i tests/fixtures/minimal.json -o "$work/$mode.pdf" >/dev/null \
    || fail "real pdf failed with $mode hook"
  [[ -s "$work/$mode.pdf" ]] || fail "real pdf produced no file with $mode hook"
done
PRINTKIT_TEST_HOOK=panic "$bin" print --data-dir "$work" -p "$queue" \
  -i tests/fixtures/minimal.json >/dev/null || fail "real print failed with panic hook"
ok "real pdf/print stay successful with panic/timeout/warn hooks"

PRINTKIT_CUPS_TEST_LOCK_HELD=1 bash tests/scripts/plugins_core_only_e2e.sh
ok "core-only e2e still green"

echo "plugins_fail_open_e2e: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
