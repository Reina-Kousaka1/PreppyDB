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
* The entrypoint creates the first admin credential once — from
  `PREPPYDB_ADMIN_PASSWORD` (user via `PREPPYDB_ADMIN_USER`, default `admin`) or from
  `PREPPYDB_BOOTSTRAP_TOKEN` — and then never touches `auth.json` again. Without either,
  the container exits with code 64 and an explanation. There is **no default password**.
* `docker stop` sends SIGTERM through `tini`; the server finishes in-flight requests,
  checkpoints and closes the WAL before exiting (grace period 30 s in the compose file).

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

The workspace has no third-party dependencies, so the build also works fully offline
(`cargo build --offline`).

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
TimeoutStopSec=30
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

`SIGTERM` and `SIGINT` both trigger a graceful shutdown (checkpoint + close).

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
  "workers": 4
}
```

`preppydb-server --data-dir … --print-config` prints the effective configuration and
exits; unknown keys in the file are rejected so typos cannot silently change behaviour.

| Key | Meaning |
| --- | --- |
| `listen` | `host:port`; a non-loopback address additionally requires `allow_public_bind: true` |
| `max_body_bytes` | request body cap, 1 KiB … 256 MiB (default 4 MiB) |
| `rate_limit_per_second` / `_burst` | per-credential limit; `0` disables it; burst must be ≥ rate |
| `sync_mode` | `always` (fsync every write, default), `interval` (fsync at most every 200 ms), `never` (tests only) |
| `read_only` | refuse all writes; the TTL sweeper must then be disabled |
| `cors_origins` | extra allowed origins for browser clients (same origin is always allowed) |
| `pricing_seed` | fixed seed for reproducible pricing; `null` = OS entropy |
| `ttl_sweep_interval_ms` | background expiry sweep; `null` disables it |
| `checkpoint_every_mutations` | automatic checkpoint threshold; `null` disables it |
| `repair` | `strict` (abort on mid-WAL corruption) or `truncate` (destructive, for recovery) |
| `auth_path` / `pricing_audit_path` | override the default locations inside the data dir |
| `workers` | HTTP worker threads (1–512) |

### Sync modes

| Mode | Guarantee |
| --- | --- |
| `always` | a completed write is on stable storage; survives a power cut |
| `interval` | `fsync` at most every 200 ms; a crash can lose the last few writes |
| `never` | no explicit `fsync`: relies on the OS page cache — tests and benchmarks only |

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

```bash
# Archive (takes a checkpoint first unless --no-checkpoint)
preppydb backup --data-dir ./data --out /backups/preppy-$(date +%F).bak

# Verify an archive without touching anything
preppydb backup-verify --archive /backups/preppy-2026-11-04.bak

# Restore into a new directory (existing targets are moved aside, never deleted)
preppydb restore --archive /backups/preppy-2026-11-04.bak --target /srv/preppydb-restored
```

The archive contains a header, every file with a SHA-256, and a trailing whole-file
SHA-256. `restore` verifies every checksum **before** writing anything; a corrupt archive
leaves the target untouched. Restoring over a live directory requires `--force`, which
moves the old data to `<target>.replaced-<timestamp>` instead of deleting it.

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

Power loss or a kill -9 leaves the directory recoverable:

1. On start the engine replays WAL segments after the last snapshot.
2. A torn final record is truncated and reported in
   `<data-dir>/recovery-report-<ms>.json` and via `GET /v1/admin/recovery`.
3. Corruption in the *middle* of a WAL aborts startup (`strict`). Restoring a backup is
   the right answer. If the tail is known to be junk and the operator accepts the loss,
   run any engine-opening command with `--repair`, e.g.
   `preppydb checkpoint --data-dir ./data --repair` (or start the server with
   `"repair": "truncate"`). `preppydb wal` only *reports* what it finds.

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
