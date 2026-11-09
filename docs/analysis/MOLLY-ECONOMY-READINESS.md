# PreppyDB ↔ Molly economy: independent readiness review

**Follow-up (2026-11-04):** two findings here have since been addressed in code — H5 (no
`EINTR` handling on the WAL `fsync`) and H6 (durability not visible on `GET /v1/version`).
See `CHANGELOG.md` and [`docs/MOLLY-STORAGE-CONTRACT.md`](../MOLLY-STORAGE-CONTRACT.md).
The rest of the document is unchanged and still describes `932f4c9`.

**Status of this document: analysis only.** No production data was touched, no repository
other than this one was modified, and **no test in this document was executed**. Every
code claim below is a *reading* of the named file and line range, not a measurement. Where
I extrapolate, the provenance of the numbers is named.

## Provenance and method

| Input | Revision | How it was obtained |
| --- | --- | --- |
| PreppyDB | `932f4c9` (`main`, the merge of PR #1) | local checkout in this workspace |
| Molly | `5820a645c89040c009302bde6307a35f9abdf486` (2026-11-06) | shallow `git clone` from `github.com/Reina-Kousaka1/Molly` |
| SeaWorld DB | — | **not available**; everything about it below is either Molly's usage of it or an explicit assumption |
| Capacity numbers | CI runs #148/#154/#159/#160 quoted in `docs/RESOURCE_STORAGE_DESIGN.md` | read from this repository |

No Rust toolchain and no Luvit runtime were available while writing this, so the Rust and
Lua fragments in section D are **uncompiled sketches**. They are written to match the
surrounding code closely enough to review, nothing more.

---

## A. Executive verdict

**PARTIALLY READY.**

The engine is not the problem. PreppyDB already provides, verified in code:

* single-WAL-record atomic batches with per-operation compare-and-set (`batch_checked`,
  `engine.rs:1442`), all-or-nothing, one `fsync` when `SyncMode::Always`;
* an optimistic multi-document transaction with a re-checked read set and read-your-writes
  (`txn.rs`, one WAL record at commit);
* a conditional single-document `PUT` with `expected_version`, returning the new version,
  and `409 conflict` on mismatch (`api.rs:1456‑1545`);
* fail-closed behaviour after an ambiguous commit: `commit_outcome_unknown` → HTTP 503 with
  `retryable:false`, and **all** reads and writes rejected until reopen (`engine.rs:1047`,
  `get_opt` at `engine.rs:1599`).

What is *not* ready is the Molly side plus three small server items:

1. **Molly's storage model exceeds a hard limit.** `GuildStore` creates one collection per
   guild (`molly_g_<guild_id>`, `src/persistence/guild_store.lua:34`) and `GlobalStore`
   one per name (`molly_global_<name>`). PreppyDB allows `max_collections = 256` per
   database (`store.rs:289`) and that value is **not configurable** — `EngineConfig::new`
   always uses `Limits::default()` (`engine.rs:229‑239`) and no server setting overrides it.
   A bot in more than a couple of hundred guilds cannot start its economy at all.
2. **There is no adapter** from Molly's connection-scoped store API (`session` →
   `begin`/`query`/`get`/`update`/`commit`/`rollback`) to an HTTP CAS write. The
   `src/preppydb/connector.lua` that exists is a transport wrapper, not a store backend:
   nothing in Molly's feature code uses it (`docs/preppydb-connector.md` says so explicitly,
   and the connector exposes `get`/`put(expected_version)`/`batch`, which is exactly the
   right primitive but is not wired to `Store:run`).
3. **Durability mode is invisible to clients.** The entire money-safety argument depends on
   `sync_mode = always`, yet neither `/v1/version` (`api.rs:430`) nor `/v1/stats` exposes it,
   and `Engine` has no public accessor. Molly therefore cannot refuse to run the economy
   against a non-durable server, and the `tiny` profile silently lowers the request body
   limit to 512 KiB (`config.rs:113`) — smaller than a busy guild document may become.

Two claims in the problem statement do **not** survive contact with the code:

* *"PreppyDB currently prohibits wallet, balance, credit-ledger and debit-ledger
  collections"* — true but irrelevant, and weaker than described: the ban is an **exact
  name match on seven names, enforced only at collection-creation time**
  (`preppydb-model/src/lib.rs:29‑37`, `api.rs:113‑123`). `molly_guilds`,
  `molly_g_<id>` and fields called `wallet`/`balance` *inside* documents are unaffected. It
  is a naming guardrail, not a write-time restriction, and it is trivially bypassed by
  renaming — so it is not a security boundary either. It blocks nothing that Molly does.
* *"PreppyDB does not provide the full transactional contract required by Molly"* — Molly's
  verified requirement is **a single-document read-modify-write with lost-update
  protection inside one atomic commit**. That is precisely a conditional `PUT`
  (`expected_version`). Multi-document atomicity is not required by any current Molly code
  path: `guild_store.lua` performs exactly one `update` per transaction, and a grep of
  `src/` finds no `:begin(`/`:commit(` outside `src/persistence/` and the SeaWorld driver.

So: blocked today, but blocked by a *storage-shape decision and a missing adapter*, not by
missing database capability, and not by the economy-collection restriction.

---

## B. Root-cause analysis

### B.1 Confirmed defects (code-level, no test executed)

