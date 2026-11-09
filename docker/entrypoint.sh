#!/bin/sh
# Container entrypoint for PreppyDB.
#
# Responsibilities, in order:
#   1. make sure the data directory exists and is writable;
#   2. create the first admin credential *once* if the database has none yet;
#   3. exec the server so that `docker stop` reaches it directly (tini forwards SIGTERM).
#
# Deliberately no default password: if no credential exists and neither
# PREPPYDB_ADMIN_PASSWORD nor PREPPYDB_BOOTSTRAP_TOKEN is set, the container refuses to
# start and explains what to do. That is safer than shipping "admin/admin".
set -eu

DATA_DIR="${PREPPYDB_DATA_DIR:-/var/lib/preppydb}"
LISTEN="${PREPPYDB_LISTEN:-0.0.0.0:7077}"
APP_UID="${PREPPYDB_APP_UID:-10001}"
APP_GID="${PREPPYDB_APP_GID:-10001}"

# A named volume mounted over /var/lib/preppydb is created root-owned, so the
# unprivileged user could not write to it. Take ownership once (not recursively — the
# files in it belong to the app user already) and re-exec as that user.
if [ "$(id -u)" = "0" ]; then
    mkdir -p "$DATA_DIR"
    if [ "$(stat -c %u "$DATA_DIR")" != "$APP_UID" ]; then
        echo "preppydb: taking ownership of $DATA_DIR for uid $APP_UID"
        chown "$APP_UID:$APP_GID" "$DATA_DIR"
    fi
    exec gosu "$APP_UID:$APP_GID" "$0" "$@"
fi

mkdir -p "$DATA_DIR"
if [ ! -w "$DATA_DIR" ]; then
    echo "preppydb: $DATA_DIR is not writable by uid $(id -u)" >&2
    echo "preppydb: fix the ownership of the mounted volume (chown 10001:10001)" >&2
    exit 73
fi

if [ ! -f "$DATA_DIR/auth.json" ]; then
    if [ -n "${PREPPYDB_ADMIN_PASSWORD:-}" ]; then
        ADMIN_USER="${PREPPYDB_ADMIN_USER:-admin}"
        echo "preppydb: creating admin user '$ADMIN_USER' (one-time initialisation)"
        preppydb-server --data-dir "$DATA_DIR" --init-admin "$ADMIN_USER" --init-only
    elif [ -n "${PREPPYDB_BOOTSTRAP_TOKEN:-}" ]; then
        echo "preppydb: creating bootstrap API token '$PREPPYDB_BOOTSTRAP_TOKEN' (printed once below)"
        preppydb-server --data-dir "$DATA_DIR" --bootstrap-token "$PREPPYDB_BOOTSTRAP_TOKEN" --init-only
    else
        cat >&2 <<'MESSAGE'
preppydb: no credentials found and none configured, refusing to start.

Create the first admin credential once, either with a password:

    docker run --rm -v preppydb-data:/var/lib/preppydb \
        -e PREPPYDB_ADMIN_PASSWORD='at-least-12-characters' \
        preppydb:local ...

or with an API token that is printed a single time:

    docker run --rm -v preppydb-data:/var/lib/preppydb \
        -e PREPPYDB_BOOTSTRAP_TOKEN=initial-admin \
        preppydb:local ...

After that the container starts normally; the variables are only used when
$DATA_DIR/auth.json does not exist yet.
MESSAGE
        exit 64
    fi
fi

# Durability policy. `always` is the only mode in which an acknowledged write has been
# handed to the device with fsync; a second database that an economy adapter writes to
# must run in it, so it is the default here. The server has no CLI flag for sync mode, so
# it is handed over in a generated config file; --data-dir and --listen below still take
# precedence over anything in that file.
SYNC_MODE="${PREPPYDB_SYNC_MODE:-always}"
case "$SYNC_MODE" in
    always|never|interval) ;;
    interval:*)
        INTERVAL_VALUE="${SYNC_MODE#interval:}"
        case "$INTERVAL_VALUE" in
            ""|*[!0-9]*)
                echo "preppydb: PREPPYDB_SYNC_MODE '$SYNC_MODE' needs a millisecond value" >&2
                exit 64
                ;;
        esac
        ;;
    *)
        echo "preppydb: PREPPYDB_SYNC_MODE must be always, never, interval or interval:<ms>" >&2
        exit 64
        ;;
esac
RUNTIME_CONFIG="${TMPDIR:-/tmp}/preppydb-runtime-config.json"
printf '{"sync_mode":"%s"}\n' "$SYNC_MODE" > "$RUNTIME_CONFIG"
set -- "$@" --config "$RUNTIME_CONFIG"

# Bind address 0.0.0.0 inside a container is fine (the container network is the trust
# boundary), but the server only allows a non-loopback bind with an explicit flag.
case "$LISTEN" in
    127.*|localhost*|"[::1]"*) ;;
    *) set -- "$@" --allow-public-bind ;;
esac

if [ "${PREPPYDB_READ_ONLY:-0}" = "1" ]; then
    set -- "$@" --read-only
fi

# `exec` keeps PID 1 semantics through tini so SIGTERM reaches the server.
exec preppydb-server --data-dir "$DATA_DIR" --listen "$LISTEN" "$@"
