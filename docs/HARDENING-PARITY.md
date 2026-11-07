# PreppyDB / SeaWorld hardening parity

This comparison uses SeaWorld DB main at `d6589e61564c91e2e013df69a98d0544e3026032`
and PreppyDB PR #1 at `c76052945456c3391569e7230c8507ff6ea6984f`, inspected on
2026-11-07. SeaWorld is a read-only reference; no SeaWorld files were changed.

This is an engineering comparison, not a production-readiness declaration. CI proves
only the tested jobs and failure model. Process-crash tests do not prove survival of
power loss, filesystem bugs, device-cache loss, or hostile modification by another
process.

## Parity matrix

| Area | SeaWorld mechanism | PreppyDB current state | Missing guarantee / difference | Transferable approach | Needed evidence | Risk |
|---|---|---|---|---|---|---|
| WAL and commit protocol | V2–V4 append one checksummed transaction batch with begin/mutations/commit; sync succeeds before memory publication. | Segmented checksummed WAL; prepared mutations are undone in memory on append/sync error; handle is poisoned and reports unknown outcome. | Similar outcome ambiguity is unavoidable after a complete marker may reach the OS. PreppyDB's segmentation, rotation, and multi-record recovery create additional boundaries. | Keep explicit commit marker and publish memory only after the configured durability barrier; make the unknown outcome machine-readable and require reopen before retries. | Inject append, partial write, sync, rotation, and reopen failures for single, batch, and transaction paths on Linux and Windows. | High |
| Sync failure semantics | API success follows `File::sync_all`; documented as an OS request, not universal power-loss durability. | `Always` syncs commits; interval mode acknowledges before background sync and poisons on failure; sync/append errors can have indeterminate durable outcome and poison the handle. | PreppyDB callers need to inspect state after reopen before retrying an unknown outcome. Interval mode has a deliberate acknowledged-write loss window. | Preserve the typed unknown-outcome error; ensure every read/write/backup entry point refuses a poisoned handle and document interval-mode loss bounds honestly. | Cross-path tests for poisoned reads, stats, backup, checkpoint and close; crash immediately before/after sync. | High |
| Multi-segment corruption and torn tails | One append log. Incomplete final transaction/record suffix is discarded; a complete bad checksum or semantic error fails open. | Multiple WAL segments; streaming scanner; complete CRC failures are corruption; torn final tails may be truncated; sequence gaps, regressions, and torn non-final segments fail closed; explicit repair may remove later suffix segments. | Segment ordering and suffix deletion add repair crash windows absent from a single file. The plan still marks the full checkpoint/multi-segment corruption matrix incomplete. | SeaWorld's rule “never skip a complete corrupt record” is already followed. Extend failure injection around each destructive repair step and repeat open to prove idempotence. | Corrupt/truncate first, middle, and final segments around checkpoints; inject truncate, unlink, and directory-sync failures; verify strict/read-only preservation and second reopen. | High |
| Backup and restore publication | Backup writes a compacted file to a new path, reopens/verifies it, then publishes with no-overwrite hard link. Restore requires a nonexistent target and publishes only a verified copy; no in-place replacement. | Backup archives a stable engine view and now validates archive checksums plus the extracted database before publishing. Restore validates staging and uses an intent marker; replacing an existing target moves it aside and then renames staging into place. | PreppyDB's two-rename replacement has an interruption window; intent blocks open and requires manual inspection/recovery. SeaWorld avoids that replacement problem by refusing existing targets. | Prefer create-new restore target as the safe default; if force replacement remains, preserve old data, durable intent, fail-closed open, and explicit recovery instructions. | Crash at each rename, marker creation/retirement, directory barrier and rollback; verify old/new paths and idempotent reopen on Windows/Linux. | Critical |
| Filesystem sync and Windows | Files are synced before publication; parent directory sync is attempted where supported. Windows and device/filesystem limits are expressly documented. | POSIX directory sync is used; Windows moves use write-through APIs in selected paths, but the helper cannot flush parent directories. No power-loss guarantee is made. | Neither design proves power-loss durability on all filesystems. PreppyDB replacement operations involve more directory transitions. | Separate file-content durability from directory-entry durability in docs and errors; treat failed/unsupported barriers as explicit limits. Avoid promising universal Windows atomicity. | Native Windows tests for replacement/no-replace and process crashes; Linux fsync injection; document that power-loss remains unqualified. | High |
| Audit and pricing mutation | SeaWorld has no pricing subsystem or pricing audit log; not directly comparable. | Pricing audit is bounded, streamed and checked on reopen, but its append and the database mutation are separate and non-atomic. Valid suffix deletion before first verification has no external anchor. | A crash or I/O error can leave a mutation without an audit entry or vice versa; local hash/checksum cannot detect a fully removed suffix without an external trusted anchor. | First define the audit contract. For atomicity, put audit intent and pricing mutation in the same PreppyDB transaction or implement a recoverable transactional outbox; do not claim atomicity from two file writes. | Crash at audit append, database commit and response; reopen and reconcile both directions; test tamper/truncation and idempotent recovery. | High |
| Crash/fault injection | Dedicated child-process crash harness covers transaction, page/root publication, recovery, backup and compaction process-kill boundaries. | Rust crash harness plus injected WAL, manifest, restore and sync failures; live CI also performs kill/restart persistence tests. | Not all OS errors and multi-segment repair transitions are exercised; process termination is not power-loss testing. | Reuse explicit named failpoints at durability boundaries; assert before-or-after state and reopen twice, without sleeps as synchronization. | Full matrix across commit, repair, checkpoint, backup, restore and recovery; Linux and Windows runtime CI. | High |
| ARM64 / Raspberry Pi | Rust 1.89 is in CI; Linux/Windows engine jobs. No ARM64 qualification is declared by the project. | ARM64 cross-build and QEMU tests plus multi-arch Docker image and persistence jobs pass on PR head. | QEMU is not physical Raspberry Pi qualification; storage media, kernel/filesystem and memory pressure differ. | Keep QEMU as build/regression coverage; add a non-production hardware qualification run before claiming Pi support. | On-device create/write/kill/reopen/verify/backup/restore on the intended Pi model, OS, filesystem and storage medium. | High |
| CI, MSRV, Clippy | Ubuntu and Windows CI; MSRV Rust 1.89 check; format, two Clippy modes, tests, release build and crash-test feature. | Current PR head CI: Linux, Windows, ARM64/QEMU, Lua/Luvit and Docker jobs green; Rust and tests are locked in CI. | CI green does not close the missing original finding, absent independent review, or incomplete fault matrix. | Treat CI as necessary, not sufficient; link each finding to a targeted regression and platform job. | Repeat complete CI after every final fix and obtain independent review after the finding inventory is reconciled. | Medium |

## Current qualification snapshot

- PreppyDB PR #1 is open at the head above; GitHub reports `CLEAN` merge state and all
  displayed CI jobs successful. This is merge mechanics, not a finding closure decision.
- The execution plan classifies WAL atomicity, WAL recovery, Windows manifest behavior,
  backup/restore, HTTP, authentication, pricing audit, clients, Ruby/Lua runtime paths,
  platform qualification, and performance qualification as `PARTIAL`.
- Finding inventory remains `OPEN`: the original review reports 15 findings, while only
  14 can be uniquely identified from the available report and repository records.
- No independent final review has been recorded.
- SeaWorld's current main CI includes Rust 1.89, Ubuntu, and Windows checks. The latest
  main commit was a Dependabot-only commit; its passing Dependabot checks are not engine
  test evidence. The engine CI run identified in the SeaWorld repository history passed
  on the preceding engine change.

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
