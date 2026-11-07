# Testing

This page states exactly what is tested, what was executed while building version 0.1.0,
and what remains unverified. If something below is not backed by an executed command, it
is marked as such.

## Environment used for the checks in this document

| Item | Value |
| --- | --- |
| Host | Linux x86-64 sandbox (no Docker daemon, no Windows, no ARM64 hardware) |
| Compiler | rustc 1.99.0, cargo 1.99.0 |
| Formatting | rustfmt 1.10.0-stable |
| Linting | clippy 0.1.99 |
| Lua | Embedded Lua 5.5 through `lupa`; real Luvit 2.18.1 also used for live tests on 2026-11-05 |
| Server-side Lua | bundled Lua 5.4 through `mlua` (`lua54`, `vendored`); exercised by the Rust server unit/API integration suites |
| Dependencies | Direct runtime dependencies `getrandom`, `ctrlc`, and `mlua` are pinned in `Cargo.lock`; first build needs crates.io or a Cargo cache |

## Test inventory

| Suite | Location | What it covers |
| --- | --- | --- |
| JSON (18) | `crates/preppydb-json` | parsing, escapes, numbers, canonical form, ordering, rejects invalid input |
| Crypto (30) | `crates/preppydb-crypto` | SHA-256 vectors, HMAC, PBKDF2, CRC32C, base64 round-trips, hex, RNG |
| Storage unit (86) | `crates/preppydb-storage/src/**` | records, catalog, schema validation, WAL framing/CRC/truncation, snapshot blocks, manifest, locks, clock, backup format |
| Storage integration (37 + 1 helper) | `crates/preppydb-storage/tests/engine_integration.rs` | durability, checkpoint/replay and publication crash windows, indexes across restart, transactions (commit/conflict/rollback), batch atomicity across restart, TTL, **crash recovery with a real `abort()` in a child process**, torn-tail truncation, terminal and mid-log corruption, post-checkpoint sequence gaps, repair suffix cleanup, manifest path/metadata/symlink validation, backup/restore round-trip |
| Pricing (10) | `crates/preppydb-pricing` | bounds, clamping, discounts, seeded determinism, offer expiry/staleness/double-redeem, ledger immutability, audit tamper detection |
| Model (5) | `crates/preppydb-model` | 14 collections declared, no name collisions with forbidden names, schemas validate, bootstrap idempotence |
| Rust client (5 + 1 doc) | `crates/preppydb-client` | URL parsing, https rejection, error mapping (`is_retryable`, `code`), request building |
| API integration (12) | `crates/preppydb-server/tests/api_integration.rs` | real HTTP server on an ephemeral port: public endpoints, authn/authz, CRUD + CAS, schema rejection, batch atomicity, body/rate limits, routing, login/session tokens, pricing flow end-to-end, simulation determinism, personality model + admin endpoints, index queries |
| Lua client (30 + 70 + live) | `clients/luvit/tests/*.lua` | JSON codec, request construction, error mapping, retries, async transport contract, and a **live** end-to-end suite against a running server (40 checks) |

Total in the Rust workspace: **170 passed, 0 failed, 1 ignored** (the ignored test is the
child-process helper used by the crash test, spawned explicitly).

## Commands executed (Linux x86-64, 2026-11-04)

```bash
cargo fmt --all --check      # clean
cargo clippy --workspace --all-targets --locked   # 0 warnings
cargo test --workspace --locked                   # 170 passed, 0 failed, 1 ignored
cargo build --workspace --release --locked        # success
```

Per crate:

```
preppydb-json:    18 passed, 0 failed
preppydb-crypto:  30 passed, 0 failed
preppydb-storage: 67 passed, 0 failed, 1 ignored   (46 unit + 21 integration)
preppydb-pricing: 10 passed, 0 failed
preppydb-model:    5 passed, 0 failed
preppydb-client:   6 passed, 0 failed             (5 unit + 1 doc test)
preppydb-server:  34 passed, 0 failed             (22 lib + 12 API integration)
preppydb-cli:      0 tests (verified manually, see below)
```

### Manual verification performed

* **Server smoke test** — started `preppydb-server` on `127.0.0.1:7077` with a bootstrap
  token and exercised, with `curl`: `/healthz`, `/readyz`, `/v1/version`, `/v1/stats`
  (401 without token), collection creation, `PUT`/`GET` documents, schema rejection,
  compare-and-set conflict (`409`, `retryable: false`), batch atomicity, scan, forbidden economy
  collection (`403`), personality bootstrap (14 collections), price rule + tick, offer →
  purchase → double-redeem rejection, delete of an immutable `purchases` record (`403`),
  audit verification, checkpoint, verify, and the web UI (`/`, `/assets/app.js` → 200).
