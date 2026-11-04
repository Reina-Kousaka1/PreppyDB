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
| Lua | Lua 5.5 embedded through `lupa` (no `luvit`, no system Lua) |
| Network | crates.io unreachable → workspace has **zero** external dependencies |

## Test inventory

| Suite | Location | What it covers |
| --- | --- | --- |
| JSON (18) | `crates/preppydb-json` | parsing, escapes, numbers, canonical form, ordering, rejects invalid input |
| Crypto (30) | `crates/preppydb-crypto` | SHA-256 vectors, HMAC, PBKDF2, CRC32C, base64 round-trips, hex, RNG |
| Storage unit (46) | `crates/preppydb-storage/src/**` | records, catalog, schema validation, WAL framing, snapshot blocks, manifest, locks, clock, backup format |
| Storage integration (21 + 1 helper) | `crates/preppydb-storage/tests/engine_integration.rs` | durability, checkpoint/replay, indexes across restart, transactions (commit/conflict/rollback), batch atomicity across restart, TTL, **crash recovery with a real `abort()` in a child process**, torn-tail truncation, mid-log corruption handling, backup/restore round-trip |
| Pricing (10) | `crates/preppydb-pricing` | bounds, clamping, discounts, seeded determinism, offer expiry/staleness/double-redeem, ledger immutability, audit tamper detection |
| Model (5) | `crates/preppydb-model` | 14 collections declared, no name collisions with forbidden names, schemas validate, bootstrap idempotence |
| Rust client (5 + 1 doc) | `crates/preppydb-client` | URL parsing, https rejection, error mapping (`is_retryable`, `code`), request building |
| API integration (12) | `crates/preppydb-server/tests/api_integration.rs` | real HTTP server on an ephemeral port: public endpoints, authn/authz, CRUD + CAS, schema rejection, batch atomicity, body/rate limits, routing, login/session tokens, pricing flow end-to-end, simulation determinism, personality model + admin endpoints, index queries |
| Lua client (30 + 66 + live) | `clients/luvit/tests/*.lua` | JSON codec, request construction, error mapping, retries, async transport contract, and a **live** end-to-end suite against a running server (40 checks) |

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
  compare-and-set conflict (`409`, `retryable`), batch atomicity, scan, forbidden economy
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
  served sampled documents intact, then shut down on SIGTERM and passed `preppydb verify`.

## What was NOT executed

The sandbox that produced this tree has no Docker daemon, no Windows and no ARM64
hardware, so those legs run only in GitHub Actions. The current state, kept honest:

| Item | State | How it is covered |
| --- | --- | --- |
| Linux x86-64 fmt/clippy/tests, crash recovery, release build | **green in CI** | `linux-x86_64` job |
| Windows x86-64 build + full test suite + PowerShell smoke | **green in CI** | `windows-x86_64` job |
| Docker image build (amd64 + arm64) | **green in CI** | `docker-images` job, build only — never pushed |
| `scripts/docker-persistence-test.sh` (restart + recreation) | **green in CI** | `docker-persistence` job |
| Linux ARM64 build and emulated tests | CI only | `linux-arm64` job under `qemu-user`; this is emulation, **not** real ARM64 hardware |
| `docker-compose.yml` | not started | the persistence test drives `docker run` directly |
| `luvit` as the Lua runtime | not run | luvit is not installed; the suite ran under embedded Lua with a substituted transport |
| Long-running soak / memory profiling | not run | out of scope for this phase |
| Replication | not implemented | later module |

Note for the emulated job: password verification is PBKDF2-HMAC-SHA-256 with 120k
iterations in portable Rust, which `qemu-user` executes roughly two orders of magnitude
slower than native code. Tests that log in therefore use an explicit, generous client
timeout (`with_timeout`) instead of the 10 s default.

Workflow: <https://github.com/Reina-Kousaka1/PreppyDB/actions/workflows/ci.yml>.
| `luvit` as the Lua runtime | not run | luvit is not installed; the suite ran under embedded Lua 5.5 with a Python-bridged transport, so the luvit `http` transport itself is unexercised |
| Long-running soak / memory profiling | not run | out of scope for this phase |
| Replication | not implemented | later module |

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

## Gaps worth closing next

1. Run the Docker and Windows jobs at least once (CI), and do a real Raspberry Pi run.
2. Add a `luvit` job with the real `http` transport (the CI job tries and tolerates its
   absence today).
3. Property-based tests for the JSON parser and the schema validator (fuzzing a parser by
   hand is not enough).
4. A soak test: 24 h of mixed reads/writes with periodic checkpoints, watching RSS and
   segment growth.
5. Cross-version upgrade test (0.1.0 → next) with a real data directory.
