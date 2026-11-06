# Architecture

PreppyDB is a single-node, embedded-first document database with an HTTP front end. This
document explains how the pieces fit together, what durability and consistency
guarantees actually hold, and which trade-offs were chosen and why.

## Crate map

```
preppydb-json ─┐
preppydb-crypto ─┤
                 ├─> preppydb-storage ─┬─> preppydb-server ─> preppydb-client
preppydb-model ──┘                     ├─> preppydb-pricing
                 ├─> preppydb-cli ──────┘
```

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `preppydb-json` | JSON value type, parser, canonical serialisation, pretty printing | — |
| `preppydb-crypto` | SHA-256, HMAC-SHA-256, PBKDF2, CRC32C, base64, hex, OS RNG | `getrandom` |
| `preppydb-storage` | Catalog, records, WAL, snapshots, manifest, transactions, backup | json, crypto |
| `preppydb-pricing` | Price rules, price changes, offers, purchases, audit log | json, crypto |
| `preppydb-model` | Personality/shop collection definitions (schemas, indexes, limits) | json |
| `preppydb-server` | HTTP/1.1 server, routing, auth, rate limiting, web UI | storage, pricing, model, `ctrlc` |
| `preppydb-client` | Blocking HTTP client for the `/v1` API | json |
| `preppydb-cli` | Administration and diagnostics against a data directory or a server | storage, pricing, model, server, client |

Layering rules that are actually enforced by Cargo: storage knows nothing about HTTP;
the server never links the CLI; the CLI is the only crate that may link both storage and
the HTTP client (it needs both local maintenance commands and `preppydb health`).

## Minimal dependencies

