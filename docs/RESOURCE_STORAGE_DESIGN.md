# Bounded-Memory Storage Design (Proposal)

## Resource evidence (collected 2026-11-08; code baseline `b2c4f56`)

The measurement binaries were built from code commit
`b2c4f563ab40e74351666895f02ebeb881679adb`. Subsequent commits through `fe85e40` changed
this document only. GitHub Actions run [#150](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42523479376)
passed after a targeted retry of one Windows smoke-test job. It exercised the same code
baseline on Linux, Windows, emulated Linux ARM64, Lua/Luvit, Docker, the crash harness, and
the bounded resource benchmark. ARM64 uses QEMU, not Raspberry Pi hardware. No result here
establishes Raspberry Pi capacity or real-device qualification; CI artifacts are not
releases.

### Hosted bounded 100,000-document CI measurement (run #148)

Run #148 used the release server, `tiny` profile, 100,000 deterministic synthetic documents
with 1,024 payload bytes each, one scalar index, batch size 400, a 60-second idle window,
and seed `20261108`. It ran on GitHub Actions Ubuntu 22.04 x86-64 with a 1.5 GiB
address-space limit, 15-minute CPU limit, 2 GiB per-file limit, and 128-file limit. The
temporary benchmark data directory was removed. Actions tested synthetic merge revision
`e053d423a9bfa86ee9d498f96f59d91983cc19f3`, containing PR head `b2c4f563ab40e74351666895f02ebeb881679adb`.
The machine-readable result is in the
[run #148 resource benchmark artifact](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42521488292/artifacts/13634200361)
(90-day retention).

| Measurement | Result |
| --- | ---: |
| Whole-run wall time | 90.106 s |
| Process peak RSS (kernel high-water mark) | 232,554,496 bytes (221.71 MiB) |
| Idle RSS / CPU after health check (60.06 s) | 3.76 MiB / 0.17% of one core |
| Threads at idle | 3 |
| Checkpoint RSS before → after | 207,982,592 → 208,048,128 bytes (198.35 → 198.42 MiB) |
| Sampled checkpoint-phase peak RSS | 208,048,128 bytes (198.42 MiB) |
| Peak phase | Verify: 232,554,496 bytes (221.71 MiB) |
| Checkpoint duration | 1.727 s |
| Recovery startup after isolated child SIGKILL | 1.518 s |
| Write batch p50 / p95 | 19.775 / 20.591 ms per 400-document batch |
| Read p50 / p95 | 20.111 / 20.234 ms |
| Snapshot / WAL after checkpoint | 121,347,033 / 12 bytes |

This confirms low observed idle use and that the verifier peak fell below the earlier
run #140 result. The workload still needs about 222 MiB at peak for 100,000 documents,
far above the provisional small-load target if applied at this scale. This synthetic
hosted x86-64 result is not a Raspberry Pi measurement or a capacity guarantee.

### Matched CI comparison for PR head `471ef53` (run #154)

GitHub Actions [run #154](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42527969164)
measured merge checkout `52c933dead764e4894fad890d5be114346444a35` (PR head
`471ef53d7a055617493e725dbe2f62dc705ad724` merged into base `20e3b591acca52ae20ffefe3cc1075ec15034ad1`)
and a pre-optimization baseline at `f0eb9c3678a8309308a1b6c184a92b815f566a1b` in the same
Ubuntu x86-64 job. Both used Tiny, 100,000 deterministic 1,024-byte documents, one `group`
index, batch size 400, seed `20261108`, and a 60-second idle interval. Each configuration
was measured once.

| Measurement | Merge checkout `52c933d` (PR head `471ef53`) | Baseline `f0eb9c3` |
| --- | ---: | ---: |
| Idle RSS / CPU / threads | 3.81 MiB / 0.13% / 3 | 3.74 MiB / 0.13% / 3 |
| Whole-process peak RSS (`VmHWM`) | 213.40 MiB | 376.61 MiB |
| Sampled Verify-phase RSS peak | 212.84 MiB | 288.62 MiB |
| Checkpoint RSS before → after | 198.41 → 198.43 MiB | 201.71 → 201.71 MiB |
| Bounded scan (`limit=100`) duration | 21.3 ms | 305.7 ms |
| Scan-phase sampled RSS peak | 198.41 MiB | 376.61 MiB |

This is evidence for the combined changes present on `471ef53`, including bounded scan
materialization and compact verification replay; it does not isolate the effect of either
change. In this single pair, whole-process peak was 43.3% lower and sampled Verify-phase
RSS was 26.3% lower. It is a matched synthetic run on hosted x86-64, not a Raspberry Pi
result or a repeated-run estimate. The verifier representation added later is measured in
run #159 below.

### Current PR-head qualification for `f2de769` (run #159)

GitHub Actions [run #159](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42572148462)
measured merge checkout `52b92b58e5bb654e9d005d0aac20f43c71ce2cf6` (PR head
`f2de769a37844219efe9fba219eb4b3b770accfb` merged into base
`20e3b591acca52ae20ffefe3cc1075ec15034ad1`) and the same pre-optimization baseline
`f0eb9c3678a8309308a1b6c184a92b815f566a1b` in one Ubuntu x86-64 job. Both runs used Tiny,
100,000 deterministic documents with 1,024-byte payloads, one `group` index, batch size
400, seed `20261108`, and a 60-second idle interval. Each configuration was measured once.

| Measurement | Merge checkout `52b92b5` (PR head `f2de769`) | Baseline `f0eb9c3` |
| --- | ---: | ---: |
| Idle RSS / CPU / threads | 3.89 MiB / 0.15% / 3 | 3.73 MiB / 0.18% / 3 |
| Whole-process peak RSS (`VmHWM`) | 212.02 MiB | 376.60 MiB |
| Sampled Verify-phase RSS peak | 211.34 MiB | 288.65 MiB |
| Checkpoint RSS before → after | 198.44 → 198.45 MiB | 201.70 → 201.70 MiB |
| Checkpoint duration | 2.120 s | 1.901 s |
| Recovery startup after SIGKILL | 1.321 s | 1.321 s |

The current merge checkout used 43.7% less whole-process peak RSS and 26.8% less sampled
Verify-phase RSS than the matched pre-optimization baseline in this single pair. This is a
branch-level comparison, not an isolated measurement of the `f2de769` verifier change. The
workload uses one ordinary `group` index and does not stress sparse documents with many
unique indexes, which is the new verifier representation's targeted case. It is a hosted
x86-64 measurement, not a Raspberry Pi result or a repeated-run estimate.

### Repeat of the current resource qualification (run #160)

Run #160 repeated the same paired benchmark on documentation-only head
`cb84912d52397a2b0b804aeade3e8d6484cb989b`; the measured engine source was unchanged from
run #159. It again used Tiny, 100,000 deterministic 1,024-byte documents, one `group`
index, batches of 400, seed `20261108`, and a 60-second idle interval. Each configuration
was measured once in the same Ubuntu x86-64 job. See [run #160](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42573094086).

| Measurement | Current branch | Pre-optimization baseline |
| --- | ---: | ---: |
| Idle RSS / CPU / threads | 3.79 MiB / 0.13% / 3 | 3.68 MiB / 0.15% / 3 |
| Whole-process peak RSS (`VmHWM`) | 212.07 MiB | 376.53 MiB |
| Checkpoint RSS before → after | 197.86 → 198.35 MiB | 201.70 → 201.70 MiB |
| Checkpoint duration | 1.933 s | 2.075 s |
| Recovery startup after SIGKILL | 1.426 s | 1.413 s |
| Write batch p50 / p95 | 19.751 / 20.820 ms | 19.755 / 20.829 ms |

The repeated pair shows 43.7% less whole-process peak RSS on this workload, consistent with
run #159. The tiny differences in idle readings, checkpoint time, and write latency are
single-run observations and should not be interpreted as a measured regression or speedup.
This second run is still hosted x86-64 evidence, uses the same synthetic workload, and does
not establish performance or memory use on a physical Raspberry Pi. The 3 idle threads
include the main server thread, the configured single Tiny HTTP worker, and the signal
handler thread; Tiny configures one HTTP worker, not a total process-thread count of one.

### Current small-load Tiny-profile WSL measurements

The release server binary for PR source `b2c4f563ab40e74351666895f02ebeb881679adb` was
also measured locally in Ubuntu WSL2 x86-64 using the repository's
`scripts/benchmark-resources.py`. Runs used Tiny, a 900-byte deterministic payload (about
926 encoded bytes per document), one scalar index, seed `20261108`, 10 seconds idle,
50-write batches for 100 documents and 500-write batches otherwise. The benchmark limited
the temporary server to 1.5 GiB address space, 900 CPU seconds, 2 GiB per file, and 128
open files. Data directories were temporary and removed. Each cell is a single run.

| Documents | Idle RSS | Idle CPU (10 s) | Threads | Process peak RSS (VmHWM) | RSS after checkpoint | RSS after index build | Checkpoint time | Recovery start after SIGKILL |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 3.86 MiB | 0.10% | 3 | 4.96 MiB | 4.80 MiB | 4.75 MiB | 19.1 ms | 111 ms |
| 1,000 | 3.90 MiB | 0.10% | 3 | 12.98 MiB | 9.89 MiB | 11.55 MiB | 26.3 ms | 121 ms |
| 10,000 | 3.88 MiB | 0.00% | 3 | 28.96 MiB | 26.15 MiB | 27.54 MiB | 91.2 ms | 120 ms |

Whole-run CPU was 0.24%, 0.72%, and 1.73% of one core respectively; these percentages
include writes, reads, recovery, verification, and shutdown, so they are not idle CPU.
The 10,000-document peak was below the provisional 40 MiB small-load target in this
specific workload. It does not imply a 40 MiB cap: resident document and index state grows
with document size, index count, and data count. These are WSL2 observations, not physical
Raspberry Pi measurements.

### Historical bounded 100,000-document CI measurement (run #140)

Run #140 used the release server, `tiny` profile, 100,000 deterministic synthetic documents
with 1,024 payload bytes each, one scalar index, batch size 400, a 60-second idle window,
and seed `20261108`. It ran on GitHub Actions Ubuntu 22.04 x86-64 with a 1.5 GiB
address-space limit, 15-minute CPU limit, 2 GiB per-file limit, and 128-file limit. The
temporary benchmark data directory was removed. The PR source head was
`f98ba65c3023e5641e35f0939ca03bf3831fd4ac`; Actions tested synthetic merge revision
`4923079ea7b7d8a835a254de8d6c57b258bbe577`. The machine-readable current and matching
pre-optimization results are in the
[run #140 benchmark artifact](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42512188019/artifacts/13629009734)
(90-day retention).

| Measurement | Result |
| --- | ---: |
| Whole-run wall time | 90.255 s |
| Process peak RSS (kernel high-water mark) | 304,009,216 bytes (289.93 MiB) |
| Idle RSS / CPU after health check (60.06 s) | 3.74 MiB / 0.1% of one core |
| Threads at idle | 3 |
| Checkpoint RSS before → after | 207,953,920 → 207,970,304 bytes (198.32 → 198.34 MiB) |
| Sampled checkpoint-phase peak RSS | 208,056,320 bytes (198.42 MiB) |
| Peak phase | Verify/stats/shutdown: about 289.93 MiB sampled and kernel high-water |
| Checkpoint duration | 1.917 s |
| Recovery startup after isolated child SIGKILL | 1.412 s |
| Write batch p50 / p95 | 19.814 / 20.835 ms per 400-document batch |
| Read p50 / p95 | 20.109 / 20.408 ms |
| Snapshot / WAL after checkpoint | 121,347,033 / 12 bytes |
| WAL after recovery tail writes | 443,184 bytes |
| Available system memory at start / observed minimum | 15,690,514,432 / 15,494,545,408 bytes |

The earlier paired scan comparison in run #139 built the pre-scan implementation from
`f0eb9c3`: a 100-document full scan took 333.236 ms and raised the process high-water mark
by 186,675,200 bytes (178.03 MiB). The then-current implementation returned the same 100
documents in 21.622 ms and raised that high-water mark by 139,264 bytes (0.13 MiB). This
is evidence that pagination-before-cloning removed the scan-sized transient copy for that
workload. Run #140 rechecked the same code path: current took 20.975 ms and did not exceed
the process high-water mark already reached during loading. Verify/stats/shutdown still
dominate the total peak near 290 MiB. These are hosted x86-64 comparisons, not a 4 GiB Pi
simulation or a capacity promise; the runner had more than 14 GiB available memory and the
benchmark imposes per-process limits only.

This document records the storage boundary observed in the current implementation and
defines a migration path to a bounded-memory backend. It is a design proposal only: it
does not change the current backend, on-disk format, public API, or durability contract.

## Current implementation and measured limit

The active state is `Engine::state: RwLock<State>`, where `State` owns a `Store`. A
`Store` owns every collection, each collection owns every full `Doc` in a
`BTreeMap<Arc<str>, Doc>`, and every secondary index owns another in-memory ordered map
from encoded values to sets of shared document-key handles. Snapshot and WAL files are
persistence and recovery inputs; they are not a page cache or lazy document source. Opening a database
reconstructs this complete state. Secondary indexes are rebuilt from snapshot documents
and then updated by WAL replay.

The `tiny` server profile is a request/concurrency preset, not an aggregate database RAM
quota: it configures one HTTP worker, a backlog of four accepted connections, a 512 KiB
HTTP body limit, and disables periodic TTL cleanup. The default storage limits allow up to
5,000,000 documents per collection, 4 MiB per document, and 1,000 operations per batch;
collection metadata can override the document count and size defaults. Transactions
separately cap staged writes at 32 MiB. These guardrails bound request and transaction
sizes and default collection cardinality, but they do not cap total resident state. There
is no bounded page cache in the current backend, so `tiny` memory use continues to grow
with the stored dataset. The 4 GiB Pi target therefore still depends on measured workload
size until a bounded-memory storage backend exists.

Two RAM costs are consequently proportional to database size:

1. the live documents and their JSON values;
2. the in-memory index entries and tree nodes. Document-key bytes are shared between
   the primary map and all secondary-index buckets with `Arc<str>` instead of copied for
   every index. A pointer-identity regression test verifies this invariant.

Document updates now validate all index constraints before replacing the old record.
Single-document engine writes move the displaced `Doc` into their rollback value instead
of deep-cloning it before every update/delete; unique-index conflicts are detected before
the current document is detached from its indexes. This also gives WAL append rollback
ownership of the old value without copying it. Atomic multi-operation batches retain the
old documents needed for rollback, subject to the existing undo-byte budget.

Checkpointing serializes from the locked store without cloning it. Snapshot document
frames stream JSON directly through the snapshot writer: checkpointing no longer clones
`Doc.value` into a `Doc::to_json()` object or materializes a complete encoded document
string. A fixed 16 KiB buffer is used for the final streaming body-hash pass; the frame
length and CRC are backpatched without changing the snapshot bytes. This trades one
sequential read of the just-written snapshot body for less transient per-document RAM.
Byte-identity tests compare the streamed payload and complete file with the prior format.
Snapshot recovery
decodes frames directly into a private `Store`, avoiding an additional full-document
vector. Before `f0eb9c3`, `Engine::verify` also reconstructed every full document in a
second `Store`, making verification approach the memory cost of two stores. At `f0eb9c3`,
the verifier was changed to retain only document keys, versions, collection metadata, and
all fields used by snapshot or WAL index definitions. The compact replay state described
below further avoids retaining versions, timestamps, and non-unique index state while
checking transaction replay and unique constraints. It still decodes and checksum-validates
the complete persisted snapshot and WAL; only its private semantic replay state is
projected. If the bounded index-field plan cannot be built safely, verification uses the
original full-document fallback.

The Tiny-profile synthetic measurements in [TESTING.md](TESTING.md#corrected-phase-separated-tiny-run-pr-head-0e63e168)
show the current scale on one x86-64 WSL2 host: idle RSS is about 3.8 MiB; the one-run
verify high-water mark was 25.17 MiB at 5,000 documents, 35.05 MiB at 7,500 documents,
and 43.98 MiB at 10,000 documents. Those are workload-specific measurements, not
capacity guarantees or Raspberry Pi qualification.

An additional paired WSL2 x86-64 comparison used the same Tiny harness, 900-byte payload,
seed `20261108`, and release profile against clean commit `f02d80f` and this working tree.
The 30-second idle observations were:

| Documents | Build | Idle RSS | Idle CPU (% of one core) | Threads | Process peak RSS | Checkpoint RSS |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 100 | `f02d80f` | 4.10 MiB | 0.13% | 3 | 5.74 MiB | 5.25 MiB |
| 100 | working tree | 3.85 MiB | 0.10% | 3 | 5.59 MiB | 5.11 MiB |
| 1,000 | `f02d80f` | 4.16 MiB | 0.10% | 3 | 10.36 MiB | 8.07 MiB |
| 1,000 | working tree | 3.90 MiB | 0.10% | 3 | 9.88 MiB | 8.02 MiB |
| 10,000 | `f02d80f` | 4.05 MiB | 0.10% | 3 | 44.11 MiB | 25.79 MiB |
| 10,000 | working tree | 3.93 MiB | 0.13% | 3 | 43.29 MiB | 27.86 MiB |

The one-index 10,000-document peak was 1.9% lower in this sample, but RSS immediately
after checkpoint was 2.07 MiB higher. At 100 and 1,000 documents both peak and checkpoint
RSS were modestly lower. These are single paired runs; allocator/RSS variance is visible
in the phase samples, so this is directional evidence rather than a portable performance
claim. A 10,000-document run still exceeds the provisional 40 MiB process-peak target.

The harness now accepts `--index-count` (1–16). A four-index stress comparison (four
separate non-unique indexes on the same `group` field, 3-second idle sample) measured
process peaks of 12.89 vs. 11.85 MiB at 1,000 documents, and 48.38 vs. 45.32 MiB at
10,000 documents (`f02d80f` vs. working tree). In the latter run, the post-index RSS was
25.77 MiB at baseline and 27.79 MiB in the working tree even though the working-tree
whole-process high-water mark was 3.06 MiB lower. This mixed phase-RSS result needs
repeated sampling; four identical-field indexes are a key-sharing stress case, not a
representative production workload. The working-tree 10,000-document peak with four
indexes was 45.32 MiB and still misses the 40 MiB objective.

A follow-up experiment compared the exact `3da4a96` baseline with a temporary hybrid
index bucket (up to 16 sorted keys in a vector, then a `BTreeSet`). Both release builds
used the same 10,000-document Tiny workload, 900-byte payload, seed `20261108`, and
3-second idle sample. The single-index peak was 43.50 MiB for both builds. With four
indexes, the baseline peak was 45.16 MiB and the hybrid-bucket peak was 46.21 MiB. The
index-build RSS and checkpoint RSS were effectively unchanged. The experiment therefore
did not reduce peak memory and was discarded; the current implementation retains
`BTreeSet` buckets. The peak still occurs during full-store semantic verification, so
changing only the per-value key container does not address the dominant allocation.

The working tree also moves a displaced document into the single-document rollback value
instead of deep-cloning the old document on each update/delete. A regression test verifies
restoration after an injected WAL append failure. This removes an extra O(document-size)
rollback copy from that path; no update-latency claim is made. For the verifier projection,
the engine first scans the retained WAL for index definitions, with limits of 4,096 distinct
field paths and 1 MiB of field-path bytes. The projected snapshot reader unions that plan
with index fields declared in the snapshot itself, enforcing the same bounds while it reads
the snapshot once. This matters when an index predates the snapshot and no retained WAL
record repeats its definition: later WAL mutations must still be replayed with that field, or
the verifier can miss a unique-index conflict that reopen rejects. Regression coverage now
checks this exact case. If the WAL is damaged or the combined plan exceeds those bounds,
verification falls back to the prior full-document replay path. The snapshot reader still
validates the complete serialized snapshot and its checksum; projection occurs only after
each document has been decoded. WAL operations are likewise decoded and validated in full,
then only the indexed paths are retained for semantic replay. This optimization applies only
to the read-only verifier, never to live state or recovery.

A single paired resource run compared the exact `3da4a96` baseline with this projected
verifier using the same Tiny workload, 10,000 documents, 900-byte payload, seed `20261108`,
release profile, and 3-second idle observation. With one secondary index, process high-water
RSS fell from 43.50 MiB to 34.55 MiB; with four same-field indexes, it fell from 45.16 MiB
to 36.50 MiB. The verifier remained the peak phase. These are directional results from one
synthetic WSL2 x86-64 run per case, not a portable guarantee or Raspberry Pi qualification.
Idle CPU sampled at 0.0% of one core, with three threads and about 3.9 MiB idle RSS. The
normal read/write path does not run the projection scan; the extra scan and projection cost
is paid only by explicit verification. Independent repeated runs and real-hardware tests
remain necessary before making broader performance claims.

### Compact semantic verifier comparison

Two paired WSL2 x86-64 runs per build used the same release benchmark, Tiny profile,
100,000 synthetic documents, 1,024-byte payload, one scalar index, batch size 400, seed
`20261108`, and 3-second idle observation. The baseline was clean commit
`b2c4f563ab40e74351666895f02ebeb881679adb`; the candidate changed only the read-only
semantic replay state used by `Engine::verify`. Both runs include checkpoint, recovery,
queries, verification, and shutdown. Temporary database directories were removed.

| Build | Process peak RSS, run 1 | Process peak RSS, run 2 | Peak phase | Write batch p50 range |
| --- | ---: | ---: | --- | ---: |
| Baseline | 232,124,416 bytes | 232,132,608 bytes | stats/shutdown | 20.06–20.10 ms |
| Compact verifier | 223,236,096 bytes | 223,313,920 bytes | stats/shutdown | 20.09–20.10 ms |

The paired process high-water mark fell by about 8.4 MiB (3.8%) in these four runs.
Idle RSS remained about 4 MiB and idle CPU sampled at 0.0% of one core; checkpoint and
recovery timings were effectively unchanged in these short runs. The verifier keeps the
document keys, bounded fields needed by unique indexes, and unique-index entries; it does
not retain full documents or MVCC metadata in this private replay store. Snapshot and WAL
records are still fully parsed and integrity-checked. If the field-plan bound is exceeded,
the established full-store verifier path remains the fallback. This is a targeted reduction
of verification memory on one synthetic workload, not a bound on database RSS, a real
Raspberry Pi result, or evidence that the in-memory storage architecture has been replaced.

All measurements are synthetic WSL2 x86-64 observations, not Raspberry Pi qualification.

### Streamed checkpoint frame allocation comparison

The streamed snapshot writer was measured on Ubuntu WSL2 x86-64 with the same benchmark
driver and one 900 KiB ASCII document. The baseline release binary was built from clean
commit `0aa628f` (the preceding direct-by-reference serializer); the candidate adds
streamed JSON frames and fixed-buffer body hashing. Each binary was run three times with
the standard profile, one-document batch, and a one-second idle sample. Checkpoint-phase
kernel high-water samples were 12.13, 12.17, and 12.15 MiB for baseline, versus 11.27 MiB
in all three candidate runs. Median checkpoint high-water fell from 12.15 MiB to 11.27 MiB
(about 0.88 MiB, or 7.2%).

Median checkpoint duration was 22.7 ms for baseline and 22.7 ms for the candidate; sampled
whole-run process CPU was 0.04 s in both medians. Whole-process high-water was essentially
unchanged (18.42 MiB baseline, 18.45 MiB candidate), because later phases dominate the
peak. This establishes a narrow checkpoint-allocation reduction on one synthetic host,
not lower total peak memory or Raspberry Pi qualification. The benchmark accepts
`--payload-bytes` so the case can be reproduced without modifying the harness.
At the parser's 1 MiB per-string limit, a separate single paired run measured 13.25 MiB
versus 12.25 MiB checkpoint-phase high-water, with checkpoint times of 22.7 ms and
22.8 ms. This boundary run is a smoke measurement, not an additional repeated estimate.

The bounded Tiny harness was also run against the release binary from `f0eb9c3` with a
900-byte payload, seed `20261108`, one secondary index, and a 3-second idle observation.
These runs are individual samples; the 100-document run used batches of 50, while the
1,000- and 10,000-document runs used batches of 500.

| Documents | Idle RSS | Idle CPU | Threads | Process peak RSS | RSS after checkpoint | Peak phase | Whole-run CPU | Wall time |
| ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: |
| 100 | 3.87 MiB | 0.0% | 3 | 4.98 MiB | 4.77 MiB | shutdown | 0.37% | 5.38 s |
| 1,000 | 3.85 MiB | 0.0% | 3 | 12.92 MiB | 9.83 MiB | writes/recovery setup | 0.89% | 23.61 s |
| 10,000 | 3.95 MiB | 0.0% | 3 | 34.55 MiB | 27.91 MiB | stats/verify | 2.39% | 24.24 s |

A separate 10,000-document run with four same-field indexes peaked at 36.50 MiB and
27.85 MiB after checkpoint. Thus the measured process peak was below the provisional
40 MiB target in both 10,000-document cases. The CPU percentages across each complete
benchmark include writes, reads, recovery, verification, and shutdown; they are not idle
CPU. Idle CPU was sampled for only three seconds, so these short observations do not
replace a longer idle test or a Raspberry Pi measurement.

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
