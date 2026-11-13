# Storage Engine Hardening V1

This review and implementation is based on `main` at
`932f4c9f3c63aea941f3b2abc5713351cfd9f4e4`. It is limited to local storage
integrity and recovery; it does not claim universal power-loss durability.

## Findings and changes

### WAL synchronization interruption

`WalWriter::sync` previously returned immediately when `File::sync_data` returned
`Interrupted`. The engine correctly treated append/sync failures as an unknown commit
outcome and poisoned the handle, but a retryable OS interruption could needlessly
turn a commit into an unknown outcome. `sync` now retries `Interrupted` at most eight
times after the initial attempt. Other errors and exhausted retries still fail. The
pending unsynced-frame count is cleared only after a successful sync.

Deterministic fault-injection tests cover success after eight interrupted calls,
failure after the bounded retry limit, retention of unsynced state after failure, and
a later successful retry. The WAL frame format is unchanged.

### WAL record decoding

The writer has always emitted `Record { seq, ts, op }`, and put operations always
emitted a `v` member (with explicit JSON `null` represented as a value). The decoder
previously defaulted a missing or mistyped `ts` to zero and a missing put value to
JSON `null`, silently accepting malformed complete records. It now rejects those
records as corruption. The serialized format is unchanged; explicit `null` remains
valid. Deterministic tests cover the malformed fields, every strict prefix of a valid
record payload, and fixed byte mutations without adding a property-testing dependency.

## Existing guarantees observed

- Document writes and transactions are serialized by the engine writer lock. A
  transaction is a single WAL batch; commit failure does not publish a partial
  in-memory mutation, and an uncertain append/sync poisons the handle until reopen.
- Recovery streams WAL records, bounds frame size, distinguishes torn final frames
  from complete CRC corruption, checks contiguous sequence numbers, and fails closed
  when a manifest watermark cannot be reached.
- Checkpoints publish immutable snapshot/manifest generations before retiring covered
  WAL segments. Backup and restore use staging, verification, and explicit recovery
  state; process-crash tests exercise publication boundaries.
- The `always` mode acknowledges only after `sync_data` succeeds. This is bounded by
  OS, filesystem, device, and controller behavior. Process-crash tests are not
  power-cut tests. Windows parent-directory flush is not implemented, so universal
  Windows power-loss durability is not established.

## Validation on this revision

- Storage crate tests on Windows/MSVC: 168 unit tests passed, 2 crash-child helpers
  ignored in their standalone slots; 64 integration tests passed, 1 child helper
  ignored in its standalone slot and exercised by its parent.
- Default-feature workspace tests on Windows/MSVC: 411 passed, 0 failed, 3 existing
  crash-child helper slots ignored and invoked by their parent tests.
- `cargo test --workspace --all-features --locked` failed only when two Ruby-query
  integration tests attempted to spawn `ruby`, which is not installed on this host.
  The default-feature workspace suite passed. Repository CI installs Ruby 3.3; that
  CI remains required to qualify the Ruby-enabled matrix.
- Workspace `cargo check --all-targets --all-features` could not complete on this
  Windows host: the vendored `mlua-sys` build invokes MSVC `lib.exe`, which returns
  `LNK1114` while trying to overwrite its Lua archive. Strict workspace Clippy passed
  with default and all features, and storage Clippy passed. This compiler/build-script
  limitation must be checked on CI/Linux before review completion.
- The installed Rust 1.89 toolchain entry is incomplete (`rustup` reports a missing
  toolchain manifest); MSRV-specific local tests were therefore not run. The
  repository workflow currently installs stable rather than explicitly testing 1.89.
- The release `commit_scaling` example was run at the clean baseline and this change,
  with 101 `SyncMode::Never` overwrites after 1k/10k/50k-document setup:

  | Documents | Baseline median / p95 | This change median / p95 |
  | ---: | ---: | ---: |
  | 1,000 | 5,200 / 5,600 ns | 5,100 / 5,500 ns |
  | 10,000 | 5,400 / 5,800 ns | 5,400 / 5,900 ns |
  | 50,000 | 5,400 / 6,000 ns | 5,400 / 5,700 ns |

  This small Windows smoke benchmark does not exercise the sync path and is not a
  portable performance claim. It indicates no measurable regression in this
  diagnostic workload.

Full feature-enabled workspace check/Clippy and Ruby integration qualification remain
open due the local Windows toolchain/runtime limitations. Linux CI is needed for the
full feature matrix. No physical power-loss or long-duration soak test was run.
