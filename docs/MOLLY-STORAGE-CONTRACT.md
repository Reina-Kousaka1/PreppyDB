# Storage contract for a Molly (or similar) adapter

Audience: whoever writes the persistence adapter on the application side. Everything here
is verified against PreppyDB `932f4c9` unless it says "estimate" or "not verified".

This document deliberately does **not** add an economy API to PreppyDB. Money, receipts and
their invariants stay in the application; the database guarantees atomic, durable,
compare-and-set document writes.

## 1. Data shape: one collection, one document per tenant

Use a single collection whose document key is the tenant id:

```
collection "molly_guilds", key "<guild id>", value { schema_version: 1, guild: "<id>", data: { … } }
```

Why not one collection per guild: `Limits::default()` allows **256 collections per
database** (`crates/preppydb-storage/src/store.rs:289`) and that limit comes from
`EngineConfig::new`, which always uses `Limits::default()` (`engine.rs:229‑239`); no server
configuration or API call overrides it. A guild-per-collection model therefore stops at 256
tenants. Key-per-document does not: `max_docs_per_collection` is 5 000 000 and
`max_key_len` is 512 (`store.rs:290‑292`).

Also note what the collection-name restriction actually is: an exact-name list
(`economy_balances`, `wallet`, `wallets`, `balance`, `balances`, `credit_ledger`,
`debit_ledger`) rejected **when a collection is created** (`api.rs:113‑123`) and for
server-side Lua mutations (`api.rs:1189`). It is a naming guardrail, not a review of what a
document contains: `molly_guilds` and documents whose *fields* are called `wallet` are not
affected. Do not rename a forbidden collection to work around the guardrail — the intended
policy is "the database does not model money", which a document store satisfies by keeping
the money in application documents.

## 2. The write: conditional PUT

| | |
| --- | --- |
| Read | `GET /v1/collections/{c}/documents/{key}` → `data.value`, `data.version` |
| Write | `PUT /v1/collections/{c}/documents/{key}` with `{"value": …, "expected_version": N}` |
| Insert-only | `expected_version: 0` — fails if the document already exists |
| Conflict | `409` with `error.code = "conflict"` and `retryable: true`; no write happened |
| Success | `200` with `data.version` = the new version |

Verified in `api.rs:1456‑1545` (request parsing, `PutOptions`, conflict mapping through
`db_error`) and `engine.rs` (`put` → `commit_document`).

A successful `PUT` is atomic and durable under `SyncMode::Always`: the whole document is one
WAL record, applied in memory and appended under the engine write lock, and `fsync`
completes before the response is written (`wal.rs:458‑460`). Readers cannot observe the
new value before the append succeeds (`engine.rs:1329`).

## 3. Atomicity of balances, inventory, journal and receipts

Because they all live in the same document, one `PUT` covers them:

* a transfer, its journal row, its receipt and the inventory change commit or vanish
  together — there is no window in which a balance moved but its receipt did not;
* no multi-document transaction is required. `POST /v1/transactions` (per-operation
  `expected_version`, one WAL record) exists for the case where the data is split later;
* `POST /v1/lua/execute` is *not* on this path: it is admin-only, disabled by default
  (`lua_execution_unavailable`), and its script has no database handle — the caller passes
  input and the server applies the returned operations with CAS.

## 4. Conflict and retry rules

| Situation | What the adapter must do |
| --- | --- |
| `409 conflict` (no write applied) | re-read the document, re-run the operation, retry with the new version. Safe, because nothing was written. Bound the retries. |
| `503 commit_outcome_unknown` | **do not retry.** The record may or may not be in the log. Reopen (restart the server) and reconcile by reading the document and its receipt for that request id. |
| Transport error / no response | treat as unknown, not as failure. Same reconciliation path. |
| `422 limit_exceeded` | the document is too large or has too many operations; fix the data model, do not retry. |
| `503 database_locked` | another process owns the directory; retry after a short delay. |

`commit_outcome_unknown` is `retryable: false` (`error.rs:55‑60`, `api.rs:2419`) and — this
is the part client authors miss — while the engine is poisoned **every** request fails,
including reads (`engine.rs:1047`, `get_opt` at `engine.rs:1599`). The process must be
restarted by its supervisor before the data can be inspected.

Reconciliation is only as good as the retention of the receipt you reconcile against. If
receipts are capped and rotated, an old ambiguous request eventually becomes unresolvable;
the adapter should therefore record request ids it has not yet resolved in its own durable
file until the outcome is known.

## 5. Size and performance limits

