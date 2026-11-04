#!/bin/sh
# Container entrypoint for PreppyDB.
#
# Responsibilities, in order:
#   1. make sure the data directory exists and is writable;
#   2. create the first admin credential *once* if the database has none yet;
#   3. exec the server with a PID that receives SIGTERM from `docker stop`.
#
# Deliberately no default password: if no credential exists and neither
# PREPPYDB_ADMIN_PASSWORD nor PREPPYDB_BOOTSTRAP_TOKEN is set, the container refuses to
# start and explains what to do. That is safer than shipping "admin/admin".
set -eu

DATA_DIR="${PREPPYDB_DATA_DIR:-/var/lib/preppydb}"
LISTEN="${PREPPYDB_LISTEN:-0.0.0.0:7077}"

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