| # | Defect | Evidence | Impact |
| --- | --- | --- | --- |
| D1 | `WalWriter::sync` does not retry `ErrorKind::Interrupted`; `std`'s `File::sync_data` does not loop on `EINTR` either | `wal.rs:480‑498` | A signal delivered during `fsync` (SIGTERM via `ctrlc`, SIGCHLD, debugger) turns one commit into `commit_outcome_unknown` **and poisons the whole engine** for a transient, harmless condition |
| D2 | No "provably not committed" outcome. Any `append` failure poisons the engine and returns `CommitOutcomeUnknown`, even when the frame was never written or when the failed tail could be truncated and re-synced | `engine.rs:1094‑1130`, `1160‑1180`, `1404‑1432`; `wal.rs:385‑478` | Availability loss where a clean, *safe* retry would be justified: an `ENOSPC` on the first byte currently requires a full process restart; the same class of failure on a `read-only` filesystem would too |
| D3 | `docs/API.md` examples write to `collection = "wallets"` | `docs/API.md:76‑80` | The documented Lua example cannot run: creating that collection returns `403 forbidden_collection`, and writing into a non-existent collection fails. Documentation defect, no data impact |
| D4 | `max_collections = 256`, not configurable | `store.rs:289`, `engine.rs:229‑239`; no `Limits {` construction outside storage defaults | Hard ceiling on Molly's current per-guild-collection model. Not a storage bug — a mismatch between a fixed limit and a consumer's data model |
| D5 | `sync_mode` not observable through the API; engine has no accessor | `api.rs:430‑437`, `api.rs:~1255` (stats), no `Engine::sync_mode` | A client cannot verify the durability it depends on; the `tiny` profile's 512 KiB body limit is likewise invisible (`config.rs:107‑113`) |

### B.2 Missing capabilities (by design or omission)

| # | Gap | Consequence for Molly | Do we need it? |
| --- | --- | --- | --- |
| M1 | No per-tenant/collection authorisation: `Principal` carries only global `roles` (`auth.rs:48‑53`) | Multi-guild isolation cannot be enforced by the database; it is key discipline inside one process + one token | **No.** Molly already isolates by guild id and runs one process. Document the residual risk; do not build tenancy into the DB |
| M2 | No server-side idempotency keys ("no durable retry/idempotency mechanism", `docs/API.md:68`) | A retry after a lost response could re-apply | **No.** Molly's receipts live in the *same document* as the balances (`ledger.lua:_run_with_receipts` and `check_request`), so idempotency is atomic with the money movement. Moving receipts into a server-side key table would make idempotency *less* reliable, not more |
| M3 | No permanent audit log: journal keeps 1000 rows per guild (`ledger.lua:6`, `docs/v2-economy-modes.md`) | Old money ids stop resolving (`replay_expired` / `transaction_not_found`) | **Deferred.** If permanent auditability is a requirement, add an append-only `molly_history` collection written in the *same* commit later; do not block the migration on it |

### B.3 Deliberate safety restrictions — keep them

* `commit_outcome_unknown` is `retryable:false` (`error.rs:55‑60`) and returns 503 while the
  engine is poisoned, for reads too. This is correct; **do not weaken it to make retries
  convenient.** Clients must reconcile, not repeat.
* `RepairPolicy::Strict` refuses to open on corruption (`engine.rs:190‑196`), torn tails are
  only tolerated at the end of the **last** segment (`engine.rs:821`ff), and a complete
  frame with a bad CRC is *always* corruption, never a torn tail (`scan_segment_visit_with_offset`, `wal.rs:688`ff). Any
  "just skip the bad record" request is a data-integrity downgrade.
* `IMMUTABLE_COLLECTIONS` and pricing-managed collections stay off-limits to Molly writes
  (`api.rs:1435‑1454`).
* No automatic retries in the Lua client for write transport errors
  (`clients/luvit/preppydb.lua:715`, `retryable = false`), default `retries = 0`
  (`:567`). Correct for money.

### B.4 Assumptions that still need verification

| # | Assumption | How to check | If false |
| --- | --- | --- | --- |
| A1 | Molly uses only single-document transactions (so one CAS `PUT` is equivalent) | grep of `src/` for `:begin(`/`:session(` — done here, shows only the two store files and the SeaWorld driver; re-run after any Molly branch merge | Multi-document atomicity would be needed → use `POST /v1/transactions` with per-op `expected_version` |
| A2 | The largest guild aggregate stays below `max_doc_bytes` (4 MiB default) and below `max_body_bytes` (4 MiB default / 512 KiB tiny) | measure the export: max serialized doc size, max accounts per guild | Raise `max_doc_bytes` per collection at creation (supported, `store.rs:554`) and `max_body_bytes` in the server config; re-derive write amplification |
| A3 | SeaWorld can be exported read-only and consistently | SeaWorld source/documentation not available here | Freeze Molly for the whole export instead of a snapshot |
| A4 | SeaWorld actually prevents lost updates between two connections (Molly relies on the connection transaction for that) | SeaWorld capability contract; `src/persistence/seaworld_connector.lua:90` only checks `capabilities.transactions == true` | No change for Molly — PreppyDB's CAS is *stronger* than an unverified guarantee, but the migration must not assume SeaWorld's rollback is safe |
| A5 | The Luvit transport in `src/preppydb/connector.lua` can serve `Store:run` synchronously | read `connector.lua:1‑83`; its own doc says a synchronous transport or `options.wait` pump is required | `Store:run` must become coroutine-based; the current `Ledger` call sites assume a synchronous return |
| A6 | Molly's supervisor restarts the server after `commit_outcome_unknown` | deployment config (systemd/compose) | Molly must exit non-zero instead of serving 503 forever |

---

## C. Minimal safe architecture

### C.1 The shape of the change

```
Molly (unchanged)                    Molly (new)                     PreppyDB (small changes)
┌───────────────────────┐   ┌──────────────────────────────┐   ┌────────────────────────────────┐
│ ledger / cases / shop │──▶│ GuildStore:run(guild, fn)     │──▶│ PUT /v1/collections/          │
│ (unchanged Lua)       │   │  backend = preppydb_store.lua │   │     molly_guilds/documents/<g> │
└───────────────────────┘   │  GET (version) → fn() → PUT   │   │ { value, expected_version }    │
                            │  guarded by per-guild mutex   │   │  → 1 WAL record, fsync         │
                            └──────────────────────────────┘   └────────────────────────────────┘
```

Three decisions do all the work:

1. **One collection, guild id as the key.** `molly_guilds`, document key = the guild id
   string (Molly already validates `^%d+$`, `guild_store.lua:18`). This removes the
   256-collection ceiling without changing a single line of economy code. `molly_global_*`
   collapses the same way into `molly_globals`.
