# PreppyDB / SeaWorld hardening parity

This comparison uses SeaWorld DB main at `d6589e61564c91e2e013df69a98d0544e3026032`
and PreppyDB PR #1 at `aaeeeaf288a03cd12909341633cfc92d1c79cba9`, inspected on
2026-11-08. SeaWorld is a read-only reference; no SeaWorld files were changed.

This is an engineering comparison, not a production-readiness declaration. CI proves
only the tested jobs and failure model. Process-crash tests do not prove survival of
power loss, filesystem bugs, device-cache loss, or hostile modification by another
process.

## Parity matrix

| Area | SeaWorld mechanism | PreppyDB current state | Missing guarantee / difference | Transferable approach | Needed evidence | Risk |
|---|---|---|---|---|---|---|
| WAL and commit protocol | V2–V4 append one checksummed transaction batch with begin/mutations/commit; sync succeeds before memory publication. | Segmented checksummed WAL; prepared mutations are undone in memory on append/sync error; handle is poisoned and reports unknown outcome. | Similar outcome ambiguity is unavoidable after a complete marker may reach the OS. PreppyDB's segmentation, rotation, and multi-record recovery create additional boundaries. | Keep explicit commit marker and publish memory only after the configured durability barrier; make the unknown outcome machine-readable and require reopen before retries. | Inject append, partial write, sync, rotation, and reopen failures for single, batch, and transaction paths on Linux and Windows. | High |
| Sync failure semantics | API success follows `File::sync_all`; documented as an OS request, not universal power-loss durability. | `Always` syncs commits; interval mode acknowledges before background sync and poisons on failure; sync/append errors can have indeterminate durable outcome and poison the handle. New correctness-sensitive export uses `try_export_snapshot`; the legacy infallible `export_snapshot` is deprecated but retained for source compatibility. | Callers of legacy infallible accessors can still observe a stale in-memory view after an unknown outcome; interval mode also has a deliberate acknowledged-write loss window. | Use fallible health-checked APIs for all correctness-sensitive reads/exports; keep legacy infallible methods clearly deprecated and document reopen-before-retry semantics. | Inject sync failure, assert checked reads/stats/export/backup reject the poisoned handle, reopen, then compare recovered state. | High |
| Multi-segment corruption and torn tails | One append log. Incomplete final transaction/record suffix is discarded; a complete bad checksum or semantic error fails open. | Multiple WAL segments; streaming scanner; complete CRC failures are corruption; torn final tails may be truncated; sequence gaps, regressions, and torn non-final segments fail closed; explicit repair may remove later suffix segments. | Segment ordering and suffix deletion add repair crash windows absent from a single file. The plan still marks the full checkpoint/multi-segment corruption matrix incomplete. | SeaWorld's rule “never skip a complete corrupt record” is already followed. Extend failure injection around each destructive repair step and repeat open to prove idempotence. | Corrupt/truncate first, middle, and final segments around checkpoints; inject truncate, unlink, and directory-sync failures; verify strict/read-only preservation and second reopen. | High |
| Backup and restore publication | Backup writes a compacted file to a new path, reopens/verifies it, then publishes with no-overwrite hard link. Restore requires a nonexistent target and publishes only a verified copy; no in-place replacement. | Backup archives a stable engine view and now validates archive checksums plus the extracted database before publishing. Restore validates staging and uses an intent marker; replacing an existing target moves it aside and then renames staging into place. | PreppyDB's two-rename replacement has an interruption window; intent blocks open and requires manual inspection/recovery. SeaWorld avoids that replacement problem by refusing existing targets. | Prefer create-new restore target as the safe default; if force replacement remains, preserve old data, durable intent, fail-closed open, and explicit recovery instructions. | Crash at each rename, marker creation/retirement, directory barrier and rollback; verify old/new paths and idempotent reopen on Windows/Linux. | Critical |
| Filesystem sync and Windows | Files are synced before publication; parent directory sync is attempted where supported. Windows and device/filesystem limits are expressly documented. | POSIX directory sync is used; Windows moves use write-through APIs in selected paths, but the helper cannot flush parent directories. No power-loss guarantee is made. | Neither design proves power-loss durability on all filesystems. PreppyDB replacement operations involve more directory transitions. | Separate file-content durability from directory-entry durability in docs and errors; treat failed/unsupported barriers as explicit limits. Avoid promising universal Windows atomicity. | Native Windows tests for replacement/no-replace and process crashes; Linux fsync injection; document that power-loss remains unqualified. | High |
| Audit and pricing mutation | SeaWorld has no pricing subsystem or pricing audit log; not directly comparable. | Pricing audit is bounded, streamed and checked on reopen. A durable `started` intent precedes a DB transaction; `committed`/`aborted` follows it. Startup reconciles exact before/after images after WAL recovery, and unresolved intents block further pricing writes. Audit file and database still are not one atomic transaction. Valid suffix deletion has no external trusted anchor. | The recovery protocol handles tested process-crash windows, but cannot make two independent files atomic; an indeterminate storage outcome remains unresolved until reopen/reconciliation. A valid removed audit suffix is undetectable without an external anchor. | Keep the intent/reconciliation protocol and fail closed. If stronger atomic audit semantics are required, move the authoritative outbox/anchor into the same DB transaction before adding another independent persistence protocol. | Crash after intent, after DB commit and before terminal audit append; injected audit append/sync failures; reopen twice; verify exact before/after reconciliation and fail-closed behavior for ambiguous state; test suffix deletion limitations. | High |
| Crash/fault injection | Dedicated child-process crash harness covers transaction, page/root publication, recovery, backup and compaction process-kill boundaries. | Rust crash harness plus injected WAL, manifest, restore and sync failures; live CI also performs kill/restart persistence tests. | Not all OS errors and multi-segment repair transitions are exercised; process termination is not power-loss testing. | Reuse explicit named failpoints at durability boundaries; assert before-or-after state and reopen twice, without sleeps as synchronization. | Full matrix across commit, repair, checkpoint, backup, restore and recovery; Linux and Windows runtime CI. | High |
| ARM64 / Raspberry Pi | Rust 1.89 is in CI; Linux/Windows engine jobs. No ARM64 qualification is declared by the project. | ARM64 cross-build and QEMU tests plus multi-arch Docker image and persistence jobs pass on PR head. | QEMU is not physical Raspberry Pi qualification; storage media, kernel/filesystem and memory pressure differ. | Keep QEMU as build/regression coverage; add a non-production hardware qualification run before claiming Pi support. | On-device create/write/kill/reopen/verify/backup/restore on the intended Pi model, OS, filesystem and storage medium. | High |
| CI, MSRV, Clippy | Ubuntu and Windows CI; MSRV Rust 1.89 check; format, two Clippy modes, tests, release build and crash-test feature. | Current PR head CI: Linux, Windows, ARM64/QEMU, Lua/Luvit and Docker jobs green; Rust and tests are locked in CI. | CI green does not close the missing original finding, absent independent review, or incomplete fault matrix. | Treat CI as necessary, not sufficient; link each finding to a targeted regression and platform job. | Repeat complete CI after every final fix and obtain independent review after the finding inventory is reconciled. | Medium |