* **CLI end-to-end** — `init`, `token create`, `collections`, `put`/`get` (including a
  schema rejection with exit code 1), `scan`, `stats`, `verify`, `checkpoint`, `compact`,
  `backup`, `backup-verify`, `restore` into a fresh directory, `stats` on the restored
  copy, `pricing simulate`, `wal`, `audit verify`, `health` (exit 0 healthy, exit 1
  unreachable).
* **Lua client live suite** — `python3 clients/luvit/tests/run_with_lupa.py --live` against
  a running server: 40 checks, covering auth errors, schema validation, unicode keys and
  values, CAS conflicts, batch atomicity, pricing tick/offer/purchase, and cleanup.
* **Configuration handling** — `--print-config` reflects a config file; an unknown key is
  rejected with a non-zero exit; binding to `0.0.0.0` without `allow_public_bind` is
  refused with an explanatory message.
* **Read-only mode** — a server started with `--read-only` reports
  `{"ready":true,"read_only":true}` and rejects writes with `403 read_only`.
* **Container entrypoint** (without Docker) — `docker/entrypoint.sh` exits 64 with
  instructions when no credential exists and none is configured, creates the first admin
  token when `PREPPYDB_BOOTSTRAP_TOKEN` is set, starts the server for `0.0.0.0:…`
  (adding `--allow-public-bind`), and the resulting server passes `preppydb health`.
* **Committed tree** — a fresh `git clone` of the branch builds and passes the same
  `cargo test --workspace --locked` run (170 passed), proving no source, asset or lockfile
  is missing from the commit.
* **Process-level crash recovery** — `bash scripts/crash-recovery-test.sh`: a real server
  wrote 200 documents in acknowledged batches, was killed with `kill -9` (no checkpoint,
  no graceful shutdown), restarted on the same directory, reported the same 200 documents,
  served sampled documents intact, then shut down cleanly via SIGTERM and SIGINT and
  passed `preppydb verify`.

## What was NOT executed

The original 2026-11-04 build sandbox had no Docker daemon, Windows host, or ARM64 hardware.
The following GitHub Actions statuses refer to the previously committed PR head, not the
current uncommitted worktree. This workspace has no attached Raspberry Pi; ARM64 CI is
emulated.

| Item | State | How it is covered |
| --- | --- | --- |
| Linux x86-64 fmt/clippy/tests, crash recovery, release build | **green on previous PR head** | `linux-x86_64` job; current worktree has separate local results below |
| Windows x86-64 build + full test suite + PowerShell smoke | **green on previous PR head** | `windows-x86_64` job; current worktree still needs GitHub qualification |
| Docker image build (amd64 + arm64) | **green on previous PR head** | `docker-images` job, build only — never pushed |
| `scripts/docker-persistence-test.sh` (restart + recreation) | **green on previous PR head** | `docker-persistence` job; current worktree has separate local results below |
| Linux ARM64 build and emulated tests | CI only | `linux-arm64` job under `qemu-user`; this is emulation, **not** real ARM64 hardware. |
| `docker-compose.yml` | not started | the persistence test drives `docker run` directly |
| `luvit` runtime | not run in the baseline checks | The baseline suite used embedded Lua with substituted transport; current worktree results are listed below. |
| Long-running soak / memory profiling | not run | out of scope for this phase |
| Replication | not implemented | later module |

Note for the emulated job: password verification is PBKDF2-HMAC-SHA-256 with 120k
iterations in portable Rust, which `qemu-user` executes roughly two orders of magnitude
slower than native code. Tests that log in therefore use an explicit, generous client
timeout (`with_timeout`) instead of the 10 s default.

Workflow: <https://github.com/Reina-Kousaka1/PreppyDB/actions/workflows/ci.yml>.

## Additional local qualification of the uncommitted hardening work

The checks below were run against the current local worktree on 2026-11-05. They do
not describe the older committed PR head and do not substitute for GitHub Actions on the
eventual pushed commit.

| Check | Local result | Boundary |
| --- | --- | --- |
| Linux x86-64 formatting, workspace check, strict Clippy, tests, release build, crash recovery | **passed** | Latest Ruby-feature workspace run reported 252 passed, 0 failed, and 1 ignored helper; the helper was explicitly spawned and passed by the crash test. |
| Linux ARM64 cross-build and QEMU tests | **passed** | Emulated environment; the child-process crash helper is skipped by the same setting used in CI. |
| Windows GNU workspace check and strict Clippy | **passed** | Cross-target compile/lint only; no Windows runtime tests and no MSVC qualification. |
| Docker AMD64 + ARM64 image build | **passed** | Local buildx OCI export; nothing was pushed. |
| Docker persistence test with Ruby query planning enabled | **passed** | Authenticated HTTP query used the image's `/usr/bin/ruby`, Rust plan validation and indexed execution; restart, recreation, verify and cleanup also passed. |
| Ruby → Rust → Lua → Rust composition test | **passed** | Read-authorized indexed query supplies a versioned document to a separate admin-authorized Lua request; its CAS write is committed by the same Rust storage engine. These are separate requests, not one cross-runtime transaction. |
| Real Luvit transport and HTTP client | **passed** | CI-pinned Luvi plus official Lit/Luvit Git commits built locally; normal and oversized response tests passed, followed by 40 real-server checks. |