2. **The commit is one conditional `PUT`.** Read the envelope (it carries `version`),
   run the existing `fn(data)`, then write the whole document with `expected_version`.
   A concurrent writer makes the `PUT` fail with `409 conflict` →
   Molly's existing `serialization_failed`. Nothing is staged, so there is nothing to roll
   back and no read-set to validate: the version check *is* the conflict detection.
3. **Money stays in Molly.** Receipts, journal, account tables and the integer invariants
   (`MAX = 1e12`, `amount()` validation, `account()` validation) are unchanged and stay in
   the same document as the balances, so idempotency and money move in one atomic commit.
   PreppyDB is an atomic, durable, compare-and-set document store — not a ledger.

### C.2 What PreppyDB must guarantee (and mostly already does)

| Guarantee Molly needs | Where it comes from | Status |
| --- | --- | --- |
| A commit is durable before the HTTP 200 | `SyncMode::Always` → `sync_data` inside `WalWriter::append` (`wal.rs:458‑460`) | exists; must be *enforced*, see defect D5 / fix D.3 |
| No reader sees a half-applied batch | apply + append happen under the engine write lock (`engine.rs:1329‑1331`) | exists |
| Lost updates impossible | `expected_version` check inside the same write lock (`engine.rs:1374` and `engine.rs:1382`) | exists |
| Ambiguity is loud, never silent | poison + 503 + `retryable:false` (`engine.rs:1047`, `api.rs:2419`) | exists |
| Transient failures do not need a restart | *missing* | **D1/D2 fix** |
| Client can verify durability mode | *missing* | **D5 fix** |
| No silent truncation of a too-large document | `limit_exceeded` (422) before apply (`store.rs:554`) | exists |

### C.3 What we deliberately do **not** build

* No economy/ledger engine inside PreppyDB (no balances, no transfer primitive, no money
  arithmetic). A database that owns money must also own Molly's rounding, cooldowns,
  receipts and Discord replay window; that is a rewrite, not an integration.
* No general SQL-style transaction manager with `BEGIN … COMMIT` over HTTP. `txn.rs` +
  `batch_checked` already cover the multi-document case, and Molly does not need it today.
* No dual-write. Two independent stores cannot be committed atomically; PreppyDB's own
  honest `commit_outcome_unknown` would become an *unresolvable* cross-system ambiguity.
* No rename of a forbidden collection to smuggle a balance table in. That would defeat the
  guardrail and gain nothing, because Molly's money already lives in a document.

### C.4 Migration architecture (Challenge 4)

Constraint that shapes everything: Molly's entire guild state — balances, journal,
receipts, cases, staff, inventory, settings — is **one JSON document per guild**
(`guild_store.lua:34`, `50‑70`). A migration is therefore a *document copy*, not a
normalised relational import, and it is reversible in the same way.

**(1) Read-only source export.** A new Molly-side tool (`tools/migrate/export.lua`) reads
SeaWorld through the existing read-only path (or an offline copy of the SeaWorld data
directory) and writes a JSONL file per run:

```
{"guild_id":"123","source_id":"…","source_version":N,"data":{...},"sha256":"…"}
```

It opens SeaWorld read-only, performs no write of any kind, and refuses to run if
`inspect` shows it is not read-only. The manifest records: SeaWorld version/capabilities,
run timestamp, guild count, per-guild sha256, total sha256, and the sha256 of the sorted
list of `guild_id`s.

**(2) Versioned target schema.** Three collections, created exactly once, before any import:

| Collection | Key | Value | Created with |
| --- | --- | --- | --- |
| `molly_guilds` | guild id | `{schema_version:1, guild:<id>, data:{…}}` | `max_doc_bytes` = measured max × 1.5, capped below `max_body_bytes` |
| `molly_globals` | global store name | `{schema_version:1, name:…, data:{…}}` | default |
| `molly_meta` | `"migration"`, `"import"` | phase, counters, checksums, timestamps | default |

`schema_version` is additive: a reader that does not know it treats the document as v1, so
a rollback to SeaWorld never has to understand it.

**(3) Dry-run.** The same importer runs against a *second* PreppyDB directory
(`PREPPYDB_DATA_DIR` pointing at `/var/lib/preppydb-migration`) with the production server
untouched. Acceptance: import completes, `POST /v1/admin/verify` is clean, and the
reconciliation report of (6) is empty. Only then is the production path considered.

**(4) Import checkpoints.** Progress is written **in the same atomic batch as the documents
it covers**, so a crash can never leave "documents imported but progress not recorded" or
the reverse:

```json
POST /v1/transactions
{"operations":[
  {"op":"put","collection":"molly_guilds","key":"123","value":{...},"expected_version":0},
  {"op":"put","collection":"molly_meta","key":"import","value":{"phase":"importing","last_guild":"123","imported":1},"expected_version":<current>}
]}
```

Resume = read `molly_meta/import`, skip every guild whose stored sha256 already equals the
manifest's, continue from `last_guild`.

**(5) Idempotent import.** Insert-only (`expected_version = 0`) for new documents. An
existing document is left alone when its canonical sha256 matches, and only overwritten
(`expected_version = current`, explicit `--repair`) when it does not. Running the importer
twice must produce byte-identical canonical documents and an empty diff.

**(6) Balance and transaction reconciliation.** Per guild and in total, compare source and
target: number of accounts, `Σ wallet`, `Σ bank`, journal row count, max journal id,
receipt count, receipt watermarks, `next_case`, inventory counts, and the canonical sha256
of the whole document. Invariant: **every figure equals the source's**, not merely
"conserved" — Molly's ledger has no mint/burn ledger to compare against, so equality with
the frozen source is the only meaningful statement.

**(7) Checksums and record counts.** One canonicaliser is used by exporter, importer and
verifier: keys sorted, integers rendered as digits (Molly's own encoder is *not*
canonical — it does not sort keys — so it must not be reused for checksums), no floats in
money fields. The manifest's total sha256 must equal the target's recomputed total.

