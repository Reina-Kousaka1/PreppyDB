# Bounded-Memory Storage Design (Proposal)

This document records the storage boundary observed in the current implementation and
defines a migration path to a bounded-memory backend. It is a design proposal only: it
does not change the current backend, on-disk format, public API, or durability contract.

## Current implementation and measured limit

The active state is `Engine::state: RwLock<State>`, where `State` owns a `Store`. A
`Store` owns every collection, each collection owns every full `Doc` in a
`BTreeMap<String, Doc>`, and every secondary index owns another in-memory ordered map
from encoded values to sets of document keys. Snapshot and WAL files are persistence and
recovery inputs; they are not a page cache or lazy document source. Opening a database
reconstructs this complete state. Secondary indexes are rebuilt from snapshot documents
and then updated by WAL replay.

Two RAM costs are consequently proportional to database size:

1. the live documents and their JSON values;
2. the in-memory index entries, including repeated key storage per index.

Checkpointing already serializes from the locked store without cloning the whole store.
Snapshot recovery decodes frames directly into a private `Store`, avoiding an additional
full-document vector. `Engine::verify` intentionally constructs a second replay `Store`
while the live `Store` remains resident so it can check that the persisted snapshot and
WAL are semantically replayable. This preserves a useful integrity check but makes its
peak approach the memory cost of two stores.

The Tiny-profile synthetic measurements in [TESTING.md](TESTING.md#corrected-phase-separated-tiny-run-pr-head-0e63e168)
show the current scale on one x86-64 WSL2 host: idle RSS is about 3.8 MiB; the one-run
verify high-water mark was 25.17 MiB at 5,000 documents, 35.05 MiB at 7,500 documents,
and 43.98 MiB at 10,000 documents. Those are workload-specific measurements, not
capacity guarantees or Raspberry Pi qualification.

## Target properties

A future bounded-memory backend should make resident memory primarily a function of
configured cache and transaction budgets, rather than total document count. It must
retain the current logical API and preserve:

- atomic batches and transaction commit ordering;
- the WAL's committed-prefix and recovery rules;
- stable document versions, sequence watermarks, TTL behavior, schemas, and uniqueness;
- ordered range scans and index-vs-full-scan equivalence;
- backups, restore verification, compaction, and concurrent readers;
- existing snapshot/WAL readers and Java client compatibility.

It must also avoid a hidden unbounded cache in the pager, index layer, query executor,
backup path, or verification path. Memory limits should be explicit for the page cache,
transaction staging, result materialization, and any pinned pages held by iterators.

## Proposed physical layout

Keep the logical document model and public API above a storage interface. Add a
page-oriented backend with checksummed, versioned pages and copy-on-write roots:

```text
Public API / query semantics
            |
Logical transaction and validation layer
            |
Record/WAL source of truth for recent commits
            |
Page manager + bounded cache
  + primary document tree
  + secondary index trees
  + catalog/schema tree
            |
Filesystem
```

The page manager would cache pages in a fixed-size LRU (or clock) pool. A point read
loads only its primary-tree path and the target document. Scans iterate ordered leaves
and return bounded batches. Index leaves point to stable document keys; index changes
and the document mutation are published under one root generation. Any page used by a
reader is pinned only for the duration of its bounded operation or iterator batch.

The initial migration should keep the existing logical WAL record format as the source
of truth for acknowledged operations. A checkpoint materializes the WAL prefix into a
new immutable page generation, validates page checksums and tree invariants, then
publishes a manifest containing the generation roots and sequence watermark. Only after
that publication may covered WAL segments be retired. Recovery opens the last published
page generation and replays the retained logical WAL suffix. This keeps the commit point
independent of background page materialization and avoids making a new page layout the
only recovery source during rollout.

This design still requires a precise page-write/publication protocol: pages must be
written and synced before the manifest can point at them; old roots and WAL history must
remain available until the new manifest is published; and recovery must reject mixed or
incomplete generations. A universal power-loss guarantee must not be claimed beyond the
actual filesystem and device guarantees.

## Explicit migration and compatibility plan

Do not reinterpret the current snapshot format in place and do not silently convert a
database during ordinary `open`.

1. **Specify first.** Define page headers, page IDs, format version, checksums, root
   metadata, limits, allocation behavior, orphan handling, and publication/recovery
   states. Keep this version separate from the public API version.
2. **Read-only prototype.** Implement a page reader/writer behind a private interface
   and test it against generated logical stores. Do not make it the default backend.
3. **Differential qualification.** For the same deterministic operation histories,
   compare the legacy backend and page backend for point reads, scans, indexes,
   uniqueness, TTL, transactions, reopen, backup, restore, and compaction. Inject a
   crash at every page write, sync, and root publication boundary.
4. **Explicit offline migration.** Read the legacy manifest, snapshot, and WAL using the
   existing bounded parsers. Write the complete converted state into a unique temporary
   generation. Verify its logical contents, IDs, index definitions, checksums, and
   watermarks; sync it; then publish a new format marker/root through an atomic,
   recoverable protocol. Preserve the legacy files until a separate verified backup
   exists. A failed migration leaves the original database openable and unchanged.
5. **Compatibility period.** Retain read-only legacy-format support and an explicit
   migration/rollback procedure. Never silently downgrade a page-backed database to a
   legacy snapshot that omits page roots or index state.
6. **Default only after evidence.** Consider changing the default backend only after
   Linux, Windows, ARM64, crash-recovery, and real Raspberry Pi qualification. The
   in-memory backend remains selectable until the page backend passes the same semantic
   suite and operational review.

Migration identity must be explicit and idempotent. The source's database ID, document
keys/versions, collection metadata, index definitions, expiry values, and last sequence
must survive exactly. Re-running after a crash must either resume from a validated
temporary generation or discard only that unreferenced temporary generation; it must
never infer that an incomplete target is an empty database.

## Verification under a memory budget

Do not remove semantic WAL replay validation to meet an RSS target. A future verifier
must keep its working set bounded while independently checking page checksums, B+Tree
ordering, child/root references, manifest watermarks, WAL framing/CRC/sequence, and
transaction semantics (including uniqueness and schema constraints). Suitable designs
to evaluate include replay into an isolated temporary page store with a hard page-cache
budget, followed by an ordered logical comparison, or an external-memory verifier that
uses bounded sorted runs. Verification must fail closed on scratch-space exhaustion,
corruption, or resource limits and must not modify the active database.

The verifier must not validate pages solely by trusting the same index traversal whose
correctness it is meant to establish. It should walk each referenced page with cycle and
bounds checks, then compare primary-document and secondary-index contents using an
independent bounded method. Orphan-page reporting may be best-effort if allocation
metadata can prove reachability without loading the full database.

## Decisions still required before implementation

- page size and compression policy;
- append-only generations versus reusable free-page map;
- whether secondary index roots are committed in the same page generation or rebuilt;
- cache budget defaults for Tiny and Standard profiles;
- behavior when a query's sort/result exceeds its memory budget;
- migration command and retained-legacy-file policy;
- platform-specific atomic replacement and directory durability contract.

These are storage-format and recovery decisions, not tuning constants. They need a
separate reviewed design and fault-injection plan before the current V1 backend is
replaced or any existing database is migrated.