The runtime image now includes Debian's Ruby runtime because the experimental query
endpoint accepts an administrator-configured Ruby executable. The feature remains off
by default. Its container smoke test is part of `scripts/docker-persistence-test.sh`.

The Lua live suite substitutes only the transport: request construction, JSON codec,
response handling and error mapping are the real Lua code. This is called out in
`clients/luvit/README.md` as well.

## Reproducing

```bash
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Lua client (no luvit needed)
python3 -m pip install lupa
python3 clients/luvit/tests/run_with_lupa.py

# Lua client against a live server
DATA=$(mktemp -d)
./target/debug/preppydb-server --data-dir "$DATA" --bootstrap-token ci --init-only > /tmp/init.txt
TOKEN=$(sed -n '2p' /tmp/init.txt)
./target/debug/preppydb-server --data-dir "$DATA" --listen 127.0.0.1:7077 &
PREPPYDB_URL=http://127.0.0.1:7077 PREPPYDB_TOKEN="$TOKEN" \
  python3 clients/luvit/tests/run_with_lupa.py --live

# With luvit installed
PREPPYDB_URL=http://127.0.0.1:7077 PREPPYDB_TOKEN="$TOKEN" luvit clients/luvit/tests/test_live.lua

# Process-level crash recovery (needs curl + a built binary)
bash scripts/crash-recovery-test.sh

# Docker persistence (needs Docker)
docker build -f docker/Dockerfile -t preppydb:ci .
IMAGE=preppydb:ci bash scripts/docker-persistence-test.sh
```

## How the important tests work

* **Crash recovery** (`crash_recovery_keeps_committed_writes`): spawns a child process
  that writes 100 documents with `sync_mode = always` and then calls
  `std::process::abort()`. The parent asserts the child failed, reopens the directory and
  checks that all 100 documents are present and that the last one has the expected value.
* **Torn tail** (`torn_tail_is_truncated_and_reported`): appends a half-written frame to
  the WAL on disk, reopens, and asserts the committed prefix is intact and a recovery
  report was written.
* **Mid-log corruption** (`mid_log_corruption_is_rejected_by_default`): corrupts a frame
  in the middle of a segment; startup must fail rather than silently drop data, and
  `RepairPolicy::TruncateCorruptTail` must be required to proceed.
* **Transaction conflicts**: commit two transactions that read the same key, assert the
  second fails with `TransactionConflict` and that none of its writes landed.
* **API tests**: start a real `http::serve` on `127.0.0.1:0`, talk raw HTTP and through
  the Rust client; the server is stopped via a shutdown flag in `Drop`.
* **Pricing audit**: mutate an entry of the audit log on disk and assert `verify` fails.

## Commit scaling smoke benchmark

Run `cargo run --release -p preppydb-storage --example commit_scaling --locked` to
compare repeated updates of one existing document in stores containing 1,000, 10,000,
and 50,000 documents. The example reports median and p95 latency over 101 writes per
store. It uses `sync_mode = never` to isolate in-process commit work from fsync latency;
it is not a durability benchmark and its results are not cross-machine performance
claims.

Two local runs on Ubuntu under WSL2 (Linux 6.18.33.2, x86_64, 12th Gen Intel Core
i5-12450H, rustc 1.99.1, release profile) reported median times of 2,146/2,522/2,579 ns
and 2,141/2,566/2,615 ns respectively for the three store sizes. The corresponding p95
times were 2,339/3,624/2,764 ns and 2,282/2,805/2,876 ns. This is a smoke check that
ordinary single-document commit cost does not grow with a full-store copy; use repeated
runs on the target host for any deployment-specific capacity decision.

## Gaps worth closing next

1. Qualify the current local diff on GitHub Actions, obtain Windows MSVC/runtime evidence, and run on a real Raspberry Pi before declaring those platforms qualified.
2. Add a `luvit` job with the real `http` transport (the CI job tries and tolerates its
   absence today).
3. Property-based tests for the JSON parser and the schema validator (fuzzing a parser by
   hand is not enough).
