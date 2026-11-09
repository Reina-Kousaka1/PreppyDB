#!/usr/bin/env python3
"""Run the Lua client test suite without luvit installed.

This executes the *real* Lua sources in `clients/luvit/` with an embedded Lua 5.5
interpreter (the `lupa` package) and swaps only the transport:

  * `test_json.lua` and `test_client.lua` need no network at all and run unchanged.
  * `test_live.lua` talks to a real PreppyDB server, but its HTTP calls are performed
    by Python's `http.client` through the injected `PREPPYDB_TRANSPORT`.

That means the JSON codec, URL/body construction, response handling and error
mapping are exercised for real; only the luvit `http` transport itself is not. The
luvit path is covered by running the same file with `luvit` (see README).

Usage:
    python3 clients/luvit/tests/run_with_lupa.py                 # offline suites only
    PREPPYDB_URL=http://127.0.0.1:7077 PREPPYDB_TOKEN=... \\
        python3 clients/luvit/tests/run_with_lupa.py --live      # include live tests
"""

from __future__ import annotations

import argparse
import http.client
import os
import sys
import urllib.parse

try:
    from lupa import LuaRuntime
except ImportError:  # pragma: no cover - dependency hint
    sys.exit("lupa is required: pip install lupa")

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
CLIENT_DIR = os.path.join(REPO_ROOT, "clients", "luvit")


def make_transport():
    """A synchronous Lua transport that performs real HTTP requests."""

    def transport(request, callback):
        url = urllib.parse.urlsplit(request.url)
        body = request.body
        headers = {}
        lua_headers = request.headers
        for key in lua_headers.keys():
            headers[key] = lua_headers[key]
        # lupa hands Lua strings over as `str`; HTTP needs UTF-8 bytes.
        if isinstance(body, str):
            body = body.encode("utf-8")
        connection = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=20)
        try:
            path = url.path or "/"
            if url.query:
                path += "?" + url.query
            connection.request(request.method, path, body=body, headers=headers)
            response = connection.getresponse()
            payload = response.read().decode("utf-8", "replace")
            callback(response.status, payload, {})
        except Exception as exc:  # noqa: BLE001 - mirror the transport contract
            callback(None, f"python transport error: {exc}", {})
        finally:
            connection.close()

    return transport


def run(script: str) -> int:
    lua = LuaRuntime(unpack_returned_tuples=True)
    lua.execute(f'package.path = "{CLIENT_DIR}/?.lua;{CLIENT_DIR}/?/init.lua;" .. package.path')
    lua.globals().PREPPYDB_TRANSPORT = make_transport()
    # `os.exit` would terminate the Python process, so turn it into a catchable error.
    lua.execute(
        "os.exit = function(code) error('LUA_EXIT:' .. tostring(code or 0), 0) end"
    )
    os.chdir(REPO_ROOT)
    try:
        lua.execute(f'dofile("{script}")')
    except Exception as exc:  # noqa: BLE001 - lupa surfaces Lua errors as exceptions
        message = str(exc)
        marker = message.find("LUA_EXIT:")
        if marker >= 0:
            code = message[marker + len("LUA_EXIT:"):].strip().split()[0]
            return 0 if code == "0" else 1
        print(f"{script}: {message}", file=sys.stderr)
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--live", action="store_true", help="also run the live server suite")
    parser.add_argument("--only", help="run a single test file (json, client, live)")
    options = parser.parse_args()

    scripts = []
    if options.only in (None, "json"):
        scripts.append("clients/luvit/tests/test_json.lua")
    if options.only in (None, "client"):
        scripts.append("clients/luvit/tests/test_client.lua")
    if options.only == "live" or (options.live and options.only is None):
        if not os.environ.get("PREPPYDB_URL") or not os.environ.get("PREPPYDB_TOKEN"):
            print("--live needs PREPPYDB_URL and PREPPYDB_TOKEN", file=sys.stderr)
            return 2
        scripts.append("clients/luvit/tests/test_live.lua")

    failures = 0
    for script in scripts:
        failures += run(script)
    if failures:
        print(f"{failures} Lua test script(s) failed", file=sys.stderr)
        return 1
    print("all Lua test scripts passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