**(8) Controlled write freeze at cutover.** Molly enters maintenance mode (commands answer
"maintenance"), and the tool waits until both `busy` tables are empty, i.e. no `Store:run`
is in flight. Then: final export → import delta → reconcile → switch the store backend in
config → start Molly. Any *post-freeze* change of source data is impossible because Molly
is the only writer and it is stopped. Target: freeze under five minutes; the number is
measured during rehearsal, not assumed.

**(9) Rollback that accounts for post-cutover writes.** Rollback is **not** "point Molly at
SeaWorld again": SeaWorld would be missing everything written after the cutover. The
honest path is a tested *reverse* import:

1. Export `molly_guilds` from PreppyDB (same canonical form).
2. Import into SeaWorld, overwriting the stale documents by their original ids.
3. Verify the same reconciliation figures against the PreppyDB side.
4. Start Molly on SeaWorld and keep PreppyDB untouched for the retention period.

Because the whole guild document round-trips unchanged, this is mechanical — but it must be
*build and rehearsed before cutover*, and it has a hard limit: the journal and receipts are
capped (1000 each), so a rollback after those caps have rotated loses history that only
exists in PreppyDB. Decide and document that window (or keep the append-only history
archive) before cutover rather than discovering it during an incident.

**No dual-writing.** Two independent databases cannot be committed atomically; Molly would
gain a third outcome ("written to one, unknown in the other") on top of the one PreppyDB
already reports honestly. The freeze plus a tested reverse import is strictly safer.

**(10) Production readiness criteria** — all must be evidenced, not asserted:

* [ ] `commit_outcome_unknown` path exercised end to end, including a supervised restart.
* [ ] `sync_mode = always` asserted by the client preflight (D.5) and visible in `/v1/version`.
* [ ] `max_doc_bytes`/`max_body_bytes` sized from the *measured* largest guild document, with
      headroom; the tiny profile explicitly rejected if the measurement says so.
* [ ] Reconciliation report empty on a full dry-run, twice in a row (idempotence).
* [ ] Import interrupted at least three times at different phases, resumed, reconciled.
* [ ] Reverse import (rollback) rehearsed against the same data set.
* [ ] Crash drills from E.4 executed on the target hardware, results recorded.
* [ ] Peak RSS and p95 write latency on the target hardware for a realistic guild count.
* [ ] Backup taken immediately before cutover, restore verified into a scratch directory.
* [ ] Runbook with rollback trigger conditions (e.g. any `commit_outcome_unknown` in the
      first hour → freeze and roll back).

---

## D. Concrete Rust and Lua changes

> **Everyone of these fragments is uncompiled and untested.** They are written to the shape
> of the existing code so they can be reviewed; treat them as pseudocode with types.

### D.1 PreppyDB: make `fsync` interrupt-safe (defect D1)

`crates/preppydb-storage/src/wal.rs`, `WalWriter::sync` (currently `wal.rs:480‑498`):

```rust
pub fn sync(&mut self) -> Result<()> {
    #[cfg(test)]
    if std::mem::take(&mut self.fail_next_sync) {
        return Err(DbError::io("injected WAL sync failure", std::io::Error::other("injected")));
    }
    // `fsync` may be interrupted by a signal that is delivered while it is running;
    // retry instead of turning an ordinary SIGCHLD/SIGTERM into a poisoned engine.
    let mut interrupted = 0u32;
    loop {
        match self.file.sync_data() {
            Ok(()) => break,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted && interrupted < 8 => {
                interrupted += 1;
            }
            Err(err) => {
                return Err(DbError::io(format!("syncing {}", self.current.display()), err))
            }
        }
    }
    self.unsynced_frames = 0;
    #[cfg(test)]
    { self.successful_syncs = self.successful_syncs.saturating_add(1); }
    Ok(())
}
```

Cost: ~10 lines. Risk: very low (a bounded retry of an interrupted syscall is the standard
fix; it does not weaken durability because the only outcome that now becomes "success" is
one where `fsync` actually returned success).

### D.2 PreppyDB: distinguish "not committed" from "outcome unknown" (defect D2)

This is the change that matters for Molly's availability. Two parts.

**(a)** Give `WalWriter` an explicit tail-discard that is durable before it reports success:

```rust
// wal.rs
impl WalWriter {
    /// Remove a tail that this writer may have partially created and make the removal
    /// durable. On `Ok(())` the bytes of the failed append are provably not in the log.
    pub fn discard_tail(&mut self, len: u64) -> Result<()> {
        self.file
            .set_len(len)
            .map_err(|err| DbError::io("truncating WAL tail", err))?;
        self.file
            .seek(SeekFrom::Start(len))
            .map_err(|err| DbError::io("seeking WAL tail", err))?;
        self.current_len = len;
        // Mandatory: a truncation that is not durable proves nothing.
        self.sync()
    }
}
```

**(b)** Use it on the append error path, in every commit function
(`commit_document` `engine.rs:1158`, `commit_document_batch` `engine.rs:1329`,
`commit_schema_batch`, `commit_schema`):

```rust
let len_before = writer.current_len();
if let Err(err) = writer.append(&record) {
    // The append failed. Either the frame never made it (or can be removed), or the
    // tail is now uncertain. Only the second case may poison the engine.
    match writer.discard_tail(len_before) {
        Ok(()) => {
            roll_back_in_memory(...)?;              // same undo path as today
            return Err(DbError::Io {                 // definitively not committed
                context: "WAL append".into(),
                detail: format!("{err}; the tail was discarded and re-synced"),
            });
        }
        Err(discard_err) => {
            self.poisoned.store(true, Ordering::SeqCst);
            roll_back_in_memory(...)?;
            return Err(DbError::CommitOutcomeUnknown(format!(
                "{err}; discarding the tail failed ({discard_err})"
            )));
        }
    }
}
```