| Limit | Default | Where |
| --- | --- | --- |
| Document size | 4 MiB (`max_doc_bytes`) | `store.rs:291`; a collection may set its own `max_doc_bytes` at creation, and that value *replaces* the default (`store.rs:554`) |
| Request body | 4 MiB (`max_body_bytes`), 512 KiB in the `tiny` profile | `config.rs:83`, `config.rs:107‑113` |
| Operations per batch | 1000 (`max_batch_ops`) | `store.rs:294` |
| Collections | 256 | `store.rs:289` |
| Documents per collection | 5 000 000 | `store.rs:290` |
| WAL frame | 64 MiB | `wal.rs:31` |

Two consequences worth stating plainly:

1. A document near the `max_doc_bytes` default cannot be sent, because the JSON envelope
   has to fit in `max_body_bytes`. Size both together, from a measurement of the largest
   real document.
2. There is **one writer**. Every commit takes the engine write lock and waits for its own
   `fsync`. Measured on hosted CI (see `docs/RESOURCE_STORAGE_DESIGN.md`, run #148):
   ~20 ms p50 for a 400-document/400 KiB batch with `sync_mode=always`. Rewriting one large
   document per operation therefore costs roughly the same order of magnitude *per
   operation*, plus a few times that on a Raspberry Pi (estimate, not measured). Reads block
   while a writer holds the lock.

Storage overhead measured on the same workload: whole-process peak RSS ≈ 2.2 × dataset size,
checkpoint ≈ 1.7 s per ~120 MB. Plan the device's memory around the *total* document size,
not the per-document limit.

## 6. Luvit / event-loop constraints

`Store:run`-style read-modify-write is synchronous at every call site in a Luvit bot. Two
options, both with traps:

* **Synchronous transport**: the current official Lua client needs a synchronous HTTP
  transport or an `options.wait` pump; inside Luvit every request blocks the loop, so
  gateway heartbeats and voice suffer during a slow write.
* **Coroutine + async client**: the domain callback must never yield inside the
  read-modify-write, or two operations can interleave on the same document and the second
  one's CAS will (correctly) fail — but the in-memory callback may already have mutated
  shared state.

The per-tenant in-process mutex the adapter already uses is an optimisation, not a safety
mechanism: the CAS is what prevents lost updates, and it must be exercised by a
cross-process test, not only by an in-process one.

## 7. Investigation: append-failure classification (not implemented)

Question: can a *provably zero-byte* append failure be reported as "not committed" instead
of poisoning the engine?

* What is provably distinguishable: after a failed `write_all` (`wal.rs:450`), the segment's
  `metadata().len()` either grew or did not. If it did not grow, the failed record is not in
  the file at all, so it can never be replayed and "not committed" would be a fact.
* What is **not** distinguishable: a failed `fsync` after a complete write (`wal.rs:458`).
  The frame may or may not be on the device; that case must stay `commit_outcome_unknown`.
* Why it is not implemented here: five commit sites (`engine.rs:1128/1174/1205/1430/1523`)
  and at least six fault-injection tests encode today's contract, including one that
  distinguishes the recovered value for a partial write versus a pre-sync failure. Changing
  the contract is a WAL behaviour change that needs a locally executed suite to justify; it
  was not available in this environment (see the report), and "make the restart go away" is
  not a good enough reason to touch recovery semantics. The conservative behaviour is
  retained deliberately.

If someone implements it: the retry is only safe when everything before the failed record is
already durable (`unsynced_frames == 0`), the check must use `metadata().len()` rather than
the writer's cached `current_len`, a `set_len` + `fsync` must complete before reporting
"not committed", and a failure of that truncation must poison exactly as today. Tests: zero
bytes written, partial frame, sync failure after a complete frame, and truncation failure —
each asserting both the returned code and the state after reopen.

## 8. Migration and rollback tests required before any cutover

The database side is ready for an adapter; it is **not** ready for a production migration
until these exist and pass on the target hardware:

1. Export → import → export again produces byte-identical canonical documents (keys sorted,
   integers as digits).
2. Import interrupted at three different phases resumes without duplicates and reconciles.
3. Per-tenant reconciliation: account count, sums of balances, journal row count, receipt
   count and watermarks equal the frozen source.
4. `POST /v1/admin/verify` clean on the target, and `GET /v1/admin/recovery` empty.
5. Duplicate request id → exactly one effect (receipt in the same document).
6. Two independent processes writing one document → exactly one wins, the other gets
   `409`; no lost update.
7. Kill the server between the write and the response → after restart the document is
   either fully there or fully absent, and the client's unknown is reconciled.
8. Reverse import back into the source database, then reconciliation of the same figures.
9. Peak RSS and p95 write latency measured at the expected tenant count on the target
   device, with a pre-agreed abort threshold.
10. Backup taken immediately before, restored into a scratch directory, verified.
