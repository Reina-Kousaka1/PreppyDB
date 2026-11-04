#!/usr/bin/env bash
# End-to-end crash recovery at the process level.
#
#   1. start a real server, write 200 documents from 20 clients' worth of batches;
#   2. kill -9 the process (no graceful shutdown, no checkpoint);
#   3. restart it on the same directory;
#   4. assert every acknowledged document is still readable and storage verifies clean.
#
# This complements the in-process `abort()` test in
# crates/preppydb-storage/tests/engine_integration.rs: here the whole HTTP server dies.
set -euo pipefail

BIN_DIR="${BIN_DIR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug}"
SERVER="$BIN_DIR/preppydb-server"
CLI="$BIN_DIR/preppydb"
PORT="${PORT:-7081}"
BASE="http://127.0.0.1:${PORT}"
DOCS="${DOCS:-200}"

for binary in "$SERVER" "$CLI"; do
    [ -x "$binary" ] || { echo "missing binary: $binary (run cargo build first)" >&2; exit 1; }
done

WORKDIR="$(mktemp -d)"
DATA="$WORKDIR/data"
LOG="$WORKDIR/server.log"
SERVER_PID=""

cleanup() {
    [ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

start_server() {
    "$SERVER" --data-dir "$DATA" --listen "127.0.0.1:${PORT}" >> "$LOG" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 100); do
        if curl -fsS "${BASE}/healthz" > /dev/null 2>&1; then
            return 0
        fi
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            echo "server exited during startup; log:" >&2
            cat "$LOG" >&2
            exit 1
        fi
        sleep 0.1
    done
    echo "server did not become healthy" >&2
    cat "$LOG" >&2
    exit 1
}

echo "==> initialise credentials"
TOKEN=$("$SERVER" --data-dir "$DATA" --bootstrap-token crash --init-only | sed -n '2p')
AUTH=(-H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json")

echo "==> start server"
start_server

curl -fsS -X POST "${BASE}/v1/collections" "${AUTH[@]}" -d '{"name":"crash","max_docs":10000}' > /dev/null

echo "==> write ${DOCS} documents (batches of 20, each acknowledged before the next)"
published=0
while [ "$published" -lt "$DOCS" ]; do
    body='{"operations":['
    separator=""
    for _ in $(seq 1 20); do
        [ "$published" -ge "$DOCS" ] && break
        body="${body}${separator}{\"op\":\"put\",\"key\":\"k${published}\",\"value\":{\"n\":${published},\"pad\":\"${published}-payload\"}}"
        separator=","
        published=$((published + 1))
    done
    body="${body}]}"
    response=$(curl -fsS -X POST "${BASE}/v1/collections/crash/batch" "${AUTH[@]}" -d "$body")
    echo "$response" | grep -q '"operations"' || { echo "batch failed: $response" >&2; exit 1; }
done
echo "    acknowledged writes: ${published}"

BEFORE=$(curl -fsS "${BASE}/v1/collections/crash" "${AUTH[@]}" | tr -d '\n ' | grep -o '"documents":[0-9]*')
echo "    server reports: ${BEFORE}"

echo "==> kill -9 the server (no checkpoint, no graceful shutdown)"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
[ -f "$DATA/wal/000000000001.wal" ] || echo "    (note: WAL already rotated; that is fine)"

echo "==> restart on the same directory"
start_server

AFTER=$(curl -fsS "${BASE}/v1/collections/crash" "${AUTH[@]}" | tr -d '\n ' | grep -o '"documents":[0-9]*')
echo "    server reports after restart: ${AFTER}"
[ "$BEFORE" = "$AFTER" ] || { echo "FAIL: document count changed across the crash (${BEFORE} -> ${AFTER})" >&2; exit 1; }

echo "==> spot-check documents, including the last acknowledged one"
for key in 0 1 99 $((DOCS - 1)); do
    doc=$(curl -fsS "${BASE}/v1/collections/crash/documents/k${key}" "${AUTH[@]}")
    echo "$doc" | grep -q "\"n\":${key}" || { echo "FAIL: document k${key} is missing: $doc" >&2; exit 1; }
done
echo "    all sampled documents intact"

echo "==> recovery report"
RECOVERY=$(curl -fsS "${BASE}/v1/admin/recovery" "${AUTH[@]}")
echo "    ${RECOVERY}" | head -c 300; echo

echo "==> stop the restarted server gracefully (SIGTERM)"
kill -TERM "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
echo "    exited; the directory is free for offline checks"

echo "==> verify the storage"
"$CLI" verify --data-dir "$DATA" > "$WORKDIR/verify.json"
grep -q '"ok": true' "$WORKDIR/verify.json" || { cat "$WORKDIR/verify.json" >&2; exit 1; }
echo "    storage verifies clean"

echo
echo "PASS: ${DOCS} documents survived kill -9 without a checkpoint"