The workspace directly uses `getrandom` for OS-backed entropy, `ctrlc` for portable
shutdown signals, and `mlua` to embed a vendored Lua 5.4 VM for the opt-in server-side
script endpoint. `mlua` and its bundled Lua source are MIT-licensed; the dependency graph
is pinned in `Cargo.lock`. Lua runs without filesystem, networking, package, or debug
libraries and is bounded by memory and instruction budgets. The database's JSON,
cryptographic primitives, HTTP server and storage remain implemented in-tree. The first
build needs these crates cached or reachable from crates.io. See [TESTING.md](TESTING.md),
[API.md](API.md#controlled-lua-mutation-scripts), and [../SECURITY.md](../SECURITY.md)
for scope and verification.

## On-disk layout

```
<data-dir>/
  LOCK                      OS advisory lock (pid/version metadata); one writer per directory
  MANIFEST                  current generation: snapshot ref + last sequence number
  auth.json                 credentials and API tokens (mode 0600)
  pricing-audit.log         append-only pricing audit chain
  wal/
    000000000001.wal        WAL segments, rotated at a size threshold
    000000000002.wal
  snap/
    snap-000000000001.pdb   checksummed snapshot per generation
  recovery-report-<ms>.json written only when recovery had to act
```

### Records

```rust
Record { key, value: Json, version: u64, revision: u64, created_at, updated_at, expires_at }
```

* `version` starts at 1 for every key and increments on each write; it is the unit of
  compare-and-set (`expected_version`, `Some(0)` = "must not exist").
* `revision` is the global sequence number of the write that produced this record. It
  makes "is this document newer than that snapshot?" answerable without clocks.
* `expires_at` (ms since the Unix epoch) implements TTL; expired documents disappear from
  reads immediately and are physically removed by the sweeper or the next compaction.

### WAL

Each mutation, batch, transaction and catalog change is one WAL frame:

```
[len: u32 LE][crc32c: u32 LE][payload: len bytes]
```

The payload is a serialised `Record`/`Op`/catalog entry. Writes are appended and
`fsync`ed according to `SyncMode` (`Always` by default, `Never` for tests). A torn final
frame — a crash in the middle of an append — is detected by the length/CRC check and
truncated during recovery; a torn frame in the middle of the log is treated as
corruption and aborts recovery unless `RepairPolicy::TruncateCorruptTail` is set
explicitly (`preppydb --repair`).

An incomplete final segment header (shorter than the fixed 12-byte header and with no
possible record frame) is also treated as interrupted segment creation: writable open
removes that final segment after validating the preceding WAL history and syncs the WAL
directory. A non-final incomplete header is corruption; read-only open refuses to remove
an incomplete header. A complete but invalid header remains corruption.

Sequence numbers are monotonic and gap-free after the published snapshot. Recovery
rejects gaps, duplicates, and regressions in the replayable suffix. WAL segment names
encode rotation order, not record sequence numbers.

Recovery and verification consume frames incrementally. The CLI summary path keeps only
per-segment counters. The low-level `scan_segment` convenience API retains decoded
records and therefore rejects segment files larger than 128 MiB; callers that need to
inspect larger segments must use `scan_segment_visit` or `summarize_segment`.

### Checkpointing

1. Take the writer lock.
2. Rotate to a new WAL segment.
3. Serialise the in-memory store into a new snapshot generation, framed in blocks with
   CRC32C per block plus a SHA-256 of the whole file.
4. Sync a temporary generation manifest and rename it to a new, unused generation
   filename. The previous published manifest is never replaced; on failure, stop writes
   until restart. `MANIFEST.json` remains readable for databases created by older builds.
5. Delete segments that only contain sequences already in the snapshot.

The snapshot is synced before publication. Replacement and directory durability depend
on the operating system and filesystem. Process-crash and power-loss behavior across
all platforms has not yet been qualified; in particular Windows fault-injection coverage
is still required. The manifest reference is treated as untrusted input: its filename
must exactly match its generation, and snapshot header/length metadata is checked before
the snapshot is applied. The parser validates the footer digest and rejects trailing bytes.
The manifest's snapshot reference must include a well-formed SHA-256 and nonzero byte
length; both are checked against the published snapshot.
The parser also checks the declared collection/document counts, rejects duplicate
collection names and duplicate document keys, and applies the same 64 MiB maximum frame
size as the writer. Snapshot temporary files are created exclusively after removing any
stale temporary directory entry without following it. Snapshot and WAL segment symlinks
are rejected during file discovery/opening. Manifest watermarks are required and must
agree with any snapshot reference; incomplete or inconsistent watermarks are rejected
instead of defaulting to an empty database state.

### Recovery

1. Read `MANIFEST`. An unreadable manifest, a missing manifest with snapshot files or
   retired WAL history, a sequence gap or regression in the replayable suffix, or a
   missing referenced snapshot causes an error; automatic snapshot discovery is not
   implemented.
2. Load the snapshot (verify block CRCs and the file hash).
3. Replay WAL records in sequence order, skipping records already covered by the
   snapshot. Complete bad-CRC frames fail closed by default.
4. A partial final frame is truncated automatically. Explicit writable
   `RepairPolicy::TruncateCorruptTail` truncates at the first corrupt or discontinuous
   frame and removes later WAL segments; this deliberately discards the untrusted suffix.
   Recovery writes `<data-dir>/recovery-report-<ms>.json` and exposes it through
   `GET /v1/admin/recovery` and `preppydb wal`.

## Concurrency model

* **Cross-process:** an OS advisory lock (`File::try_lock` → `flock`/`LockFileEx`) on the
  `LOCK` file, which the kernel releases when the owner dies. A second process (server or
  CLI) fails with `database_locked` (HTTP 503) rather than corrupting data, and a crash
  never leaves a lock that has to be removed by hand. The file also records the owner's
  pid, start time and version for diagnostics.
* **In-process:** one writer mutex; readers take a read lock on the store. Readers never
  block readers, writers never interleave.
* **HTTP:** a bounded worker pool (default 4) with a request queue; each request is
  handled on one thread, so a slow client cannot exhaust the pool indefinitely because
  socket reads/writes are bounded by timeouts and body limits.

### Transactions

```rust
let mut txn = engine.begin()?;
let doc = txn.get("memories", "m1")?;      // read-committed + own writes
txn.put("memories", "m2", value, None)?;
txn.commit()?;                              // validates read versions, one WAL record
```

* Reads inside a transaction see the committed store plus the transaction's own writes.
* At commit the engine re-checks the version of every key the transaction read or wrote;
  any change since the read produces `TransactionConflict` with the offending keys.
* A transaction is represented by one `Op::Batch { label: "transaction" }` WAL record.
  Recovery therefore replays a complete frame as a whole and ignores only an incomplete
  torn tail. If an I/O error occurs after append begins, the caller's outcome is
  indeterminate: a complete frame may be recovered even though `commit` returned an
  error. The engine poisons itself after append errors to prevent further writes until
  reopen; callers must inspect state after recovery instead of assuming the operation
  aborted.
* There is no automatic retry. Callers get `409 conflict`, `retryable: true`, and decide.
* Read sets are bounded by the configured `max_batch_ops` limit (1,000 by default).
  Staged write data is bounded to 32 MiB per transaction before commit; re-writing the
  same key replaces its buffered-byte accounting. Serialized size is counted through a
  bounded formatter that stops at the remaining transaction budget rather than building
  an unbounded temporary JSON string. Commit moves staged JSON values into the WAL
  operation list instead of cloning those values.
* Atomic batches retain at most 32 MiB of serialized prior-document values as rollback
  state. Schema rollback moves removed collections and indexes into an operation journal
  rather than cloning their contents; creating a new index still requires scanning and
  building that index for the collection.

## Indexes

An index maps `field value -> [primary keys]` for one document field (`by_profile`,
`by_item`, `by_starts`, ...). Indexes are maintained inside the same write path as the
document, so they cannot drift within a commit; `verify` additionally rebuilds nothing
but reports index/metadata inconsistencies.

Limits that are documented rather than hidden:

* Only top-level fields are indexed; nested paths (`a.b`) are rejected.
* Index values must be scalars (string, number, boolean); objects and arrays are skipped
  with a validation error at index creation time if declared unique.
* Unique indexes reject the second document with the same value (`409 index_exists` /
  `validation_failed`), but there is no cross-collection uniqueness.
* Range queries walk the sorted in-memory index; they are binary-search based, not
  persisted cursors. A range spanning millions of keys holds the read lock while
  iterating — acceptable for the intended dataset sizes, documented in
  [OPERATIONS.md](OPERATIONS.md#limits).

## Snapshots and backups

`Engine::export_snapshot` and the backup module produce a self-describing archive:

```
PPDBBAK1 | u32 header_len | header JSON | members | PPDBEND1 | sha256(all previous bytes)
```

The header records the creation time, whether a checkpoint was taken, the manifest
generation and per-member SHA-256 hashes. `restore` verifies the archive hash *and* every
member hash before touching the target directory, then moves any existing target to
`<name>.replaced-<ts>` instead of deleting it. A failed restore therefore leaves the old
data intact. Backup output is rejected when its resolved destination is inside the live
database directory, preventing the final archive rename from replacing a manifest,
snapshot, or WAL segment. Parent paths are resolved again before publication, but the
implementation does not claim protection against a hostile concurrent parent-directory
replacement race.

## HTTP layer

* Hand-written HTTP/1.1 with `Connection: close`, bounded request line/header parsing,
  body limits, read/write timeouts.
* Routing is an explicit match on method + path segments; unknown routes answer a
  structured `404 unknown_endpoint` rather than an empty response.
* Authentication runs before routing, so unauthenticated callers cannot probe which
  collections exist.
* Every response carries `X-Request-Id`; errors carry a stable machine-readable `code`.
* The admin UI is a static single page (`web/index.html`, `app.js`, `style.css`) served
  from the binary with a strict CSP and no inline script; it stores the token in
  `sessionStorage` only.

## Deliberate trade-offs

| Decision | Why | Cost |
| --- | --- | --- |
| In-memory working set, WAL on disk | Simple, fast, exact conflict checks | Dataset must fit in RAM |
| One writer, many readers | No lock manager, no deadlocks | Write throughput is serialised |
| Snapshot = whole store | Recovery is trivially correct | Checkpoint cost grows with data |
| JSON values, no binary codec | Inspectable, client-neutral, tests are readable | Larger WAL/snapshots than a columnar format |
| No SQL | Out of scope by requirement | Ad-hoc analytics need exports |
| Two focused runtime dependencies | OS-specific entropy and signal handling | First build needs crates.io or cache |

## Inspirations (no code reused)

* **Redis**: TTL semantics and an admin CLI that prints machine-readable output.
* **WiredTiger / RocksDB**: WAL + checkpoint + manifest generation model, torn-tail
  handling, explicit `verify`.
* **LMDB**: single-writer/multi-reader discipline; copy-on-write snapshots.
* **FoundationDB / CockroachDB**: versioned reads and commit-time conflict validation.
* **CouchDB**: document revisions and CAS semantics.
* **ScyllaDB**: the idea that the data model should be declared once (schema, indexes,
  limits) instead of assembled ad hoc per feature.

No source code from these projects is included; the implementations here are original.
