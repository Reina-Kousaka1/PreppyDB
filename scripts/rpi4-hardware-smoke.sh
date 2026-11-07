#!/usr/bin/env bash
# Small, resource-limited smoke test for a real Raspberry Pi 4.
# Uses the exact ARM64 binaries uploaded by the matching GitHub Actions run.
set -Eeuo pipefail
umask 077

EXPECTED_BRANCH="arena/146caad2-preppydb"
GLIBC_MIN="2.35"
MIN_MEM_AVAILABLE_MIB="${MIN_MEM_AVAILABLE_MIB:-1024}"
MIN_DISK_FREE_MIB="${MIN_DISK_FREE_MIB:-1024}"
ADDRESS_SPACE_MIB="${ADDRESS_SPACE_MIB:-512}"
CPU_LIMIT_SECONDS="${CPU_LIMIT_SECONDS:-30}"
WALL_LIMIT_SECONDS="${WALL_LIMIT_SECONDS:-60}"
MAX_FILE_MIB="${MAX_FILE_MIB:-32}"

die() { echo "FAIL: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"; }

for command in awk cat chmod curl date df dirname getconf git grep head id mktemp prlimit ps \
    rm seq sed sha256sum sleep sort tail tr uname; do
    need "$command"
done

[[ "${EUID:-$(id -u)}" -ne 0 ]] || die "run as an unprivileged Pi user, not root"
[[ "$MIN_MEM_AVAILABLE_MIB" =~ ^[0-9]+$ && "$MIN_MEM_AVAILABLE_MIB" -ge 768 ]] ||
    die "MIN_MEM_AVAILABLE_MIB must be numeric and at least 768"
[[ "$MIN_DISK_FREE_MIB" =~ ^[0-9]+$ && "$MIN_DISK_FREE_MIB" -ge 512 ]] ||
    die "MIN_DISK_FREE_MIB must be numeric and at least 512"
[[ "$ADDRESS_SPACE_MIB" =~ ^[0-9]+$ && "$ADDRESS_SPACE_MIB" -ge 256 && "$ADDRESS_SPACE_MIB" -le 768 ]] ||
    die "ADDRESS_SPACE_MIB must be between 256 and 768"
[[ "$CPU_LIMIT_SECONDS" =~ ^[0-9]+$ && "$CPU_LIMIT_SECONDS" -ge 1 && "$CPU_LIMIT_SECONDS" -le 30 ]] ||
    die "CPU_LIMIT_SECONDS must be between 1 and 30"
[[ "$WALL_LIMIT_SECONDS" =~ ^[0-9]+$ && "$WALL_LIMIT_SECONDS" -ge 10 && "$WALL_LIMIT_SECONDS" -le 60 ]] ||
    die "WALL_LIMIT_SECONDS must be between 10 and 60"
[[ "$MAX_FILE_MIB" =~ ^[0-9]+$ && "$MAX_FILE_MIB" -ge 1 && "$MAX_FILE_MIB" -le 32 ]] ||
    die "MAX_FILE_MIB must be between 1 and 32"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
[[ -d "$REPO_ROOT/.git" ]] || die "run from a full PreppyDB checkout"
HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD)"
BRANCH="$(git -C "$REPO_ROOT" branch --show-current)"
[[ "$BRANCH" == "$EXPECTED_BRANCH" || -z "$BRANCH" ]] ||
    die "expected $EXPECTED_BRANCH or a detached commit; found $BRANCH"
[[ -z "$(git -C "$REPO_ROOT" status --porcelain)" ]] ||
    die "checkout has local changes; use a separate clean clone"

ARCH="$(uname -m)"
[[ "$ARCH" == "aarch64" ]] || die "native aarch64 required; found $ARCH"
MODEL="$(tr -d '\000' < /proc/device-tree/model 2>/dev/null || true)"
[[ "$MODEL" == *"Raspberry Pi 4"* ]] || die "device tree is not a Raspberry Pi 4: ${MODEL:-unavailable}"
[[ ! -e /.dockerenv && ! -e /run/.containerenv ]] || die "containers are not supported for this hardware smoke test"
if grep -Eqi '(docker|containerd|podman|kubepods)' /proc/1/cgroup 2>/dev/null; then
    die "container cgroup detected; run directly on the Pi host"
