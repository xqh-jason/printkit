#!/usr/bin/env bash
# Shared serialization for the macOS acceptance scripts that build or touch CUPS.
#
# ippeveprinter + lpadmin scripts mutate the real CUPS scheduler (queues, jobs,
# the default destination). They also rebuild target/debug/printkit before each
# run, and the scripts need different feature sets, so two runs at once can
# overwrite the binary under the other run's feet. Every script that builds or
# calls lpadmin/lpstat sources this file, takes the lock before any build or
# CUPS mutation, and releases it in cleanup after removing its queues.
#
# Primary lock: `flock` on /tmp/printkit-cups-tests.lock. The kernel releases it
# when the last descriptor closes, including on SIGKILL, so no pid liveness
# probe is needed (the runner here assigns non-ps-visible process ids, so a
# pid check can call live owners dead). Children inherit the descriptor until
# they exit; cleanup kills the long-lived ones (ippeveprinter/serve) first.
#
# Fallback (no flock, e.g. stock macOS): atomic `mkdir` plus a pid file. A live
# owner is never stolen, however old the lock is. Only when the recorded pid is
# not alive AND the lock directory is older than five minutes is it claimed, by
# an atomic `mv`, so two waiters cannot both delete a freshly acquired lock.
#
# Nested runs: a script invoked by another script that already holds the lock
# must set PRINTKIT_CUPS_TEST_LOCK_HELD=1 so it does not deadlock on its own
# parent; that child takes no new lock and releases nothing.
#
# Usage:
#   source "tests/scripts/lib/cups_test_lock.sh"    # after cd to repo root
#   cups_test_lock_acquire || fail "CUPS test lock timeout"
#   ...build and run...
#   # in the script's cleanup trap, after deleting the CUPS queues:
#   cups_test_lock_release
#
# PRINTKIT_CUPS_TEST_LOCK            lock file/dir (default /tmp/printkit-cups-tests.lock)
# PRINTKIT_CUPS_TEST_LOCK_TIMEOUT    seconds to wait (default 180)
# PRINTKIT_CUPS_TEST_FORCE_MKDIR=1   ignore flock and use the mkdir fallback
# PRINTKIT_CUPS_TEST_LOCK_HELD=1     caller already holds the lock (nested run)

cups_test_lock_path=${PRINTKIT_CUPS_TEST_LOCK:-/tmp/printkit-cups-tests.lock}
cups_test_lock_timeout=${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-180}
cups_test_lock_owned=0
cups_test_lock_kind=

cups_test_lock_file_mtime() {
  stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || printf '0\n'
}

cups_test_lock_acquire() {
  ((cups_test_lock_owned == 0)) || return 0
  local timeout=${PRINTKIT_CUPS_TEST_LOCK_TIMEOUT:-${cups_test_lock_timeout:-180}}
  if [[ ${PRINTKIT_CUPS_TEST_LOCK_HELD:-0} == 1 ]]; then
    cups_test_lock_kind=inherited
    cups_test_lock_owned=1
    return 0
  fi

  if [[ ${PRINTKIT_CUPS_TEST_FORCE_MKDIR:-0} != 1 ]] && command -v flock >/dev/null 2>&1; then
    exec 9>>"$cups_test_lock_path"
    if ! flock -w "$timeout" 9; then
      exec 9>&-
      printf 'FAIL: could not acquire the CUPS test lock %s within %ss\n' \
        "$cups_test_lock_path" "$timeout" >&2
      return 1
    fi
    cups_test_lock_kind=flock
    cups_test_lock_owned=1
    return 0
  fi

# Use the real binary: the harness's mkdir builtin was observed to let two
# waiters create the same directory, which breaks the mutual exclusion.
  local pid mtime now age waited=0 stale
  local attempts=$((timeout * 5))
  local dir="$cups_test_lock_path.d"
  while ! /bin/mkdir "$dir" 2>/dev/null; do
    pid=$(cat "$dir/pid" 2>/dev/null || true)
    if [[ -z $pid ]] || ! kill -0 "$pid" 2>/dev/null; then
      mtime=$(cups_test_lock_file_mtime "$dir")
      now=$(date +%s)
      age=$((now - mtime))
      # A live owner is never stolen, no matter how old the lock is.
      if ((age > 300)); then
        stale="$dir.stale.$$"
        rm -rf "$stale"
        if /bin/mv "$dir" "$stale" 2>/dev/null; then
          rm -rf "$stale"
        fi
        continue
      fi
    fi
    if ((waited >= attempts)); then
      printf 'FAIL: CUPS test lock %s is still held after %ss\n' \
        "$dir" "$timeout" >&2
      return 1
    fi
    sleep 0.2
    waited=$((waited + 1))
  done
  printf '%s\n' "$$" >"$dir/pid"
  cups_test_lock_kind=mkdir
  cups_test_lock_owned=1
}

cups_test_lock_release() {
  ((cups_test_lock_owned == 1)) || return 0
  if [[ $cups_test_lock_kind == flock ]]; then
    flock -u 9
    exec 9>&-
  elif [[ $cups_test_lock_kind == mkdir ]]; then
    local owner
    owner=$(cat "$cups_test_lock_path.d/pid" 2>/dev/null || true)
    if [[ $owner != $$ ]]; then
      printf 'WARN: CUPS test lock is held by %s, not %s; leaving it alone\n' \
        "${owner:-unknown}" "$$" >&2
      cups_test_lock_owned=0
      cups_test_lock_kind=
      return 1
    fi
    rm -rf "$cups_test_lock_path.d"
  fi
  cups_test_lock_owned=0
  cups_test_lock_kind=
}

# Remove one test queue. A missing queue is success; a queue that stays after
# five attempts is a failure and prints why, so cleanup cannot silently leave
# the shared scheduler dirty.
cups_test_remove_queue() {
  local queue=$1 attempt
  lpstat -p "$queue" >/dev/null 2>&1 || return 0
  for attempt in 1 2 3 4 5; do
    if lpadmin -x "$queue" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  printf 'FAIL: could not remove CUPS queue %s after 5 attempts\n' "$queue" >&2
  return 1
}