Why this is *not* "suppressing `commit_outcome_unknown`": the error is only downgraded when
the failed frame has been removed from the log **and that removal has been fsynced**. After
that, the record cannot be replayed by any later recovery, so reporting "not committed" is
a statement of fact, and a client retry is safe (the operation never happened). When the
discard itself cannot be made durable — the dying-disk case — the current, correct
`commit_outcome_unknown` behaviour stays exactly as it is.

Caveat worth stating in the code comment: on Linux a `fsync` that already failed may leave
the *page cache* in a state that a later `fsync` does not repair (the "fsyncgate" lesson).
That is why the discard's own `sync()` is the deciding step, and why a failure there must
poison rather than guess.

**(c)** Rotation: `append` may rotate first (`wal.rs:402‑412`). `len_before` must be captured
*after* the rotation decision, i.e. inside `append`, and returned to the caller — the sketch
above assumes `writer.current_len()` is read after any rotation. Simplest correct variant:
have `append` return `AppendFailure { written_before: u64, kind }` instead of a bare error.

### D.3 PreppyDB: expose the durability mode (defect D5)

```rust
// engine.rs
impl Engine {
    /// The durability policy the WAL was opened with.
    pub fn sync_mode(&self) -> SyncMode { self.lock_mutex(&self.writer).sync_mode() }
}

// api.rs, GET /v1/version (public, no token required)
.with("sync_mode", Json::str(self.engine.sync_mode().as_str()))
.with("durable_writes", Json::Bool(self.engine.sync_mode().is_always()))
.with("max_body_bytes", Json::uint(self.config.max_body_bytes as u64))
.with("max_doc_bytes", Json::uint(self.engine.config().limits.max_doc_bytes))
```

No secrets are exposed; the values are already printed at startup (`main.rs:247‑256`). This
lets Molly *fail closed* instead of running an economy on an `interval`/`never` server.

Optional, not required for the migration: a `limits` section in `ServerConfig` so
`max_collections`/`max_doc_bytes` can be tuned per deployment. Defer — the new data model
removes the need.

### D.4 Molly: a PreppyDB store backend (the actual unblocker)

New file `src/persistence/preppydb_store.lua`, same public contract as `guild_store.lua`
(`run(guild_id, fn)` returning `true, result` or `false, reason`), so **no command, ledger,
shop, moderation or staff code changes**:

```lua
local Store = {}
Store.__index = Store

local COLLECTION = "molly_guilds"      -- one collection, guild id is the key
local SCHEMA_VERSION = 1
local EMPTY = { accounts = {}, cases = {}, staff = {}, journal = {}, receipts = {}, next_case = 1 }

local function valid_id(value)                       -- identical to guild_store.lua:5
  return type(value) == "string" and #value >= 1 and #value <= 20 and value:match("^%d+$") ~= nil
end

-- Map transport/storage errors onto the reason codes Molly already understands
-- (see guild_store.lua:resolve_* / src/commands/economy.lua:11).
local function reason(err)
  if type(err) ~= "table" then return "unavailable" end
  if err.status == nil then return "outcome_unknown" end          -- response lost: unknown
  if err.code == "commit_outcome_unknown" then return "outcome_unknown" end
  if err.code == "conflict" then return "serialization_failed" end
  if err.code == "limit_exceeded" then return "resource_limit" end
  return "unavailable"
end

function Store.new(client) return setmetatable({ client = client, busy = {} }, Store) end

function Store:run(guild_id, fn)
  if not valid_id(guild_id) then return false, "invalid_guild" end
  if self.busy[guild_id] then return false, "busy" end
  self.busy[guild_id] = true
  local called, ok, result = pcall(function()
    -- 1. read the whole guild document, with its version
    local version, data = 0, nil
    local envelope, err = self.client:get_envelope(COLLECTION, guild_id)
    if envelope then
      local value = envelope.value
      if type(value) ~= "table" or type(value.data) ~= "table" then return false, "invalid_store" end
      if value.schema_version ~= nil and value.schema_version > SCHEMA_VERSION then
        return false, "unsupported_schema"
      end
      version, data = envelope.version, value.data
    elseif err and err.code ~= "not_found" then
      return false, reason(err)
    else
      data = {}
      for key, value in pairs(EMPTY) do data[key] = {} end   -- deep copy of the template
      data.next_case = 1
      data.guild = guild_id
    end
    -- 2. structural validation, unchanged from guild_store.lua:66-70
    if data.guild ~= guild_id or type(data.accounts) ~= "table" or type(data.cases) ~= "table"
      or type(data.staff) ~= "table" or type(data.journal) ~= "table"
      or type(data.receipts) ~= "table" then
      return false, "invalid_store"
    end
    -- 3. the existing domain function runs exactly as it does on SeaWorld
    local applied, value, changed = fn(data)
    if not applied then return false, value end
    -- 4. commit: one conditional PUT, no staging, nothing to roll back
    if changed then
      local written, put_err = self.client:put_checked(COLLECTION, guild_id,
        { schema_version = SCHEMA_VERSION, guild = guild_id, data = data }, version)
      if not written then return false, reason(put_err) end
    end
    return true, value
  end)
  self.busy[guild_id] = nil
  if not called then return false, "unavailable" end
  return ok, result
end

Store.valid_id = valid_id
return Store
```

Notes that matter:

* `get_envelope` (not `get`) is required — it returns `version`, which `Client:get` strips
  (`clients/luvit/preppydb.lua:795‑810`).
* `put_checked(..., version)` with `version = 0` is an insert-only write; a second writer
  gets `409 conflict`, which Molly already surfaces as `serialization_failed`.
* `busy[guild_id]` keeps today's in-process serialisation (`guild_store.lua:30`). It is an
  optimisation and a UX nicety, **not** the safety mechanism — the CAS is.
* No `begin`/`commit`/`rollback`: with a single atomic write there is no window in which a
  staged update could be observable, so `rollback` becomes a no-op and Molly's
  `commit_attempted` bookkeeping is no longer needed on this backend.
* `src/persistence/global_store.lua` gets the same treatment with `molly_globals` and the
  name as key.
