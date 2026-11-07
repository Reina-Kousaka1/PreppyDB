# Operations

Everything an operator needs: install, configure, back up, restore, upgrade, and what to
do when something looks wrong.

## 1. Install

### Docker / Podman (Linux amd64 or arm64)

```bash
export PREPPYDB_ADMIN_PASSWORD='at-least-12-characters'
docker compose up -d --build
```

* Image: `docker/Dockerfile`, multi-stage, `--platform linux/amd64,linux/arm64`.
* Runtime user: uid/gid `10001`, no shell, no capabilities added, `no-new-privileges`.
* Data: `/var/lib/preppydb` (a declared `VOLUME`), owner `10001:10001`, mode `0700`.
* The container starts as root *only* to take ownership of a freshly created
  (root-owned) volume, then `exec gosu 10001:10001` before the database starts — the
  server itself never runs as root. With a bind mount that is already writable by uid
  10001 you can also pass `--user 10001:10001` and skip this step entirely.
* The entrypoint creates the first admin credential once — from
  `PREPPYDB_ADMIN_PASSWORD` (user via `PREPPYDB_ADMIN_USER`, default `admin`) or from
  `PREPPYDB_BOOTSTRAP_TOKEN` — and then never touches `auth.json` again. Without either,
  the container exits with code 64 and an explanation. There is **no default password**.
* `docker stop` sends SIGTERM through `tini`. The server handles SIGTERM and SIGINT through
  `ctrlc`, stops accepting new connections, finishes in-flight requests, wakes the TTL
  sweeper, and shuts down the engine. Compose allows 35 seconds for that drain.

Arm64 without emulation:

```bash
docker buildx build --platform linux/arm64 -f docker/Dockerfile -t preppydb:arm64 .
```

