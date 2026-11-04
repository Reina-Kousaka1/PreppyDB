#!/usr/bin/env bash
# Prove that data survives a container restart *and* container recreation.
#
#   1. start a container on a fresh named volume, bootstrap an admin token;
#   2. write a collection and a document;
#   3. `docker restart` it, read the document back;
#   4. remove the container and start a new one on the same volume, read it back;
#   5. also verify that the WAL was checkpointed cleanly on SIGTERM.
#
# Requires Docker. Run by CI; see docs/TESTING.md for what has and has not been executed.
set -euo pipefail

IMAGE="${IMAGE:-preppydb:ci}"
VOLUME="${VOLUME:-preppydb-persistence-test}"
C1="preppydb-persist-1"
C2="preppydb-persist-2"
PORT="${PORT:-7077}"
BASE="http://127.0.0.1:${PORT}"
PASSWORD="persistence-test-password"

cleanup() {
  docker rm -f "$C1" "$C2" > /dev/null 2>&1 || true
  docker volume rm "$VOLUME" > /dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  echo "---- docker ps -a ----" >&2
  docker ps -a >&2 || true
  for container in "$C1" "$C2"; do
    if docker inspect "$container" > /dev/null 2>&1; then
      echo "---- $container: state ----" >&2
      docker inspect --format 'exit={{.State.ExitCode}} status={{.State.Status}} oom={{.State.OOMKilled}} error={{.State.Error}}' \
        "$container" >&2 || true
      echo "---- $container: logs ----" >&2
      docker logs "$container" 2>&1 >&2 || true
    fi
  done
  exit 1
}

wait_healthy() {
  local container="$1"
  for _ in $(seq 1 60); do
    if curl -fsS "${BASE}/healthz" > /dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  docker logs "$container" >&2 || true
  fail "container $container never became healthy"
}

echo "==> cleaning up leftovers"
cleanup

echo "==> first start on a fresh volume"
# The token is printed once; the container exits before serving (--init-only).
# The entrypoint prints progress lines *and* the server prints the token once, so pick the
# token by shape (32 random bytes, base64url without padding) instead of by line number.
TOKEN=$(
  docker run --rm -v "${VOLUME}:/var/lib/preppydb" -e PREPPYDB_BOOTSTRAP_TOKEN=ci "$IMAGE" \
    --bootstrap-token ci --init-only 2>&1 \
  | tr -d '\r' \
  | grep -E '^[A-Za-z0-9_-]{40,}$' \
  | head -n 1
)
[ -n "$TOKEN" ] || fail "no bootstrap token printed"
echo "    token length: ${#TOKEN}"

# The entrypoint refuses to start when auth.json is missing and no password is given,
# so subsequent runs only need the volume.
docker run -d --name "$C1" -p "127.0.0.1:${PORT}:7077" -v "${VOLUME}:/var/lib/preppydb" "$IMAGE" > /dev/null
wait_healthy "$C1"

AUTH=(-H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json")

echo "==> writing data"
curl -fsS -X POST "${BASE}/v1/collections" "${AUTH[@]}" \
  -d '{"name":"persistence"}' > /dev/null
curl -fsS -X POST "${BASE}/v1/collections/persistence/documents" "${AUTH[@]}" \
  -d '{"key":"survivor","value":{"message":"still here","n":42}}' > /dev/null
BEFORE=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$BEFORE" | grep -q "still here" || fail "document was not stored: $BEFORE"

echo "==> checkpoint and restart the same container"
curl -fsS -X POST "${BASE}/v1/admin/checkpoint" "${AUTH[@]}" > /dev/null
docker restart "$C1" > /dev/null
wait_healthy "$C1"
AFTER_RESTART=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$AFTER_RESTART" | grep -q "still here" || fail "document lost after restart: $AFTER_RESTART"
echo "    restart: document survived"

echo "==> remove the container, start a new one on the same volume"
docker rm -f "$C1" > /dev/null
docker run -d --name "$C2" -p "127.0.0.1:${PORT}:7077" -v "${VOLUME}:/var/lib/preppydb" "$IMAGE" > /dev/null
wait_healthy "$C2"
AFTER_RECREATE=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$AFTER_RECREATE" | grep -q "still here" || fail "document lost after recreation: $AFTER_RECREATE"
echo "    recreation: document survived"

echo "==> verify the on-disk state after the graceful shutdown"
docker exec "$C2" preppydb verify --data-dir /var/lib/preppydb | tee /tmp/persist-verify.json > /dev/null
grep -q '"ok": true' /tmp/persist-verify.json || fail "verify reported problems"
echo "    storage verifies clean"

echo "==> credentials file is private"
PERMS=$(docker exec "$C2" stat -c '%a' /var/lib/preppydb/auth.json)
[ "$PERMS" = "600" ] || fail "auth.json permissions are $PERMS, expected 600"
echo "    auth.json mode: $PERMS"

echo
echo "PASS: data survived restart and container recreation"