* The SeaWorld backend stays in the tree, selectable by config, until the rollback window
  closes.

### D.5 Molly: refuse to run the economy on a non-durable server

At startup, before Discord connects:

```lua
local version, err = client:version()
if not version then return false, "storage_unreachable" end
if version.sync_mode ~= "always" or version.durable_writes ~= true then
  return false, "storage_not_durable"          -- do not start the economy
end
if type(version.max_body_bytes) == "number" and version.max_body_bytes < GUILD_DOC_LIMIT then
  return false, "storage_document_limit_too_small"
end
```

This is the only place where the DB's *configuration* is asserted, and it makes the
"we run with `sync_mode=always`" claim checkable instead of a README sentence.

### D.6 Not proposed: an economy API in PreppyDB

If, after the migration, money ever needs to move across documents (for example per-user
account documents instead of one aggregate), the existing `POST /v1/transactions` batch with
per-operation `expected_version` is sufficient: balances, the receipt and the journal entry
commit as one WAL record, and a duplicate request fails the receipt's `expected_version = 0`
insert. That is a *data model* change inside Molly, not a new database feature. Any proposal
that adds `transfer`, `balance` or `credit` primitives to PreppyDB should be rejected: it
moves Molly's economy rules (MAX, cooldowns, replay windows, Discord ids) into a database
that cannot see them, and it duplicates the guardrail the repository deliberately installed.

---

## E. Test and recovery strategy

### E.1 Invariants (the only things worth asserting)

| # | Invariant | Why it can break |
| --- | --- | --- |
| I1 | A document version that the client wrote is never observed with different content | lost update behind a stale `expected_version` |
| I2 | `sum(wallet + bank)` per guild is unchanged by any operation that is not a balance-preserving transfer or an explicit mint/burn | double apply, partial apply |
| I3 | Any acknowledged transaction id resolves to a journal entry with the same amount and counterparty | acknowledgement before durability |
| I4 | A repeated request id (interaction/message) never changes balances twice | idempotency window |
| I5 | After a crash, every acknowledged write is present and no partially applied batch is | WAL ordering, torn tail |
| I6 | An ambiguous outcome is never reported as success, and never silently retried | poison / client retry policy |
| I7 | Guild A's document is never read or written while serving guild B | key discipline |

### E.2 Mandatory scenarios → where they are tested

`P` = PreppyDB (Rust, `cargo test`), `M` = Molly (Lua, offline harness), `X` = end-to-end
(real server). "Exists" means I found a test with this coverage while reading the tree; it
does not mean I ran it.

| Scenario | Layer | Injection | Invariant | Status |
| --- | --- | --- | --- | --- |
| WAL append failure (before any byte) | P | `WalWriter::inject_append_failure` (`wal.rs:~470`) | I5, I6: no visible mutation, error is definite, engine stays usable **after D2** | exists for the first two; the "stays usable" half is new |
| WAL sync failure | P | `inject_sync_failure`, `AppendFault::BeforeSync` (`wal.rs:158`) | I5, I6: either success only after `fsync`, or poison | exists (`engine.rs:~2773` `interval_sync_failure_poisons_engine_and_is_reported`) |
| Partial frame write | P | `AppendFault::PartialWrite` | I5: reopen truncates the tail, the record is absent, no half-document | exists |
| Crash before durable commit | P/X | kill the child after `write()` but before `sync` (needs a test-only barrier hook) | I5: document absent after reopen | **new** (needs a barrier) |
| Crash after durable commit, before HTTP response | X | kill the server right after `append` returns Ok | I3, I6: document present; the client sees a transport error, not success | **new** |
| Ambiguous commit result | X | `SIGKILL` between commit and response; then reopen | I6: after reopen the document is either fully there or not at all; `commit_outcome_unknown` was never a success | **new** (end-to-end) |
| Duplicate transfer request | M/X | send the same interaction id twice | I4: `wallet` unchanged on the second call, same journal entry returned | exists (`tests/economy_integration.lua`, "repeated interaction must not duplicate reward") |
| Concurrent transfers, same wallet | M/X | two coroutines in one guild; two *processes* via two clients | I1, I2: exactly one commits, the other gets `serialization_failed`/`conflict` | exists in-process (`economy_integration.lua` "concurrent payments must not both commit"); **cross-process is new** |
| Insufficient funds | M | `pay` more than the balance | I2: no negative balance, no partial transfer | exists |
| Overflow / underflow | M | `"-1"`, `"0"`, `"99999999999999999"`, `"nan"`, `MAX+1`, and 2 × `MAX` | I2: rejected, never stored, never saturated | exists for the first four; `MAX+1`/sum overflow is **new** |
| Restart during transaction | X | kill while the store is mid-`run` (after GET, before PUT) | I5: nothing applied; the next attempt succeeds | **new** |
| Lost HTTP response after a successful commit | M/X | transport drops after the server logged the write | I3, I6: Molly returns `outcome_unknown`; a *reconciliation read* shows the transfer; a blind retry does not double-apply (receipt) | **new** |
| Migration interruption and resume | X | kill the importer after N guilds, restart | idempotent: total counts and per-doc checksums identical to an uninterrupted run | **new** |
| Multi-guild isolation | M/X | interleave builds of guild A and B; try a cross-guild key | I7: no document written under the wrong key | **new** |
| Reconnaissance after migration | X | compare export vs import: per-guild `sum(wallet+bank)`, journal count, `next_case`, doc sha256 | I2, I3: all equal; `POST /v1/admin/verify` clean | **new** |

### E.3 Failure injection that does not exist yet (and is cheap)

1. **Rust: a commit barrier hook.** A `#[cfg(test)]` static that the WAL writer can pause at
   ("about to write", "written, not synced", "synced, not acknowledged") so a child process
   can be killed exactly at the interesting point. Without it, "crash before/after durable
   commit" cannot be tested deterministically — the existing crash harness kills at a random
   moment, which proves recovery but not the boundary.
   Suggested shape: an env-controlled pause (`PREPPYDB_TEST_PAUSE_AT=before_sync`) that only
   exists in test builds.
