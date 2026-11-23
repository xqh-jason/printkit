#!/usr/bin/env bash
# Serve the URL fixtures on the loopback port that tests/fixtures/url.json names.
# The fixture is on loopback, which the SSRF guard refuses by default. A
# **debug** build honours the test-only opt-in; a release build ignores it.
# Run this in one shell, then run
#   PRINTKIT_ALLOW_PRIVATE_FETCH=1 ./target/debug/printkit pdf -i tests/fixtures/url.json
set -euo pipefail
cd "$(dirname "$0")/../fixtures"
exec python3 -m http.server 8899 --bind 127.0.0.1