fi

GLIBC_VERSION="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
[[ -n "$GLIBC_VERSION" ]] || die "could not identify the host glibc version"
[[ "$(printf '%s\n' "$GLIBC_MIN" "$GLIBC_VERSION" | sort -V | head -n 1)" == "$GLIBC_MIN" ]] ||
    die "artifact requires glibc >= $GLIBC_MIN; host has $GLIBC_VERSION"

ARTIFACT_DIR="${PREPPYDB_ARTIFACT_DIR:-}"
[[ -n "$ARTIFACT_DIR" && -d "$ARTIFACT_DIR" ]] ||
    die "set PREPPYDB_ARTIFACT_DIR to the extracted matching CI artifact"
SERVER="$ARTIFACT_DIR/preppydb-server"
CLI="$ARTIFACT_DIR/preppydb"
MANIFEST="$ARTIFACT_DIR/manifest.txt"
[[ -x "$SERVER" && -x "$CLI" && -f "$MANIFEST" && -f "$ARTIFACT_DIR/SHA256SUMS" ]] ||
    die "artifact must contain preppydb-server, preppydb, manifest.txt, and SHA256SUMS"
(cd "$ARTIFACT_DIR" && sha256sum --check --status SHA256SUMS) || die "binary checksum verification failed"
MANIFEST_COMMIT="$(sed -n 's/^commit=//p' "$MANIFEST")"
[[ "$HEAD" == "$MANIFEST_COMMIT" ]] ||
    die "checkout commit $HEAD does not match artifact commit $MANIFEST_COMMIT"
grep -q '^target=aarch64-unknown-linux-gnu$' "$MANIFEST" || die "artifact target is not Linux ARM64 GNU"
grep -q '^glibc_minimum=2.35$' "$MANIFEST" || die "unexpected artifact glibc baseline"
TEST_ROOT="$(mktemp -d /var/tmp/preppydb-arm64-smoke.XXXXXXXX)" || die "cannot create isolated /var/tmp directory"
case "$TEST_ROOT" in /var/tmp/preppydb-arm64-smoke.*) ;; *) die "unexpected test directory path" ;; esac
chmod 700 "$TEST_ROOT"
printf 'PreppyDB synthetic hardware smoke data\n' > "$TEST_ROOT/.owned-by-rpi4-smoke"
DATA="$TEST_ROOT/database"
RESTORED="$TEST_ROOT/restored"
ARCHIVE="$TEST_ROOT/synthetic-backup.tar"
SERVER_LOG="$TEST_ROOT/server.log"
SAMPLES="$TEST_ROOT/resources.tsv"
TOKEN_FILE="$TEST_ROOT/test-token"
mkdir -m 700 "$TEST_ROOT/tmp"
export TMPDIR="$TEST_ROOT/tmp"
SERVER_PID=""
LIMITED_PID=""
MONITOR_PID=""
MAIN_PID="$$"
SUCCESS=0
printf 'utc\tmem_available_mib\tdisk_free_mib\tserver_cpu_pct\tserver_rss_kib\n' > "$SAMPLES"

read_mem_available_mib() {
    awk '/^MemAvailable:/ {print int($2 / 1024)}' /proc/meminfo
}
read_disk_free_mib() {
    df -Pm /var/tmp | awk 'NR==2 {print $4}'
}
check_resources() {
    local mem disk
    mem="$(read_mem_available_mib)"
    disk="$(read_disk_free_mib)"
    [[ "$mem" =~ ^[0-9]+$ && "$disk" =~ ^[0-9]+$ ]] || die "could not read available system resources"
    (( mem >= MIN_MEM_AVAILABLE_MIB )) || die "only ${mem} MiB RAM is available; minimum is ${MIN_MEM_AVAILABLE_MIB} MiB"
    (( disk >= MIN_DISK_FREE_MIB )) || die "only ${disk} MiB disk is available; minimum is ${MIN_DISK_FREE_MIB} MiB"
}
check_resources