2. **Molly: a scriptable fake client** for `Store:run` that can fail on command N with
   `{status=nil}`, `{code="commit_outcome_unknown"}`, `{code="conflict"}`,
   `{code="not_found"}`, or return a *stale* envelope. Every scenario above then becomes a
   fast offline test in `tests/preppydb_store.lua`.
3. **Molly: a cross-process test** using two independent store instances over one server —
   the only way to prove that the CAS, not `busy_guilds`, prevents double spending.

### E.4 Recovery drills (manual, documented, not CI)

Disks fail in ways tests cannot fake. For the migration gate, record the outcome of:

1. `kill -9` during a coin operation, restart, `verify` → document intact, receipt consistent.
2. Corrupt one byte in the last WAL frame → server refuses to open (strict), the operator
   runs `preppydb verify`, then `--repair truncate` on a *copy*, then re-verifies sums.
3. `fsync` failure (device with `remount-ro` or a loop device returning EIO) → engine poisons,
   process restarts, state matches the last acknowledged transaction.
4. Restore a backup into a fresh directory and replay the reconciliation script against it.

---

## F. Prioritized roadmap

Ordering is by dependency: nothing in Molly should start before the server tells the truth
about durability and no longer poisons itself over an interrupted `fsync`.

### F.1 Small fixes (hours, low risk, do first)

| # | Target | Files | Change | Tests | Risk | Acceptance |
| --- | --- | --- | --- | --- | --- | --- |
| S1 | PreppyDB | `crates/preppydb-storage/src/wal.rs` | bounded retry on `ErrorKind::Interrupted` in `sync` (D.1); same in `rotate`/`truncate_current` paths | unit: inject an `Interrupted` once, assert the append succeeds and `successful_syncs` increments | low | engine no longer poisons on a spurious `EINTR`; `cargo test -p preppydb-storage` green |
| S2 | PreppyDB | `src/api.rs` (`/v1/version`), `src/engine.rs` | expose `sync_mode`, `durable_writes`, `max_body_bytes`, `max_doc_bytes` (D.3) | HTTP test asserting the fields and that `/v1/version` needs no token | low | a client can decide "durable or not" from one unauthenticated call |
| S3 | PreppyDB | `docs/API.md:76‑80`, `docs/DATA-MODEL.md` | replace the `wallets` example with an allowed collection; add a short "financial workload" note (sync always, whole-document CAS, receipts in-document, no dual-write) | doc build/links only | very low | no shipped example addresses a forbidden collection |
| S4 | Molly | `src/preppydb/connector.lua` (or a new `preflight.lua`) | startup preflight (D.5): version, `durable_writes`, body limit, `verify` | Luvit test with a stub client returning each wrong value | low | economy refuses to start on a non-durable or too-small configuration |

### F.2 Medium changes (days, the real work)

| # | Target | Files | Change | Tests | Risk | Acceptance |
| --- | --- | --- | --- | --- | --- | --- |
| M1 | PreppyDB | `wal.rs`, `engine.rs` (4 call sites) | `discard_tail` + `AppendFailure` + "provably not committed" error path (D.2) | new unit tests per call site: injected failure before write → `io_error` + engine still writable; after partial write → tail truncated, reopen shows no record; when the discard itself fails → `commit_outcome_unknown` + poison | **medium** — this is the only change that touches the durability story; a mistake here is worse than the bug | invariants I5/I6 hold under all three injected modes, and `commit_outcome_unknown` is *only* returned when the tail could not be made durable |
| M2 | Molly | new `src/persistence/preppydb_store.lua`, `global_store` twin, `src/config.lua`, `src/app.lua` | new store backend (D.4), config to select backend, SeaWorld backend retained | port `tests/economy_integration.lua` to the new backend; new `tests/preppydb_store.lua` with a scriptable fake client (E.3.2); cross-process CAS test (E.3.3) | medium | the whole existing economy suite passes unchanged against PreppyDB; duplicate/concurrent/insufficient/overflow assertions still hold |
| M3 | Molly | `tools/migrate/{export,import,reconcile}.lua` | export/import/reconcile per C.4 (1)–(7) | importer run twice → zero diff; interrupt/resume; reconciliation on synthetic mismatches | medium-high (money-adjacent, mechanical) | dry-run reconciliation empty and repeatable |
| M4 | Molly + ops | runbook, `docs/` | freeze, cutover, reverse import, rollback triggers (C.4 (8)–(10)) | full rehearsal on a copy of production data (never production) | high (process, not code) | rehearsal meets the readiness checklist |

### F.3 Major architectural work (only if the measurements demand it)

| # | Target | Change | Trigger to even consider it |
| --- | --- | --- | --- |
| L1 | Molly | Normalise money out of the aggregate document: `molly_accounts` (one doc per user, key `guild:user`), `molly_journal` (insert-only), receipts per guild; every transfer becomes a `POST /v1/transactions` batch of 3–4 ops with `expected_version` per op | measured guild documents approaching `max_doc_bytes`, or write latency above the Discord budget, or peak RSS above ~50 % of the device |
| L2 | PreppyDB | Per-tenant authorisation, per-collection quotas, `Idempotency-Key` support | only if a *second* application shares the database; none of Molly's requirements need it |

L1 is deliberately **not** part of this migration: it rewrites the money code, which is the
one part of Molly where a subtle bug is unrecoverable. It is a post-migration project with
its own reconciliation.

---

## G. Independent critique

The three strongest reasons this proposal could fail.