## Current qualification snapshot

- Refreshed on 2026-11-08 for code revision `aaeeeaf288a03cd12909341633cfc92d1c79cba9`.
  The comparison table reflects the current PreppyDB PR head and the SeaWorld main
  revision stated at the top.
- PR #1 remains open and not draft. GitHub reports `mergeable=true`; that is merge
  mechanics, not a finding-closure or independent-review decision.
- CI run [#134](https://github.com/Reina-Kousaka1/PreppyDB/actions/runs/42502774832)
  completed successfully for this exact head: Linux x86-64, Windows x86-64, ARM64/QEMU,
  Lua/Luvit, Docker image builds, Docker persistence, and aggregate status. This qualifies
  the tested jobs for `aaeeeaf`; it does not prove physical Raspberry Pi behavior or
  universal power-loss durability. The current ARM64 CI artifact is commit-named and
  expires on 2026-12-08; it is not a stable release channel.
- The execution plan classifies WAL atomicity, WAL recovery, Windows manifest behavior,
  backup/restore, HTTP, authentication, pricing audit, clients, Ruby/Lua runtime paths,
  platform qualification, and performance qualification as `PARTIAL`.
- Pricing audit reconciliation is recoverable for exact recorded before/after images,
  but is not atomic across the independent audit file and database. Valid audit-suffix
  deletion still needs an external trusted anchor to be detectable.
- Finding inventory remains `OPEN`: the original review reports 15 findings, while only
  14 can be uniquely identified from the available report and repository records.
- No independent final review has been recorded; there are no submitted PR reviews or
  inline review threads.
- Remaining documented qualification limits include interrupted restore replacement
  requiring fail-closed/manual recovery, no Windows parent-directory flush or universal
  power-loss guarantee, non-atomic pricing-audit/database mutation, no external trusted
  anchor for detecting removal of a valid audit suffix, incomplete fault/performance
  matrices, and ARM64 QEMU rather than physical Raspberry Pi qualification.
- SeaWorld main remains at `d6589e61564c91e2e013df69a98d0544e3026032`; that commit only
  changes Dependabot configuration, so it does not alter the compared engine behavior.
  The SeaWorld engine CI evidence remains from the preceding engine change.

## Merge-readiness decisions

### A. Experimental merge to `main`

**Recommendation: keep PR #1 blocked for now.** The tested CI matrix is green, but the
original review inventory is incomplete (15 findings were reported and only 14 can be
traced), no independent final review exists, and several durability/fault-injection
boundaries remain open. “Experimental” labeling would not resolve those evidence gaps.
Do not merge until the missing finding is recovered or explicitly resolved by its source,
and an independent reviewer has checked the remaining high-risk storage and auth paths.

### B. Production readiness

**Recommendation: not production-ready.** No physical Pi 4B qualification or 100,000-row
resource run has been recorded; the ARM64 results are QEMU-only and the artifact is
temporary. Windows directory-entry/power-loss behavior is not universally guaranteed,
restore can require manual fail-closed recovery after interruption, the pricing audit
and database are not one atomic transaction, and a valid audit-log suffix removal has no
external trusted anchor. These are explicit qualification limits, not failures hidden by
the green CI result.

## Decisions for this parity effort

1. Do not copy SeaWorld's file layout or format. Keep PreppyDB's segmented WAL and
   archive model unless a reproduced correctness failure requires a format change.
2. Preserve the “outcome unknown, reopen before retry” contract after any post-append
   sync error. Durable external idempotency is not implied.
3. For restore, keep the default target-nonexistent path as the low-risk operation.
   Any replacement mode must remain fail-closed and must not delete the prior database.
4. Do not state that pricing audit and mutation are atomic until a single recoverable
   transaction covers both or tests prove an equivalent protocol.
5. Treat physical Raspberry Pi qualification as a separate operational test, not
   something inferred from ARM64/QEMU CI.
