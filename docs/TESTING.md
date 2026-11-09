# Testing

This page states exactly what is tested, what was executed while building version 0.1.0,
and what remains unverified. If something below is not backed by an executed command, it
is marked as such.

## Historical baseline environment (2026-11-04)

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

## Current PR-head qualification (2026-11-09)

PR #1 remains open and unmerged on `arena/146caad2-preppydb` at
`09f134161a2979ef9d0ee0ff8b05ec130c7ac961`. GitHub Actions run [#164](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42577597162)
completed successfully for that exact head. Linux x86-64 (format, Clippy, tests, crash
recovery, release build, bounded resource benchmarks), Windows x86-64, ARM64/QEMU,
Lua/Luvit, and Docker build/persistence all passed. The optional controlled stress ladder
and stress-isolation jobs were skipped behind their manual approval gates; this run did not
stress a Raspberry Pi or another user-operated system. QEMU is not physical Pi hardware,
and process-kill/reopen is not power-loss durability.

Run #164's 100,000-document matched pair used Tiny on hosted Ubuntu x86-64 with deterministic
1,024-byte documents, one `group` index, and a 60-second idle window. The current merge
checkout measured 212.08 MiB process peak RSS; baseline `f0eb9c3` measured 376.39 MiB.
Run #164 also added a 100/1,000/10,000-document paired ladder: at 10,000 documents, peak
RSS was 28.95 MiB current versus 38.55 MiB baseline; the 100- and 1,000-document peaks
were effectively alike. These are single-run synthetic comparisons, not Raspberry Pi
measurements, repeated-run estimates, or capacity guarantees. Full values and workload
parameters are in [RESOURCE_STORAGE_DESIGN.md](RESOURCE_STORAGE_DESIGN.md).

| Item | State | Boundary |
| --- | --- | --- |
| Linux x86-64 format, Clippy, tests, crash harness, release build | **green on exact PR head** | CI run #164 |
| Windows x86-64 build and tests | **green on exact PR head** | CI run #164; not a power-loss qualification |
| Linux ARM64 build and tests | **green under QEMU** | CI run #164; emulation, not physical Raspberry Pi |
| Lua/Luvit tests | **green on exact PR head** | CI run #164 |
| Docker multi-architecture build and persistence | **green on exact PR head** | CI run #164; images were built, not pushed |
| 100/1,000/10,000-document paired Tiny ladder | **passed on hosted Ubuntu x86-64** | Run #164; one run per variant/count. Not a Pi measurement or capacity guarantee. |
| 100,000-document resource comparison | **passed on hosted Ubuntu x86-64** | Run #164; branch peak 212.08 MiB versus baseline 376.39 MiB. Not a Pi measurement or capacity guarantee. |
| Raspberry Pi 4B hardware qualification | **not recorded** | No physical-device result is part of these CI runs |
| 24-hour resource soak | **not run** | No result recorded |

## Historical checks not rerun on the current PR head

The original 2026-11-04 build sandbox had no Docker daemon, Windows host, or ARM64 hardware.
The table below records those historical observations only; current branch-level evidence
is in the section above.

| Item | State | How it is covered |
| --- | --- | --- |
| Linux x86-64 fmt/clippy/tests, crash recovery, release build | historical green | Earlier `linux-x86_64` job |
| Windows x86-64 build + full test suite + PowerShell smoke | historical green | Earlier `windows-x86_64` job |
| Docker image build (amd64 + arm64) | historical green | Earlier `docker-images` job, build only — never pushed |
| `scripts/docker-persistence-test.sh` (restart + recreation) | historical green | Earlier `docker-persistence` job |
| Linux ARM64 build and emulated tests | historical green under QEMU | Emulation; **not** real ARM64 hardware |
| `docker-compose.yml` | not started | the persistence test drives `docker run` directly |
| `luvit` runtime | not run in the baseline checks | The baseline used embedded Lua with substituted transport; separate historical Luvit results are listed below. |
| Long-running soak / memory profiling | not run | out of scope for this phase |
| Replication | not implemented | later module |

Note for the emulated job: password verification is PBKDF2-HMAC-SHA-256 with 120k
iterations in portable Rust, which `qemu-user` executes roughly two orders of magnitude
slower than native code. Tests that log in therefore use an explicit, generous client
timeout (`with_timeout`) instead of the 10 s default.

Workflow: <https://github.com/Reina-Kousaka1/PreppyDB/actions/workflows/ci.yml>.

## Historical local qualification of hardening changes (2026-11-05)

The checks below were run against a local worktree on 2026-11-05. They are historical
and do not substitute for the current-head GitHub Actions run above.

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

## Raspberry Pi resource benchmark

`scripts/benchmark-resources.py` runs a synthetic workload against only the server
binary supplied with `--server`. It creates a temporary data directory, binds to an
OS-assigned loopback port, seeds deterministic ~1 KiB documents, builds one scalar
index, checkpoints, adds a WAL tail, kills only that child process, verifies recovery,
and emits JSON. It records the Linux process high-water RSS, sampled per-phase RSS and
available RAM, idle CPU and thread counts, CPU, WAL/snapshot bytes, startup/recovery/
shutdown times, write/read p50 and p95, checkpoint duration, and index-build timing. The
idle observation defaults to 10 seconds and can be extended with `--idle-seconds`. The
temporary database is removed on completion.

Run the same workload with `--profile standard` and `--profile tiny` for comparison. Tiny
defaults to 500-operation batches so its 512 KiB HTTP body bound is respected; the
standard profile defaults to 1,000. To compare equivalent request shapes, set the same
`--batch-size` for both profiles.

The default is 1,000 documents. The 100,000-document profile requires
`--allow-large-test`, at least 1 GiB of free temporary disk, and is limited to 1.5 GiB
virtual address space, 2 GiB per file, 15 minutes of child CPU, 128 open files, and no
core dumps. Those per-process limits do not isolate aggregate system memory or page
cache, so do not run the large profile on shared Raspberry Pi hardware. CI now runs this
bounded profile on a hosted Ubuntu x86-64 runner; see the exact run and machine-readable
result under Current PR-head qualification above. It is not a Pi result and does not
substitute for aggregate resource isolation on shared hardware.

Example on a dedicated Linux host after building the release server:

```bash
cargo build --release --locked -p preppydb-server
python3 scripts/benchmark-resources.py \
  --server target/release/preppydb-server \
  --profile tiny --idle-seconds 30 \
  --documents 100000 --allow-large-test --seed 20261108 \
  --source-commit "$(git rev-parse HEAD)" \
  --output resource-results.json
```

Treat the resulting JSON as a measurement of that binary and host only. Compare changes
with the same binary flags, seed, OS, storage device, profile, and workload. Overall
process peak RSS is the maximum of Linux `VmHWM` and directly or periodically sampled
`VmRSS`; phase peaks are sampled every 50 ms plus explicit before/after observations at
index and checkpoint boundaries. Phase peaks remain lower bounds for short transients.
Per-process resource limits do not isolate the host or the filesystem page cache.

### Initial WSL2 profile measurements

The first local comparison used the same release build, seed `20261108`, synthetic
~926-byte documents, 500-operation batches for both profiles, and Ubuntu under WSL2
(Linux 6.18.33.2, x86_64, 12th Gen Intel Core i5-12450H, rustc 1.99.1). The measured
binary came from a dirty local working tree based on `bb220510a70ed6247010510ff3016f8dcc7af13b`;
these are exploratory measurements, not CI evidence or a Raspberry Pi qualification.

| Documents | Profile | Idle RSS | Idle CPU (30 s) | Threads at idle | Process peak RSS | RSS after checkpoint |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 100 | standard | 3.93 MiB | 0.13% | 11 | 5.56 MiB | 5.56 MiB |
| 100 | tiny | 3.81 MiB | 0.13% | 3 | 5.18 MiB | 5.18 MiB |
| 1,000 | standard | 3.95 MiB | not sampled for 30 s | 11 | 17.20 MiB | 17.20 MiB |
| 1,000 | tiny | 3.84 MiB | not sampled for 30 s | 3 | 11.45 MiB | 9.77 MiB |
| 10,000 | standard | 3.93 MiB | not sampled for 30 s | 11 | 70.04 MiB | 59.60 MiB |
| 10,000 | tiny | 3.86 MiB | not sampled for 30 s | 3 | 41.27 MiB | 25.68 MiB |

Idle CPU was measured during 30-second no-request windows only for the 100-document
comparison. The 1,000- and 10,000-document runs used a shorter idle sample, so no idle
CPU result is claimed for them. Tiny reduced idle thread count from 11 to 3 and lowered
the observed 10,000-document process peak, but that peak still slightly exceeds the
provisional 40 MiB target. The working set remains fully in memory; none of these
measurements establish a safe maximum dataset size. Re-run on a real Pi with its actual
storage and workload before making deployment capacity decisions.

### Snapshot recovery loader comparison (exploratory)

Two matched 10,000-document runs compared release binaries from baseline commit
`6a3beef` with the local change that decodes verified snapshot frames directly into a
private `Store`. Both used `tiny`, 500-operation batches, seed `20261108`, a 10-second
idle window and the same Ubuntu WSL2 host. This is a local synthetic comparison, not Pi
or CI qualification.

| Metric | Baseline runs | Direct-to-Store runs |
| --- | ---: | ---: |
| Idle RSS | 3.85–3.86 MiB | 3.75–3.82 MiB |
| Idle CPU | 0.0–0.1% of one core | 0.1% of one core |
| Threads at idle | 3 | 3 |
| Process RSS high-water mark | 41.22–45.61 MiB | 38.85–43.86 MiB |
| RSS after checkpoint | 25.67–25.68 MiB | 25.55–25.63 MiB |
| Recovery startup | 0.125–0.204 s | 0.119–0.207 s |

The median process high-water mark decreased from 43.42 MiB to 41.36 MiB across these
two runs. Recovery-phase samples varied between runs, so they do not establish a
recovery-only peak reduction. The structural change removes the temporary full
`SnapshotData` document vector from engine open/verify; the active documents and indexes
remain resident, and no bounded-cache or disk-backed document store has been added.
The sample is small and does not demonstrate the ≤40 MiB target at 10,000 documents.

The committed direct-to-Store build (`26b8f359e409da87faa90bb19af2a8385458eecc`) was
also measured once at each documented scale on the same host with a 10-second idle
window. These figures are an additional current-build observation, not a Pi capacity
claim:

| Documents | Idle RSS | Idle CPU (10 s) | Threads | Process peak RSS | Checkpoint RSS | Recovery phase RSS | Recovery startup | Process CPU (whole run) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 3.87 MiB | 0.1% | 3 | 5.27 MiB | 5.09 MiB | 5.23 MiB | 0.111 s | 0.16% |
| 1,000 | 3.86 MiB | 0.0% | 3 | 11.74 MiB | 9.78 MiB | 7.11 MiB | 0.103 s | 0.65% |
| 10,000 | 3.81 MiB | 0.1% | 3 | 44.07 MiB | 25.64 MiB | 35.90 MiB | 0.121 s | 1.80% |

Two local runs on Ubuntu under WSL2 (Linux 6.18.33.2, x86_64, 12th Gen Intel Core
i5-12450H, rustc 1.99.1, release profile) reported median times of 2,146/2,522/2,579 ns
and 2,141/2,566/2,615 ns respectively for the three store sizes. The corresponding p95
times were 2,339/3,624/2,764 ns and 2,282/2,805/2,876 ns. This is a smoke check that
ordinary single-document commit cost does not grow with a full-store copy; use repeated
runs on the target host for any deployment-specific capacity decision.

### Corrected phase-separated Tiny run (PR head `0e63e168`)

On 2026-11-08, the release server built from `0e63e168bc6c2b3560f9447a1c1057c91c5974fb`
was measured in Ubuntu WSL2 on x86-64 with the same 900-byte deterministic payload,
seed `20261108`, 500-operation batches, and a 10-second idle window. Each run used only
its temporary database and the harness's 1.5 GiB address-space, 2 GiB file, 15-minute
CPU, and 128-open-file child limits. The harness was locally instrumented to record
phase-boundary `VmHWM`; its sampler is stopped before the child exits to avoid attributing
a post-exit `/proc` read to the server.

| Documents | Idle RSS | Idle CPU | Idle threads | After load HWM | Checkpoint RSS | Recovery-start RSS | Verify HWM | Whole-process HWM | Whole-run CPU |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 3.83 MiB | 0.1% | 3 | 5.22 MiB | 5.07 MiB | 4.54 MiB | 5.23 MiB | 5.55 MiB | 0.16% |
| 1,000 | 3.86 MiB | 0.1% | 3 | 11.73 MiB | 9.77 MiB | 7.15 MiB | 11.81 MiB | 12.97 MiB | 0.75% |
| 10,000 | 3.83 MiB | 0.1% | 3 | 27.59 MiB | 25.64 MiB | 23.23 MiB | 43.98 MiB | 43.98 MiB | 1.86% |

Two additional one-run threshold probes used the same host, payload, seed, profile,
batch size, and child limits, with a 1-second idle window. The shorter idle window makes
their idle-CPU samples unsuitable for comparison with the table above; the load-phase
memory figures are included only to narrow where the verify peak crosses 40 MiB.

| Documents | Verify RSS after request | Verify HWM | Whole-process HWM | Checkpoint RSS | Idle threads |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 | 25.17 MiB | 25.17 MiB | 26.20 MiB | 16.81 MiB | 3 |
| 7,500 | 34.17 MiB | 35.05 MiB | 35.05 MiB | 21.22 MiB | 3 |

The sampled verify peak remained below 40 MiB at 7,500 documents and exceeded it at
10,000 in the earlier run. This is a workload-specific boundary, not a capacity promise;
document shape, indexes, and database history can change it. Tiny uses one HTTP worker;
the three process threads also include the main server thread and the `ctrl-c` signal
handler thread. The signal thread was confirmed from `/proc/<pid>/task/*/comm` in a
short-lived local Tiny process.

Index build took 15–20 ms. Its direct before/after `VmHWM` did not increase in these
runs. At 10,000 documents, `POST /v1/admin/verify` increased RSS from about 23.2 MiB
after recovery startup to about 43.1 MiB; the phase high-water mark reached 43.98 MiB.
At the time of this `0e63e168` run, `Engine::verify` reconstructed a full replay `Store`
while the live `Store` remained resident. The 40 MiB provisional peak target was therefore
exceeded by the full integrity-verification request in that workload, not by idle, the
measured index build, or checkpoint. Verification's semantic replay was retained; no
reduced-check verification mode was used. The verifier was later changed at `f0eb9c3` to
replay into a bounded field projection that preserves full-file decoding, checksums, and
index semantics. Follow-up measurements and the exact fallback limits are recorded in
[RESOURCE_STORAGE_DESIGN.md](RESOURCE_STORAGE_DESIGN.md). These WSL2 results do not qualify
a Raspberry Pi.

## Gaps worth closing next

1. Run the guarded 100,000-document synthetic benchmark on a dedicated Linux host with
   the documented workload and save its JSON result. No result exists yet, and the
   provisional 768 MiB target is not demonstrated.
2. Run the bounded Pi 4B smoke test on real hardware before claiming Raspberry Pi
   qualification. Current ARM64 CI is QEMU emulation; the current commit artifact is
   temporary and expires 30 days after its workflow run.
3. Complete property-based tests for the JSON parser and schema validator.
4. Run a 24-hour mixed read/write soak with periodic checkpoints while recording RSS and
   WAL segment growth; no such soak is recorded.
5. Test a cross-version upgrade with a real 0.1.0 data directory.
6. Recover or explicitly resolve the unavailable 15th original audit finding and obtain
   an independent final code review before considering PR #1 for merge.

## Raspberry Pi 4 hardware validation

**Real hardware has not yet been tested.** CI run [#136](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42504109330) builds and tests ARM64 under QEMU; that is emulation, not a Raspberry Pi. It also cross-builds the `preppydb-server` and `preppydb` binaries from exact PR head `2544e5e6f86ed0eabdeb79e11ddb6ba3cde637fa`, verifies their AArch64 ELF metadata, and smoke-invokes both under QEMU. The commit-named [ARM64 artifact](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42504109330/artifacts/13626861523) expires 2026-12-08 17:21 UTC. Its manifest identifies the source commit, target, compiler and required glibc baseline (2.35). It is suitable for 64-bit Raspberry Pi OS Bookworm or newer; older glibc systems must not run it. This CI artifact is not a published release or stable download channel.

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

The server and CLI child processes receive per-process limits (`RLIMIT_AS` 512 MiB, CPU 30 seconds, file size 32 MiB, core dumps disabled, 128 open files) and 60-second deadlines by default. The script checks available RAM and free `/var/tmp` space before work and samples RAM, disk, and the active PreppyDB process each second; it terminates the test if a threshold is crossed. `MIN_MEM_AVAILABLE_MIB` cannot be lowered below 768 MiB, and `MIN_DISK_FREE_MIB` cannot be lowered below 512 MiB. The generated database is removed only after a successful run and only from the runner's own uniquely named, marked temporary directory. Failures retain that directory for diagnosis; `KEEP_TEST_DATA=1` retains it after success as well.

**These limits do not provide aggregate memory isolation.** `RLIMIT_AS` applies per process, and host resource sampling is not a reservation. Therefore this runner is deliberately only a tiny smoke test. Large write loops, long soak tests, repeated crash campaigns, and full Cargo builds/tests remain blocked on the Pi until they can be run under reliable host-level resource isolation. The one-process SIGKILL check is not a power-loss test and does not prove sudden-power-loss durability.

Record the CI run URL, artifact commit, Pi model and RAM size, OS/glibc/kernel, available resources before/after, output and resource samples in the hardware test report. Until that run is completed, leave this section explicitly marked untested; passing QEMU and CI jobs do not qualify Raspberry Pi hardware or establish production readiness. Windows directory-flush guarantees, an external pricing-audit anchor, independent review, and other PR hardening gates remain separate.

### Limited scan memory check (candidate based on PR head `547cdab`)

A paired local WSL2 comparison used the same deterministic Tiny-profile workload (10,000 documents, 900-byte payloads, one `group` index, seed `20261108`, 10-second idle window) and requested a page of 100 documents. Three runs per variant were measured. The before build used the pre-scan implementation; the candidate used bounded BTree range traversal and paginated before cloning documents.

| Metric | Before | Candidate | Change |
| --- | ---: | ---: | ---: |
| Scan response time | 32.7 ms | 21.6 ms | 33.7% lower |
| Scan-phase lifetime HWM increase | 12.19 MiB | 0 MiB | no new process high-water mark |
| Whole-process peak HWM | 38.87 MiB | 33.84 MiB | 5.02 MiB lower |

The endpoint returned all 100 requested documents on every run. The HWM value is the process-lifetime peak (`VmHWM`), so a zero increase means the scan did not exceed an earlier peak; it is not an independent allocator-peak measurement. These synthetic WSL2 results are diagnostic, not Raspberry Pi qualification. Unbounded full scans still materialize their requested results, and the underlying Store and indexes remain in memory.

### Verification replay index-memory check (candidate based on `681e73b`)

The verifier's replay state keeps all index definitions but materializes index entries only for unique indexes. Unique-index entries are required to detect invalid replayed writes; non-unique index entries do not affect replay validity. Snapshot decoding, frame checksums, WAL CRC/sequence checks, and semantic replay continue to run. Normal open/recovery and query execution still build and use all indexes.

Paired runs used the same release workload on Ubuntu WSL2 (x86-64), Tiny profile, seed `20261108`, 900-byte payloads, 500-operation batches, one `group` secondary index, and a 10-second idle window. Both binaries were based on commit `681e73b52c5cc3607e5d5d2916a738c4f33aa852`; the candidate differs only by the local verifier optimization. The table compares the verifier phase's process-lifetime `VmHWM`, not an allocation-only peak:

| Documents | Baseline verify HWM | Candidate verify HWM | Change | Idle RSS / threads (baseline → candidate) |
| ---: | ---: | ---: | ---: | --- |
| 10,000 | 34.53 MiB | 27.85 MiB | −6.69 MiB (−19.3%) | 3.85 → 3.92 MiB / 3 → 3 |
| 100,000 | 277.52 MiB | 209.72 MiB | −67.80 MiB (−24.4%) | 3.86 → 3.89 MiB / 3 → 3 |

These are single-run diagnostic measurements on the same WSL host. The candidate still retains a projected document map for replay, so the 100,000-document verifier peak remains about 210 MiB and is not bounded independently of database size. No Raspberry Pi, Windows, or ARM64 measurement is implied. The optimization does not change the disk format or the active/query index representation.
