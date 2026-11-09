#!/usr/bin/env bash
set -euo pipefail

# All database files and the generated test credential are isolated here.
DATA=$(mktemp -d /tmp/preppydb-luvit-e2e.XXXXXXXX)
PID=""
cleanup() {
  if [[ -n "$PID" ]]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  case "$DATA" in /tmp/preppydb-luvit-e2e.*) rm -rf -- "$DATA" ;; *) exit 1 ;; esac
}
trap cleanup EXIT

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
SERVER=${PREPPYDB_SERVER:-./target/debug/preppydb-server}
LUVIT_BIN=${LUVIT_BIN:-luvit}
"$SERVER" --data-dir "$DATA/db" --bootstrap-token ci --init-only > "$DATA/init"
TOKEN=$(sed -n '2p' "$DATA/init")
test -n "$TOKEN"
"$SERVER" --data-dir "$DATA/db" --listen "127.0.0.1:$PORT" > "$DATA/server.log" 2>&1 &
PID=$!
ready=0
for _ in $(seq 1 100); do
  if curl -fsS "http://127.0.0.1:$PORT/healthz" > /dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$PID" 2>/dev/null; then echo "test server exited before readiness" >&2; exit 1; fi
  sleep 0.1
done
if [[ "$ready" != 1 ]]; then echo "test server did not become ready" >&2; exit 1; fi
PREPPYDB_URL="http://127.0.0.1:$PORT" PREPPYDB_TOKEN="$TOKEN" "$LUVIT_BIN" clients/luvit/tests/test_live.lua