AS_BYTES=$((ADDRESS_SPACE_MIB * 1024 * 1024))
FILE_BYTES=$((MAX_FILE_MIB * 1024 * 1024))
run_limited() {
    local result=0
    check_resources
    prlimit --as="$AS_BYTES" --cpu="$CPU_LIMIT_SECONDS" \
        --fsize="$FILE_BYTES" --nofile=128 -- "$@" &
    LIMITED_PID=$!
    monitor_server "$LIMITED_PID" &
    MONITOR_PID=$!
    if wait "$LIMITED_PID"; then result=0; else result=$?; fi
    LIMITED_PID=""
    stop_monitor
    check_resources
    return "$result"
}

stop_pid() {
    local pid="$1"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
}
stop_server() {
    stop_pid "$SERVER_PID"
    SERVER_PID=""
}
cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$MONITOR_PID" ]]; then
        kill "$MONITOR_PID" 2>/dev/null || true
        wait "$MONITOR_PID" 2>/dev/null || true
    fi
    stop_pid "$LIMITED_PID"
    stop_server
    if [[ "$SUCCESS" == 1 && "${KEEP_TEST_DATA:-0}" != 1 ]]; then
        if [[ "$TEST_ROOT" == /var/tmp/preppydb-arm64-smoke.* && -f "$TEST_ROOT/.owned-by-rpi4-smoke" ]]; then
            rm -rf -- "$TEST_ROOT"
        fi
    else
        echo "diagnostics_retained=$TEST_ROOT" >&2
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'echo "FAIL: resource guard stopped the smoke test" >&2; exit 75' USR1

monitor_server() {
    local pid="$1" elapsed=0 mem disk cpu rss
    while kill -0 "$pid" 2>/dev/null; do
        mem="$(read_mem_available_mib)"
        disk="$(read_disk_free_mib)"
        read -r cpu rss < <(ps -p "$pid" -o pcpu=,rss= 2>/dev/null || echo '0 0')
        printf '%s\t%s\t%s\t%s\t%s\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$mem" "$disk" "${cpu:-0}" "${rss:-0}" >> "$SAMPLES"
        if [[ ! "$mem" =~ ^[0-9]+$ || ! "$disk" =~ ^[0-9]+$ ]] ||
            (( mem < MIN_MEM_AVAILABLE_MIB || disk < MIN_DISK_FREE_MIB || elapsed >= WALL_LIMIT_SECONDS )); then
            echo "resource guard: mem=${mem:-unknown}MiB disk=${disk:-unknown}MiB elapsed=${elapsed}s" >&2
            kill -TERM "$pid" 2>/dev/null || true
            kill -USR1 "$MAIN_PID" 2>/dev/null || true
            return
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
}

start_server() {
    : > "$SERVER_LOG"
    prlimit --as="$AS_BYTES" --cpu="$CPU_LIMIT_SECONDS" \
        --fsize="$FILE_BYTES" --nofile=128 -- \
        "$SERVER" --data-dir "$1" --listen 127.0.0.1:0 >> "$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
    monitor_server "$SERVER_PID" &
    MONITOR_PID=$!
    local port=""
    for _ in $(seq 1 100); do
        check_resources
        kill -0 "$SERVER_PID" 2>/dev/null || { cat "$SERVER_LOG" >&2; die "server exited during startup"; }
        port="$(sed -n 's/^PreppyDB .* listening on http:\/\/127\.0\.0\.1:\([0-9][0-9]*\)$/\1/p' "$SERVER_LOG" | tail -n 1)"
        if [[ -n "$port" ]] && curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then
            BASE_URL="http://127.0.0.1:$port"
            return
        fi
        sleep 0.2
    done
    cat "$SERVER_LOG" >&2
    die "isolated smoke server did not become healthy"
}
stop_monitor() {
    if [[ -n "$MONITOR_PID" ]]; then
        kill "$MONITOR_PID" 2>/dev/null || true
        wait "$MONITOR_PID" 2>/dev/null || true
        MONITOR_PID=""
    fi
}
get_document() {
    curl -fsS "$BASE_URL/v1/collections/pi_smoke/documents/only-one" \
        -H "Authorization: Bearer $TOKEN"
}

