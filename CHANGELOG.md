# Changelog

All notable changes to PreppyDB are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres to
semantic versioning once it reaches 1.0; during 0.x the API may still change and every
change is listed.

## [0.1.0] — unreleased / first implementation

The first end-to-end implementation. Everything listed here exists in the tree and is
covered by the tests enumerated in [docs/TESTING.md](docs/TESTING.md); the items under
"Not verified" are explicitly not claimed as tested.

### Added

**Storage engine (`preppydb-storage`)**

* JSON-like documents in named collections with CRUD, key-prefix scans and
  compare-and-set versions.
* Atomic batches (one WAL record, all-or-nothing) and read-committed transactions with
  read-your-writes buffering and commit-time version validation.
* Write-ahead log with CRC32C-framed records, configurable `SyncMode`
  (`always`/`interval`/`never`), segment rotation and monotonic sequence numbers.
* Checkpointing: WAL rotation → checksummed snapshot → atomic manifest swap → removal of
  retired segments.
* Crash recovery: replay after the snapshot sequence, torn-tail truncation with a written
  recovery report, hard failure on mid-log corruption unless repair is explicitly enabled.
* Compaction, expired-document sweeps, TTL with per-document and per-collection defaults.
* Secondary indexes (equality, inclusive ranges, optional uniqueness) maintained in the
  write path.
* Consistency checks (`verify`) over manifest, snapshots and every WAL segment.
* Backup archive format (`PPDBBAK1`) with per-member and whole-file SHA-256, verified
  restore that never deletes existing data.
* Single-writer lock per directory, implemented as an OS advisory lock
  (`File::try_lock`) so the kernel releases it when the owner dies — no stale pid files.
* `replace_file` helper: the temp-file-then-rename pattern used by the manifest,
  snapshots, backups and `auth.json` also works on Windows, where a plain rename cannot
  replace an existing file.

**Server (`preppydb-server`)**

* Hand-written HTTP/1.1 server with a bounded worker pool, request ids, body limits,
  per-credential rate limiting and graceful shutdown.
* Versioned `/v1` API: collections, documents, batches, transactions, indexes, stats,
  metrics, admin (checkpoint, compact, verify, backup, recovery, auth), personality and
  pricing namespaces.
* Structured errors (`{"ok":false,"error":{"code","message","retryable"}}`) with stable
  codes and correct HTTP statuses.
* Authentication: API tokens, users with PBKDF2-SHA-256 passwords, in-memory sessions,
  roles `read`/`write`/`admin`, token revocation, login rate limiting.
* Loopback-only binding by default; non-loopback requires an explicit opt-in flag.
* Web admin UI (static, strict CSP): collections, documents, stats, pricing, backups,
  themes.
* Optional personality/shop model with 14 collections, schemas, indexes and limits.
* Dynamic pricing namespace: rules, deterministic ticks, offers, purchases, price history
  and audit verification.

**Clients and tooling**

* `preppydb-client`: blocking Rust client with typed errors and retryability metadata.
* `clients/luvit/preppydb.lua`: dependency-free Lua/Luvit client with its own JSON codec,
  sync and async call styles, and a test suite.
* `preppydb` CLI: credentials, documents, batches, maintenance, backup/restore, pricing
  simulation, audit verification and a `health` probe for supervisors.
* `docker/Dockerfile` (multi-stage, linux/amd64 + linux/arm64), `docker/entrypoint.sh`,
  `docker-compose.yml`, `.env.example`.
* `scripts/docker-persistence-test.sh`: restart and container-recreation persistence test.
* CI workflow for Linux x86-64, Windows x86-64, Linux ARM64 (qemu), Lua client and both
  Docker architectures (build only, never pushed).
* Documentation: architecture, API, operations, data model, pricing, CLI, testing,
  security policy.

### Requirements

* Rust **1.89** or newer (the lock uses the stabilised `File::try_lock`), `cargo test`
  needs only the standard library.

### Not verified in the 0.1.0 environment

* Docker image builds and the compose example (no daemon available).
* Windows build/test/smoke and ARM64 build/qemu tests (no such hosts; CI jobs written).
* The luvit `http` transport (luvit not installed; the suite runs on embedded Lua with a
  substituted transport).
* GitHub Actions itself has not been executed yet.

### Not implemented

* Replication, clustering, sharding, failover.
* TLS in the server, authentication backends other than the built-in store.
* Interactive transactions over HTTP, cursors/streaming responses.
* Query language of any kind (by design — the API is key/document based).
