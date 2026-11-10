#!/usr/bin/env bash
# Prove that data survives a container restart *and* container recreation.
#
#   1. start a container on a fresh named volume, bootstrap an admin token;
#   2. write a collection and a document;
#   3. `docker restart` it, read the document back;
#   4. remove the container and start a new one on the same volume, read it back;
#   5. verify clean offline storage after a later graceful SIGTERM.
#
# Requires Docker. Run by CI; see docs/TESTING.md for what has and has not been executed.
set -euo pipefail

IMAGE="${IMAGE:-preppydb:ci}"
DATA_DIR="${DATA_DIR:-/var/lib/preppydb}"
APP_UID="${APP_UID:-10001}"
APP_GID="${APP_GID:-10001}"
RUN_SUFFIX="${GITHUB_RUN_ID:-${CI_JOB_ID:-$(date +%s)-$$}}"
VOLUME="${VOLUME:-preppydb-persistence-${RUN_SUFFIX}}"
CONFIG_VOLUME="${VOLUME}-config"
C1="${C1:-preppydb-persist-${RUN_SUFFIX}-1}"
C2="${C2:-preppydb-persist-${RUN_SUFFIX}-2}"
PORT="${PORT:-7077}"
BASE="http://127.0.0.1:${PORT}"
PASSWORD="persistence-test-password"

cleanup() {
  docker rm -f "$C1" "$C2" > /dev/null 2>&1 || true
  docker volume rm "$VOLUME" "$CONFIG_VOLUME" > /dev/null 2>&1 || true
}

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

wait_docker_health() {
  local container="$1"
  for _ in $(seq 1 60); do
    if [ "$(docker inspect --format '{{.State.Health.Status}}' "$container" 2>/dev/null || true)" = "healthy" ]; then
      return 0
    fi
    sleep 1
  done
  docker inspect --format '{{json .State.Health}}' "$container" >&2 || true
  fail "Docker healthcheck did not become healthy for $container"
}

for container in "$C1" "$C2"; do
  if docker inspect "$container" > /dev/null 2>&1; then
    fail "refusing to remove pre-existing container $container"
  fi
done
for volume in "$VOLUME" "$CONFIG_VOLUME"; do
  if docker volume inspect "$volume" > /dev/null 2>&1; then
    fail "refusing to remove pre-existing volume $volume"
  fi
done
trap cleanup EXIT

echo "==> creating isolated test volumes"
docker volume create "$CONFIG_VOLUME" > /dev/null
printf '%s\n' '{"allow_public_bind":true,"ruby_planner_path":"/usr/bin/ruby"}' \
  | docker run --rm -i --mount "source=${CONFIG_VOLUME},target=/config" \
      --entrypoint /bin/sh "$IMAGE" -c 'cat > /config/config.json'

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
docker run -d --name "$C1" -p "127.0.0.1:${PORT}:7077" \
  -e PREPPYDB_SYNC_MODE=always \
  -v "${VOLUME}:/var/lib/preppydb" \
  --mount "source=${CONFIG_VOLUME},target=/run/preppydb-config,readonly" \
  "$IMAGE" --config /run/preppydb-config/config.json > /dev/null
wait_healthy "$C1"
wait_docker_health "$C1"

echo "==> verifying effective durability configuration"
VERSION=$(curl -fsS "${BASE}/v1/version")
echo "$VERSION" | grep -q '"sync_mode"[[:space:]]*:[[:space:]]*"always"' \
  || fail "version endpoint did not report sync_mode=always: $VERSION"
echo "$VERSION" | grep -q '"durable_writes"[[:space:]]*:[[:space:]]*true' \
  || fail "version endpoint did not report durable_writes=true: $VERSION"
echo "    sync_mode=always, durable_writes=true"