**G.1 Write amplification will bite long before the collections limit does.**
Every coin operation rewrites the *entire* guild document: JSON serialisation, full apply
under the engine's global write lock, one WAL frame of that size, one `fsync`. PreppyDB has a
single writer, so all guilds serialise behind that lock. From the CI measurement in
`docs/RESOURCE_STORAGE_DESIGN.md` (400-document × 1 KiB batches, ~20 ms p50 with
`sync_mode=always` on hosted x86-64), a 0.5 MiB guild document costs roughly the same order
of magnitude per operation — tens of milliseconds per coin flip on fast hardware, plausibly
100–250 ms on a Pi 4 class device. That is fine for a bot with a few busy guilds and
unusable for a large one.
*How to test:* build a synthetic data set at the real distribution (measure it from the
export), then measure p50/p95 write latency and peak RSS on the target device for 10, 100 and
1 000 guilds. Pre-decide the abort threshold (e.g. p95 > 300 ms or RSS > 50 % of RAM at the
expected guild count) *before* running it.

**G.2 `commit_outcome_unknown` is an availability event, and the reconciliation window is
bounded.**
The engine poisons itself and refuses *reads* until the process restarts (`engine.rs:1047`,
`get_opt`). Molly's supervisor must therefore restart it; if it does not, every command
fails with 503 — including `/balance`. Worse, reconciliation is only possible while the
receipt is still in the document, and receipts are capped at 1 000 per guild with a
`replay_expired` fail-closed rule (`docs/v2-economy-modes.md`). After 1 000 further
requests, an ambiguous transfer can no longer be resolved either way: money is neither
provably moved nor provably not. The honest mitigation is small: before sending a money
request, append its request id to a tiny local journal file and `fsync` it; delete the entry
when the outcome is known. Then reconciliation is always possible, even after the receipt
window rolls over.
*How to test:* force a 503, push >1 000 requests through the guild, then attempt
reconciliation — first without the local journal (must expose the hole), then with it (must
resolve every pending id).

**G.3 The Luvit event loop is the least-tested part of the whole chain.**
`Store:run` is synchronous at every call site, and the connector's own documentation says
its methods "require a synchronous injected transport or an appropriate event-loop `wait`
function" (`docs/preppydb-connector.md`). Inside Luvit there is no blocking socket: either
the store blocks the loop (heartbeat/music/voice stalls, gateway disconnects) or it yields
inside `fn(data)` — and `ledger.lua` was never written to survive a yield in the middle of a
read-modify-write, because on SeaWorld the whole thing happened on one connection.
*How to test:* measure gateway heartbeat jitter and command p95 while a synthetic workload
runs; add an assertion that `fn` never yields (wrap it and check `coroutine.isyieldable`
state transitions); keep the per-guild `busy` guard.

A fourth, less severe: `busy_guilds` protects only one process. Two Molly processes (or a
crash-restart overlap) rely entirely on the CAS — which is correct, but means the
`serialization_failed` path must be *reachable and tested*, not decorative.

---

## H. Final recommendation

**1. Is the WAL issue a small bug or a larger architectural problem?**
Neither extreme. The *architecture* is sound and already fail-closed: one record per commit,
`fsync` before acknowledgement, poison on ambiguity, torn tails only at the last segment's
end, corruption never silently dropped. The *implementation* has two small defects (no
`EINTR` retry; no "provably not committed" path) whose only consequence is unnecessary
availability loss. The genuinely architectural observation is different: the system assumes
an operator restarts the process after an ambiguous commit, and nothing in the stack
guarantees that yet.

**2. Can the existing batch API safely support Mollycoins?**
Yes — but it does not need the batch. One conditional `PUT` per guild document is a
complete, atomic, lost-update-free commit for Molly's current data model, and it keeps
money, journal and receipts in one atomic unit. The batch (`POST /v1/transactions`) becomes
necessary only if money is ever normalised across documents, and it is already sufficient
for that too, because every operation carries its own `expected_version`.

**3. What is the smallest correct change that unblocks Molly?**
`src/persistence/preppydb_store.lua` (≈80 lines) plus the `global_store` twin, moving guild
state to one collection keyed by guild id — **and nothing on the PreppyDB side is strictly
required for correctness**. The two PreppyDB changes I do recommend (interrupt-safe
`fsync`, "provably not committed") buy availability, and the `sync_mode` exposure buys the
ability to refuse a non-durable deployment. None of them changes the on-disk format.

**4. Which component should be implemented first?**
The PreppyDB `sync_mode`/`durable_writes` exposure (S2) and the interrupt-safe `fsync`
(S1): they are hours of work, they remove the two ways a healthy server can look broken, and
they are prerequisites for an honest Molly preflight. Then the Molly store backend (M2),
which is where 90 % of the risk and all of the value sit. The `discard_tail` work (M1) is
important but can land in parallel — it does not block Molly.

**5. What must be verified before production migration?**
The C.4 (10) checklist, and above all: measured maximum guild document size; measured write
latency and peak RSS at the expected guild count on the target hardware; an empty
reconciliation report on two consecutive dry-runs; an interrupted import that resumes
correctly; a rehearsed reverse import; the whole economy suite (duplicate request,
concurrency, insufficient funds, overflow, restart) green against PreppyDB; and one
end-to-end `commit_outcome_unknown` drill with the supervisor restarting the server. Until
`sync_mode = always` is asserted by the client, nothing in the economy should be considered
durable — regardless of what the documentation says.

---

## Appendix: what could not be verified

* **SeaWorld DB's internals** (transaction isolation, export format, crash recovery). Its
  source was not available; the assumptions are listed in B.4/A3–A4 and are *not* facts.
* **Actual guild document sizes and guild counts.** All capacity statements are estimates
  derived from Molly's caps (1 000 journal rows, 1 000 receipts, uncapped accounts) and
  PreppyDB's measured ratios (~2.2× dataset peak RSS, 1.7 s checkpoint per ~120 MB,
  ~20 ms per 400 KiB synced batch on hosted x86-64). They must be replaced by measurements
  from the export before cutover.
* **Luvit runtime behaviour** of the connector under a real event loop; no Luvit was
  available. The synchronous-call assumption is a documented requirement of the connector,
  not something observed here.
* **Any test result.** No Rust toolchain and no Luvit runtime were present; nothing in this
  document was compiled, let alone run. The tests referenced as "exists" are test names
  found by reading the files.