> The Docker image was **not** built in the environment where this code was written
> (no Docker daemon available). See [TESTING.md](TESTING.md#what-was-not-executed) — the
> Dockerfile and CI jobs are written, but treat the first build as unverified.

### Native binary

Requires Rust 1.89+ (the repository pins `stable` in `rust-toolchain.toml`; the
single-writer lock uses the stabilised `File::try_lock`).

```bash
cargo build --release
sudo install -m 0755 target/release/preppydb-server /usr/local/bin/
sudo install -m 0755 target/release/preppydb        /usr/local/bin/
```

The workspace has two focused runtime dependencies, `getrandom` for OS entropy and
`ctrlc` for signal handling. The first build needs these locked crates cached or available
from crates.io; subsequent builds can run offline.

#### Linux with systemd

```ini
# /etc/systemd/system/preppydb.service
[Unit]
Description=PreppyDB
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=preppydb
Group=preppydb
# First run only: create the admin credential and exit.
# ExecStartPre=/usr/local/bin/preppydb-server --data-dir /var/lib/preppydb --init-admin admin --init-only
ExecStart=/usr/local/bin/preppydb-server --data-dir /var/lib/preppydb --config /etc/preppydb/config.json
Restart=on-failure
RestartSec=2
TimeoutStopSec=35
StateDirectory=preppydb
StateDirectoryMode=0700
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/preppydb

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home /var/lib/preppydb --shell /usr/sbin/nologin preppydb
sudo install -d -o preppydb -g preppydb -m 0700 /var/lib/preppydb
sudo -u preppydb PREPPYDB_ADMIN_PASSWORD='…' /usr/local/bin/preppydb-server \
     --data-dir /var/lib/preppydb --init-admin admin --init-only
sudo systemctl daemon-reload && sudo systemctl enable --now preppydb
```

SIGINT, SIGTERM and SIGHUP enter the graceful shutdown path: the server stops accepting
new connections, finishes in-flight requests, wakes the TTL sweeper and shuts down the
storage engine. SIGKILL cannot be handled; the WAL recovery path still protects
acknowledged writes, which `scripts/crash-recovery-test.sh` checks. For a clean checkpoint
before a maintenance window, call `POST /v1/admin/checkpoint`.

#### Raspberry Pi 4 / ARM64 Linux

Build natively on the Pi (`cargo build --release`, ~6–10 minutes) or cross-compile:

```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt-get install -y gcc-aarch64-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

Use a USB SSD rather than an SD card for the data directory: the engine calls `fsync` on
every mutation by default, which SD cards handle poorly. If the workload tolerates it,
`"sync_mode": "interval"` reduces write amplification at the cost of durability of
the last few writes (see §3).

#### Windows x86-64

```powershell
cargo build --release
# First run: create the admin user (password from the environment)
$env:PREPPYDB_ADMIN_PASSWORD = "at-least-12-characters"
.\target\release\preppydb-server.exe --data-dir C:\PreppyDB\data --init-admin admin --init-only
# Serve
.\target\release\preppydb-server.exe --data-dir C:\PreppyDB\data --listen 127.0.0.1:7077
```

* The credential file inherits NTFS ACLs of its directory; restrict it to the service
  account (`icacls C:\PreppyDB\data /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F"`).
* Run as a Windows service with [NSSM](https://nssm.cc/) or `sc.exe` if needed:
  `nssm install PreppyDB C:\PreppyDB\preppydb-server.exe --data-dir C:\PreppyDB\data`.
* Paths with spaces must be quoted; the CLI accepts both `C:\data` and `C:/data`.
* On Windows, Ctrl+C/Ctrl+Break request a clean drain and shutdown. On Unix, SIGINT,
  SIGTERM and SIGHUP do the same. Windows has no POSIX SIGTERM event; service managers should send
  the supported console control event. The compose example allows 35 seconds for active
  requests to finish.

## 2. Configuration

Precedence: CLI flag > config file > default.

```json
{
  "listen": "127.0.0.1:7070",
  "data_path": "/var/lib/preppydb",
  "allow_public_bind": false,
  "max_body_bytes": 4194304,
  "rate_limit_per_second": 50,
  "rate_limit_burst": 100,
  "sync_mode": "always",
  "read_only": false,
  "cors_origins": [],
  "pricing_seed": null,
  "ttl_sweep_interval_ms": 60000,
  "checkpoint_every_mutations": 10000,
  "repair": "strict",
  "auth_path": null,
  "pricing_audit_path": null,
  "ruby_planner_path": null,
  "lua_execution_enabled": false,
  "workers": 4
}
```

`preppydb-server --data-dir … --print-config` prints the effective configuration and
exits; unknown keys in the file are rejected so typos cannot silently change behaviour.

| Key | Meaning |
| --- | --- |
| `listen` | `host:port`; a non-loopback address additionally requires `allow_public_bind: true` |
| `max_body_bytes` | request body cap, 1 KiB … 256 MiB (default 4 MiB) |
| `rate_limit_per_second` / `_burst` | per-credential limit; anonymous requests and login attempts are keyed by client IP (port-independent); `0` disables it; burst must be ≥ rate |
| `sync_mode` | `always` (sync every write, default), `interval` (background sync attempt every 200 ms), `never` (tests only) |
| `read_only` | refuse all writes; the TTL sweeper must then be disabled |
| `ruby_planner_path` | `null` by default; an absolute, trusted Ruby executable path enables experimental document queries. The Docker image includes Debian's Ruby runtime; set this to `/usr/bin/ruby` only when enabling the feature intentionally. The Docker persistence test exercises the authenticated Ruby-plan → Rust-validation/execution path. |
| `lua_execution_enabled` | `false` by default; enables the bounded, admin-only Lua mutation endpoint |
| `cors_origins` | extra allowed origins for browser clients (same origin is always allowed) |
| `pricing_seed` | fixed seed for reproducible pricing; `null` = OS entropy |
| `ttl_sweep_interval_ms` | background expiry sweep; `null` disables it |
| `checkpoint_every_mutations` | automatic checkpoint threshold; `null` disables it |
| `repair` | `strict` (abort on mid-WAL corruption) or `truncate` (destructive, for recovery) |
| `auth_path` / `pricing_audit_path` | override the default locations inside the data dir |
| `workers` | HTTP worker threads (1–512) |

When enabled, `POST /v1/lua/execute` runs one caller-supplied Lua chunk without a
database handle, then validates and atomically submits its `put`/`delete` result through
the normal storage batch path. It is not a trigger/function catalog and provides no
Lua-side database reads. See [API.md](API.md#controlled-lua-mutation-scripts) for
runtime limits and the exact return schema. Keep it disabled unless this execution
capability is intentionally granted to administrators.

### Sync modes

| Mode | Guarantee |
| --- | --- |
| `always` | a write is acknowledged only after `sync_data` succeeds. This depends on the OS, filesystem, device, and controller honoring flush requests; it is not a universal power-loss guarantee. |
| `interval` | A background worker attempts `sync_data` every configured interval (200 ms for `interval`). Scheduling and I/O can delay an attempt; commits are acknowledged before it, so writes since the last successful sync may be lost after an OS, hardware, or power failure. A process-only crash usually leaves the OS page cache available, but is not used to claim durability. A failed sync poisons the engine and subsequent reads/writes fail until reopen. Graceful shutdown performs a final sync. |
| `never` | no explicit WAL sync; tests and benchmarks only. |

If a WAL append or sync fails after a write may have reached the log, the operation
returns `commit_outcome_unknown`. The engine rolls back its in-memory mutation, enters a
failed state, and rejects subsequent document reads and writes until it is closed and
reopened. Recovery then determines whether the complete record was present. Callers
must inspect the reopened state before retrying; a returned I/O error in this window is
not proof that the write was absent.

## 3. Routine operations

### Health and readiness

```bash
preppydb health --url http://127.0.0.1:7070           # exit 0 healthy, 1 unhealthy
curl -s localhost:7070/healthz
curl -s localhost:7070/readyz
curl -s localhost:7070/v1/stats  -H "Authorization: Bearer $TOKEN"
curl -s localhost:7070/v1/metrics -H "Authorization: Bearer $TOKEN"
```

`/healthz` answers as long as the process serves HTTP. `/readyz` additionally performs a
storage operation and reports `503` when the engine cannot serve — that is the endpoint to
wire into a load balancer.

### Credentials

The credential file is limited to 16 MiB. Files exceeding that limit, invalid UTF-8,
and read errors prevent authentication rather than retaining cached credentials.
Existing installations with larger credential files must reduce their size before
upgrading. Changes invalidate existing in-memory sessions.

The Rust client accepts at most 16 MiB per response body by default and at most
16 KiB for the status line and headers together. Applications expecting larger
responses can set a body limit with `Client::with_max_response_bytes`; responses
without a valid single `Content-Length` or with `Transfer-Encoding` are rejected.

```bash
preppydb token create --data-dir ./data --name ci --role write --expires-days 30
preppydb token list   --data-dir ./data
preppydb token revoke --data-dir ./data --id tok_abcd1234
preppydb user create  --data-dir ./data --username molly --role write   # password from stdin
preppydb user disable --data-dir ./data --username molly
```

Token plaintext is printed exactly once; only a SHA-256 digest is stored (`auth.json`,
mode 0600). Sessions are memory-only and end on restart.

### Backups

Backup holds an engine read lock while it copies and verifies the archive, so writes
and checkpoints wait until it finishes. It writes to a unique temporary sibling and
verifies both archive checksums and the recoverability of the embedded database before
publication. This adds a read and temporary extraction of the database copy, requiring
temporary disk space approximately equal to that copy. Choose a unique destination path
to retain prior backup generations; publication may replace an existing destination
where the filesystem's rename semantics allow it. An interrupted operation may leave a
temporary file for inspection. This does not protect against external modification of database files
outside the engine.

```bash
# Archive (takes a checkpoint first unless --no-checkpoint)
preppydb backup --data-dir ./data --out /backups/preppy-$(date +%F).bak

# Verify an archive without touching anything
preppydb backup-verify --archive /backups/preppy-2026-11-04.bak

# Restore into a new directory (existing targets are moved aside, never deleted)
preppydb restore --archive /backups/preppy-2026-11-04.bak --target /srv/preppydb-restored
```

The archive destination must be outside the source database directory. PreppyDB resolves
existing parent symlinks/reparse points and checks the destination again before
publication; concurrent hostile changes to the parent path are not protected against by
descriptor-relative filesystem operations.

The archive contains a header, every file with a SHA-256, and a trailing whole-file
SHA-256. `backup-verify` validates both the archive checksums and the embedded database
by extracting it to an exclusive temporary directory and opening/verifying it with strict
read-only recovery. Backup creation requires a writable engine that holds the database
lock; read-only handles are rejected for backup. A read-only open takes a shared lock by
default when the persistent `LOCK` file exists, so it cannot race a writer's checkpoint
or recovery cleanup. Set `take_lock=false` only for explicit live inspection; that mode
does not provide a stable cross-process view and can fail closed if files change while
opening. Databases without a `LOCK` file remain readable for compatibility but cannot be
coordinated through that file. `restore`
performs that validation, copies and rechecks each member,
then strictly opens/verifies the staged database before publishing it; an invalid archive
or database leaves the target untouched. Verification requires temporary disk space
approximately equal to the database copy. Files and staged directories are synced before
publication, and the staged directory's parent is synced before publication. A restore
reserves the target under its directory lock before writing the durable intent; on
platforms that support directory sync, a cross-directory move syncs both its destination
and source directories. Restoring over an existing target requires `--force`, which moves
the old data to `<target>.replaced-<timestamp>/previous` instead of deleting it. The
reserved directory must not already exist. Replacing an existing target uses two renames,
so a process crash between them can leave the original under that `previous` path and the
target temporarily absent. A final parent-directory sync failure returns
`restore_outcome_unknown`; inspect both the target and retained `previous` directory
before retrying. Keep the parent directory protected from untrusted local changes.
An interrupted publication leaves a sibling restore-intent file; database open fails
with `recovery_required` rather than creating an empty replacement. Inspect the intent,
target, staged directory and retained `previous` directory before removing the intent.
On Windows, restore markers and directory moves use write-through moves. The parent
directory still cannot be flushed by the current helper, so native Windows crash tests
cover process termination, not power loss; this is not a universal atomic exchange or
power-loss guarantee.

Backing up over HTTP (admin token required):

```bash
curl -s -X POST localhost:7070/v1/admin/backup \
  -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  -d '{"path":"/backups/http.bak","checkpoint":true}'
```

Stop the server or use the HTTP endpoint for a consistent copy; copying the directory
with `cp -r` while the server runs is **not** a safe backup.

### Checkpoints and compaction

```bash
preppydb checkpoint --data-dir ./data   # WAL → snapshot, returns the new generation
preppydb compact    --data-dir ./data   # sweep expired, repack snapshot, delete retired segments
preppydb sweep      --data-dir ./data   # expire documents now
preppydb verify     --data-dir ./data   # CRC + SHA-256 of manifest, snapshots and every WAL segment
```

The server does this automatically according to `checkpoint_every_mutations` and
`ttl_sweep_interval_ms`; the CLI is for maintenance windows and for a second opinion.

### Recovery

After a process crash such as `kill -9`, startup validates and replays the persisted WAL:

1. On start the engine replays WAL segments after the last snapshot.
2. A torn final record is truncated and reported in
   `<data-dir>/recovery-report-<ms>.json` and via `GET /v1/admin/recovery`.
3. A short final WAL segment header with no record frames is treated as interrupted
   segment creation and removed on writable open after the preceding WAL history is
   validated. A non-final short header or a complete invalid header fails closed;
   read-only open does not repair it.
4. A complete frame with a bad checksum, a sequence gap, or a sequence regression
   aborts startup (`strict`). If the damaged suffix is understood and its loss accepted,
   a writable open with `--repair` truncates at the first bad/discontinuous record and
   removes every later WAL segment. This is destructive repair, not a substitute for
   restoring a verified backup. For example:
   `preppydb checkpoint --data-dir ./data --repair` (or start the server with
   `"repair": "truncate"`). Recovery also refuses to open when the replayed sequence
   remains below the manifest's `last_seq` watermark. Read-only opens never perform corruption repair;
   `preppydb wal` only *reports* what it finds.

### Upgrades

1. Take a backup (`preppydb backup`).
2. Stop the old server.
3. Replace the binaries.
4. Start the server; the manifest is forward-compatible within 0.x. A downgrade is only
   supported after restoring the matching backup.
5. Run `preppydb verify` afterwards.

## 4. Security checklist for a deployment

* Bind to loopback and terminate TLS in a reverse proxy (Caddy, nginx, Traefik). The
  server speaks plain HTTP by design.
* Never expose `/v1/admin/*` to the internet without a proxy rule.
* Create per-client tokens (role `write`, expiry set) instead of sharing an admin token.
* Give the service account access to the data directory only.
* Keep `auth.json` mode 0600 (the server enforces this on write and warns otherwise).
* Set `sync_mode: "always"` for data you care about.
* Size `max_body_bytes` and collection `max_docs` for your workload; they are the first
  line of defence against accidental resource exhaustion.
* Point alerts at `/readyz` (liveness) and at `preppydb verify` (weekly cron).

## 5. Limits

| Limit | Default | Where to change |
| --- | --- | --- |
| Request body | 4 MiB | `max_body_bytes` |
| Document size | 4 MiB | per-collection `max_doc_bytes` metadata; the global default lives in `EngineConfig::limits` |
| Batch / transaction operations | 1 000 | server handler (fixed) |
| Scan / index page | 1 000 keys | `limit` query parameter |
| Collection size | per model (see [DATA-MODEL.md](DATA-MODEL.md)) | `max_docs` per collection |
| Concurrent HTTP workers | 4 | `workers` |
| Rate limit | 50/s, burst 100 | `rate_limit_per_second`, `rate_limit_burst` |

The working set lives in memory: size the host so the whole dataset fits with headroom.
Measure with your own documents (`GET /v1/stats` reports per-collection document counts and
`GET /v1/metrics` the engine counters) instead of trusting a rule of thumb.

## 6. Troubleshooting

| Symptom | Likely cause | Action |
| --- | --- | --- |
| `database_locked` (503) | another server or CLI holds the lock | stop the other process; the lock is released by the OS when it exits, so `LOCK` may remain on disk harmlessly |
| server exits with code 64 (container) | no credentials configured | set `PREPPYDB_ADMIN_PASSWORD` once, or run `--init-admin`/`--bootstrap-token` |
| `401 authentication_required` | missing `Authorization` header | pass `-H "Authorization: Bearer …"` |
| `429 rate_limited` | per-token limit exceeded | back off, honour `Retry-After`, raise the limit for trusted clients |
| `409 conflict` | concurrent edit (`expected_version` stale) | re-read, retry (the error is marked `retryable`) |
| `corrupt_data` on start | checksum failure in a snapshot/WAL | restore a backup; run `preppydb verify` on the copy |
| start fails with `refusing to bind` | non-loopback bind without opt-in | use `--allow-public-bind` deliberately, behind a proxy |
| high memory | dataset larger than RAM | add RAM, or archive old collections |

The server logs plain lines to stderr (`journalctl -u preppydb` or `docker logs preppydb`)
and prints JSON to stdout for commands such as `--print-config`. It does not rotate its
own logs; use the supervisor's rotation.