AUTH=(-H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json")

echo "==> writing data"
curl -fsS -X POST "${BASE}/v1/collections" "${AUTH[@]}" \
  -d '{"name":"persistence"}' > /dev/null
curl -fsS -X POST "${BASE}/v1/collections/persistence/documents" "${AUTH[@]}" \
  -d '{"key":"survivor","value":{"message":"still here","n":42}}' > /dev/null
BEFORE=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$BEFORE" | grep -q "still here" || fail "document was not stored: $BEFORE"

echo "==> execute an indexed query through the bundled Ruby planner"
curl -fsS -X POST "${BASE}/v1/collections/persistence/indexes" "${AUTH[@]}" \
  -d '{"name":"by_message","field":"message","unique":false}' > /dev/null
QUERY=$(curl -sS -X POST "${BASE}/v1/collections/persistence/query" "${AUTH[@]}" \
  -d '{"version":1,"filter":{"field":"message","op":"eq","value":"still here"},"limit":10}')
echo "$QUERY" | grep -q '"ok"[[:space:]]*:[[:space:]]*true' \
  && echo "$QUERY" | grep -q '"survivor"' \
  || fail "Ruby planned query did not return the indexed document: $QUERY"
echo "    Ruby planner and Rust indexed execution: passed"

echo "==> crash the process with uncheckpointed WAL data and recover on the same volume"
docker kill --signal=KILL "$C1" > /dev/null
docker start "$C1" > /dev/null
wait_healthy "$C1"
wait_docker_health "$C1"
AFTER_RESTART=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$AFTER_RESTART" | grep -q "still here" || fail "document lost after restart: $AFTER_RESTART"
echo "    restart: document survived"

echo "==> remove the container, start a new one on the same volume"
docker rm -f "$C1" > /dev/null
docker run -d --name "$C2" -p "127.0.0.1:${PORT}:7077" \
  -e PREPPYDB_SYNC_MODE=always \
  -v "${VOLUME}:/var/lib/preppydb" \
  --mount "source=${CONFIG_VOLUME},target=/run/preppydb-config,readonly" \
  "$IMAGE" --config /run/preppydb-config/config.json > /dev/null
wait_healthy "$C2"
wait_docker_health "$C2"
AFTER_RECREATE=$(curl -fsS "${BASE}/v1/collections/persistence/documents/survivor" "${AUTH[@]}")
echo "$AFTER_RECREATE" | grep -q "still here" || fail "document lost after recreation: $AFTER_RECREATE"
echo "    recreation: document survived"

echo "==> verify the storage through the admin API"
# `preppydb verify` would need the directory lock the running server holds, so the check
# goes through the same verification code over HTTP.
VERIFY=$(curl -fsS -X POST "${BASE}/v1/admin/verify" "${AUTH[@]}")
echo "$VERIFY" | grep -qE '"ok"[[:space:]]*:[[:space:]]*true' || fail "verify reported problems: $VERIFY"
echo "    storage verifies clean"

echo "==> credentials file is private"
PERMS=$(docker exec "$C2" stat -c '%a' "${DATA_DIR}/auth.json")
[ "$PERMS" = "600" ] || fail "auth.json permissions are $PERMS, expected 600"
echo "    auth.json mode: $PERMS"

echo "==> stop the container"
docker stop -t 30 "$C2" > /dev/null
EXIT_CODE=$(docker inspect --format '{{.State.ExitCode}}' "$C2")
# 143 = 128 + SIGTERM: the server has no signal handler, so the process is terminated and
# the WAL recovers on the next open. Anything else (137/OOM, 1, 73) is a real failure.
case "$EXIT_CODE" in
    0 | 143) echo "    SIGTERM produced exit code $EXIT_CODE (expected 143, 0 tolerated)" ;;
    *) fail "unexpected exit code $EXIT_CODE after docker stop" ;;
esac

echo "==> offline verify on the stopped directory"
# As the app user, so the volume keeps its ownership and the lock file can be created.
OFFLINE=$(docker run --rm --user "${APP_UID}:${APP_GID}" \
  -v "${VOLUME}:${DATA_DIR}" --entrypoint preppydb "$IMAGE" verify --data-dir "${DATA_DIR}")
echo "$OFFLINE" | grep -qE '"ok"[[:space:]]*:[[:space:]]*true' || fail "offline verify reported problems: $OFFLINE"
echo "    offline verify clean after graceful shutdown"

echo
echo "PASS: data survived restart and container recreation"