4. A soak test: 24 h of mixed reads/writes with periodic checkpoints, watching RSS and
   segment growth.
5. Cross-version upgrade test (0.1.0 → next) with a real data directory.

## Raspberry Pi 4 hardware validation

**Real hardware has not yet been tested.** CI #107 and #109 build and test ARM64 under QEMU; that is emulation, not a Raspberry Pi. CI #109 also cross-builds the `preppydb-server` and `preppydb` release binaries from the exact PR-head commit, verifies their AArch64 ELF metadata, smoke-invokes both under QEMU, and publishes a commit-named artifact for 30 days. The artifact manifest identifies the source commit, target, compiler and required glibc baseline (2.35). It is suitable for 64-bit Raspberry Pi OS Bookworm or newer; older glibc systems must not run it. [CI #109](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42411562670) passed for commit `fa6126d8c49ec80fa8aba92e4faa26355c97825f`.

### Run the bounded hardware smoke test

1. Wait for the CI run for the exact commit to finish successfully. Download `preppydb-linux-arm64-gnu-<full-commit-sha>` from that run's artifacts. The downloaded GitHub artifact is an outer ZIP containing `preppydb-linux-arm64-gnu.tar.gz` and its `.sha256` file.
2. Extract that outer ZIP on the workstation, then transfer only the tarball and checksum file to a new staging directory on the Pi (for example by authorized SCP or removable media). The Pi commands below print the unique staging path; place both files there before running the checksum and extraction commands. On the Pi, use a clean clone of the same commit:

```bash
STAGE="$(mktemp -d /var/tmp/preppydb-artifact.XXXXXXXX)"
mkdir -m 700 "$STAGE/extracted"
printf 'Copy the tarball and checksum into: %s\n' "$STAGE"
(cd "$STAGE" && sha256sum -c preppydb-linux-arm64-gnu.tar.gz.sha256)
tar -xzf "$STAGE/preppydb-linux-arm64-gnu.tar.gz" -C "$STAGE/extracted"
git clone --branch arena/146caad2-preppydb --single-branch \
  https://github.com/Reina-Kousaka1/PreppyDB.git "$STAGE/source"
git -C "$STAGE/source" checkout --detach <full-commit-sha>
cd "$STAGE/source"
```

The script requires Bash, Git, curl, util-linux `prlimit`, and standard Linux command-line tools already present on the Pi. It installs nothing and does not require Rust. Replace `<full-commit-sha>` with the commit encoded in the artifact name and manifest.
3. Run the smoke script as an unprivileged user:

```bash
PREPPYDB_ARTIFACT_DIR="$STAGE/extracted" \
  bash scripts/rpi4-hardware-smoke.sh
```

The script refuses non-aarch64 hosts, non-Pi-4 device trees, containers, dirty or mismatched checkouts, and glibc older than 2.35. It validates artifact checksums and the commit match before creating a unique mode-700 test directory under `/var/tmp`. It uses one synthetic collection and one document, a loopback listener on an OS-assigned port, a single controlled server-process kill/reopen to exercise WAL recovery, then backup, backup verification, restore into a new temporary path, and a read-back check. It never reboots the Pi, edits boot/kernel/Docker/cgroup configuration, or touches an existing database path or service.

The server and CLI child processes receive per-process limits (`RLIMIT_AS` 512 MiB, CPU 30 seconds, file size 32 MiB, 128 open files) and 60-second deadlines by default. The script checks available RAM and free `/var/tmp` space before work and samples RAM, disk, and the active PreppyDB process each second; it terminates the test if a threshold is crossed. `MIN_MEM_AVAILABLE_MIB` cannot be lowered below 768 MiB, and `MIN_DISK_FREE_MIB` cannot be lowered below 512 MiB. The generated database is removed only after a successful run and only from the runner's own uniquely named, marked temporary directory. Failures retain that directory for diagnosis; `KEEP_TEST_DATA=1` retains it after success as well.

**These limits do not provide aggregate memory isolation.** `RLIMIT_AS` applies per process, and host resource sampling is not a reservation. Therefore this runner is deliberately only a tiny smoke test. Large write loops, long soak tests, repeated crash campaigns, and full Cargo builds/tests remain blocked on the Pi until they can be run under reliable host-level resource isolation. The one-process SIGKILL check is not a power-loss test and does not prove sudden-power-loss durability.

Record the CI run URL, artifact commit, Pi model and RAM size, OS/glibc/kernel, available resources before/after, output and resource samples in the hardware test report. Until that run is completed, leave this section explicitly marked untested; passing QEMU and CI jobs do not qualify Raspberry Pi hardware or establish production readiness. Windows directory-flush guarantees, an external pricing-audit anchor, independent review, and other PR hardening gates remain separate.
