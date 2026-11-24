#!/usr/bin/env python3
"""Serve files like `python3 -m http.server`, without the macOS getfqdn stall.

CPython's `http.server.HTTPServer.server_bind` calls `socket.getfqdn()` between
`bind()` and `listen()`. On macOS 15 and later that reverse lookup can stall for
about 35 seconds (local network privacy, actions/runner-images#14409), leaving
the socket bound but never listening while the stall is in progress. A CI
readiness probe sees `ECONNREFUSED` for the whole stall and reports that the
fixture server did not start.

Patch the lookup before running the module: the server name it derives from the
address is only used for display, so returning the address itself is harmless
and makes `listen()` follow `bind()` immediately. The module keeps its normal
command line (`PORT --bind 127.0.0.1`) and behaviour.
"""

import runpy
import socket

socket.getfqdn = lambda name="": name
runpy.run_module("http.server", run_name="__main__")