echo "hardware=$MODEL"
echo "architecture=$ARCH"
echo "glibc=$GLIBC_VERSION"
echo "commit=$HEAD"
echo "artifact=$(grep '^target=' "$MANIFEST")"
echo "memory_available_before=$(read_mem_available_mib) MiB"
echo "disk_free_before=$(read_disk_free_mib) MiB"
echo "limits=as:${ADDRESS_SPACE_MIB}MiB,cpu:${CPU_LIMIT_SECONDS}s,wall:${WALL_LIMIT_SECONDS}s,file:${MAX_FILE_MIB}MiB"
echo "scope=one synthetic document; no build, soak, load loop, reboot, cgroup, or production path"

run_limited "$SERVER" --data-dir "$DATA" --bootstrap-token pi-smoke --init-only \
    > "$TEST_ROOT/init.out"
TOKEN="$(awk '/^[A-Za-z0-9_-]{40,}$/ {print; exit}' "$TEST_ROOT/init.out")"
[[ -n "$TOKEN" ]] || die "test-only bootstrap token was not generated"
printf '%s\n' "$TOKEN" > "$TOKEN_FILE"
chmod 600 "$TOKEN_FILE"
start_server "$DATA"
curl -fsS -X POST "$BASE_URL/v1/collections" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"name":"pi_smoke","max_docs":10}' >/dev/null
curl -fsS -X POST "$BASE_URL/v1/collections/pi_smoke/documents" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"key":"only-one","value":{"source":"synthetic","n":1}}' >/dev/null
[[ "$(get_document)" == *'"source":"synthetic"'* ]] || die "synthetic write/read smoke failed"
echo "single_synthetic_write=PASS"

echo "controlled_process_crash=SIGKILL (isolated server only; no host reboot or power interruption)"
stop_monitor
kill -KILL "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
start_server "$DATA"
[[ "$(get_document)" == *'"n":1'* ]] || die "single acknowledged document did not survive server-process recovery"
echo "wal_recovery_after_process_kill=PASS"

stop_monitor
stop_server
run_limited "$CLI" verify --data-dir "$DATA" >/dev/null
run_limited "$CLI" backup --data-dir "$DATA" --out "$ARCHIVE" >/dev/null
run_limited "$CLI" backup-verify --archive "$ARCHIVE" >/dev/null
run_limited "$CLI" restore --archive "$ARCHIVE" --target "$RESTORED" >/dev/null
run_limited "$CLI" verify --data-dir "$RESTORED" >/dev/null
echo "backup_restore_verify=PASS"

run_limited "$SERVER" --data-dir "$RESTORED" --bootstrap-token pi-restore --init-only \
    > "$TEST_ROOT/restore-init.out"
TOKEN="$(awk '/^[A-Za-z0-9_-]{40,}$/ {print; exit}' "$TEST_ROOT/restore-init.out")"
[[ -n "$TOKEN" ]] || die "restored test-only bootstrap token was not generated"
printf '%s\n' "$TOKEN" > "$TOKEN_FILE"
chmod 600 "$TOKEN_FILE"
start_server "$RESTORED"
[[ "$(get_document)" == *'"source":"synthetic"'* ]] || die "restored synthetic document is unreadable"
echo "restored_document_read=PASS"
stop_monitor
stop_server
echo "resource_samples_begin"
cat "$SAMPLES"
echo "resource_samples_end"
echo "memory_available_after=$(read_mem_available_mib) MiB"
echo "disk_free_after=$(read_disk_free_mib) MiB"
echo "result=PASS"
SUCCESS=1
