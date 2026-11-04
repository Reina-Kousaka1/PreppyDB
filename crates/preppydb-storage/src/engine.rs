//! The storage engine: recovery, write path, checkpoints, transactions, stats.
//!
//! ## Durability model
//!
//! * Every mutation is appended to the write-ahead log as one framed record.
//!   Multi-document batches and transactions are a *single* record, so they are
//!   atomic even across a crash.
//! * `SyncMode::Always` (default) `fsync`s before the call returns.
//! * A checkpoint writes a fully checksummed snapshot and then retires the log
//!   segments it replaced. Segments are only deleted after the manifest that
//!   references the new snapshot is durable on disk.
//! * Recovery replays every segment still on disk, skipping records already covered
//!   by the snapshot. A torn tail (partial last frame) is truncated and reported; any
//!   other checksum failure aborts recovery unless the operator explicitly opts into
//!   repair, and is always written to a `recovery-report-*.json` file.

use crate::catalog::{CollectionMeta, IndexMeta};
use crate::clock::now_ms;
use crate::error::{DbError, Result};
use crate::lock::{create_private_dir, FileLock};
use crate::manifest::{Manifest, SnapshotRef};
use crate::record::{Op, Record};
use crate::snapshot::{self, WrittenSnapshot};
use crate::store::{ApplyMode, Collection, Doc, Limits, Store};
use crate::wal::{self, SyncMode, WalWriter};
use preppydb_json::Json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// What to do when recovery finds corruption that is not a torn tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairPolicy {
    /// Refuse to open; the operator must inspect the data (default).
    Strict,
    /// Replay everything up to the first damaged record, record a note and continue.
    /// Data *after* the damaged record is lost — this is why it is opt-in.
    TruncateCorruptTail,
}

impl RepairPolicy {
    pub fn parse(text: &str) -> Result<RepairPolicy> {
        match text.to_ascii_lowercase().as_str() {
            "strict" => Ok(RepairPolicy::Strict),
            "truncate" | "repair" => Ok(RepairPolicy::TruncateCorruptTail),
            other => Err(DbError::Config(format!("unknown repair policy '{other}'"))),
        }
    }
}

/// Engine configuration.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Data directory (created if missing, mode 0700 on Unix).
    pub path: PathBuf,
    pub sync_mode: SyncMode,
    /// Rotate the WAL when a segment grows beyond this size.
    pub max_segment_bytes: u64,
    /// Resource limits for documents/collections.
    pub limits: Limits,
    /// Take a checkpoint automatically after this many mutations (`None` = never).
    pub checkpoint_every_mutations: Option<u64>,
    /// Open read-only (no WAL writes, no checkpoints, no locks).
    pub read_only: bool,
    /// Take the directory lock (disable only for read-only inspection of a live DB).
    pub take_lock: bool,
    pub repair: RepairPolicy,
}

impl EngineConfig {
    pub fn new(path: impl Into<PathBuf>) -> EngineConfig {
        EngineConfig {
            path: path.into(),
            sync_mode: SyncMode::Always,
            max_segment_bytes: 64 * 1024 * 1024,
            limits: Limits::default(),
            checkpoint_every_mutations: Some(50_000),
            read_only: false,
            take_lock: true,
            repair: RepairPolicy::Strict,
        }
    }
}

/// Options for a single `put`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PutOptions {
    /// `Some(0)` = insert-only, `Some(n)` = compare-and-set, `None` = unconditional.
    pub expected_version: Option<u64>,
    /// Absolute expiry (ms since epoch); falls back to the collection default.
    pub expires_at: Option<i64>,
}

/// An operation plus an optional compare-and-set expectation.
#[derive(Clone, Debug)]
pub struct CheckedOp {
    pub op: Op,
    /// `Some(0)` = must not exist, `Some(n)` = must be version n, `None` = unchecked.
    pub expected_version: Option<u64>,
}

/// Options for scans.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub prefix: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub reverse: bool,
    /// Include documents whose TTL has expired (maintenance/inspection only).
    pub include_expired: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            prefix: None,
            limit: 100,
            offset: 0,
            reverse: false,
            include_expired: false,
        }
    }
}

/// A note produced during recovery; surfaced in stats and the report file.
#[derive(Clone, Debug)]
pub struct RecoveryNote {
    pub at: i64,
    pub kind: String,
    pub detail: String,
}

impl RecoveryNote {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("at", Json::int(self.at))
            .with(
                "at_rfc3339",
                Json::str(crate::clock::format_rfc3339(self.at)),
            )
            .with("kind", Json::str(self.kind.clone()))
            .with("detail", Json::str(self.detail.clone()))
    }
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub gets: AtomicU64,
    pub puts: AtomicU64,
    pub deletes: AtomicU64,
    pub batches: AtomicU64,
    pub transactions: AtomicU64,
    pub conflicts: AtomicU64,
    pub sweeps: AtomicU64,
    pub checkpoints: AtomicU64,
    pub wal_bytes: AtomicU64,
}

/// Mutable engine state guarded by an ``RwLock``.
pub struct State {
    pub store: Store,
    pub seq: u64,
    pub generation: u64,
}

/// Overall engine statistics.
#[derive(Clone, Debug)]
pub struct EngineStats {
    pub path: String,
    pub last_seq: u64,
    pub generation: u64,
    pub sync_mode: String,
    pub read_only: bool,
    pub collections: Vec<CollectionStats>,
    pub total_documents: usize,
    pub wal_segments: usize,
    pub wal_bytes: u64,
    pub snapshot_bytes: u64,
    pub disk_bytes: u64,
    pub snapshot_seq: Option<u64>,
    pub uptime_ms: u64,
    pub metrics: Json,
    pub recovery_notes: Vec<RecoveryNote>,
}

#[derive(Clone, Debug)]
pub struct CollectionStats {
    pub name: String,
    pub documents: usize,
    pub live_documents: usize,
    pub deleted_pending: usize,
    pub indexes: Vec<String>,
    pub has_schema: bool,
    pub max_docs: Option<u64>,
}

impl EngineStats {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("path", Json::str(self.path.clone()))
            .with("last_seq", Json::uint(self.last_seq))
            .with("generation", Json::uint(self.generation))
            .with("sync_mode", Json::str(self.sync_mode.clone()))
            .with("read_only", Json::Bool(self.read_only))
            .with("total_documents", Json::uint(self.total_documents as u64))
            .with("wal_segments", Json::uint(self.wal_segments as u64))
            .with("wal_bytes", Json::uint(self.wal_bytes))
            .with("snapshot_bytes", Json::uint(self.snapshot_bytes))
            .with("disk_bytes", Json::uint(self.disk_bytes))
            .with(
                "snapshot_seq",
                self.snapshot_seq.map(Json::uint).unwrap_or(Json::Null),
            )
            .with("uptime_ms", Json::uint(self.uptime_ms))
            .with("metrics", self.metrics.clone())
            .with(
                "recovery_notes",
                Json::Array(self.recovery_notes.iter().map(|n| n.to_json()).collect()),
            )
            .with(
                "collections",
                Json::Array(
                    self.collections
                        .iter()
                        .map(|c| {
                            Json::obj()
                                .with("name", Json::str(c.name.clone()))
                                .with("documents", Json::uint(c.documents as u64))
                                .with("live_documents", Json::uint(c.live_documents as u64))
                                .with("expired_documents", Json::uint(c.deleted_pending as u64))
                                .with(
                                    "indexes",
                                    Json::Array(
                                        c.indexes.iter().map(|i| Json::str(i.clone())).collect(),
                                    ),
                                )
                                .with("has_schema", Json::Bool(c.has_schema))
                                .with("max_docs", c.max_docs.map(Json::uint).unwrap_or(Json::Null))
                        })
                        .collect(),
                ),
            )
    }
}

/// Result of a verification pass (`fsck`).
#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub ok: bool,
    pub snapshot: Option<Json>,
    pub segments: Vec<Json>,
    pub issues: Vec<String>,
}

impl VerifyReport {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("ok", Json::Bool(self.ok))
            .with("snapshot", self.snapshot.clone().unwrap_or(Json::Null))
            .with("segments", Json::Array(self.segments.clone()))
            .with(
                "issues",
                Json::Array(self.issues.iter().map(|i| Json::str(i.clone())).collect()),
            )
    }
}

/// Result of a compaction run.
#[derive(Clone, Debug)]
pub struct CompactReport {
    pub expired_swept: usize,
    pub snapshot: WrittenSnapshot,
    pub segments_removed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl CompactReport {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("expired_swept", Json::uint(self.expired_swept as u64))
            .with(
                "snapshot_file",
                Json::str(self.snapshot.path.display().to_string()),
            )
            .with("snapshot_seq", Json::uint(self.snapshot.header.seq))
            .with("documents", Json::uint(self.snapshot.header.doc_count))
            .with("segments_removed", Json::uint(self.segments_removed as u64))
            .with("bytes_before", Json::uint(self.bytes_before))
            .with("bytes_after", Json::uint(self.bytes_after))
    }
}

/// Paths inside the data directory.
#[derive(Clone, Debug)]
pub struct DataPaths {
    pub root: PathBuf,
    pub wal_dir: PathBuf,
    pub snap_dir: PathBuf,
}

impl DataPaths {
    pub fn new(root: impl Into<PathBuf>) -> DataPaths {
        let root = root.into();
        DataPaths {
            wal_dir: root.join("wal"),
            snap_dir: root.join("snap"),
            root,
        }
    }

    pub fn snapshot_file(&self, generation: u64) -> PathBuf {
        self.snap_dir.join(format!("snap-{generation:012}.pdb"))
    }
}

/// The PreppyDB storage engine.
pub struct Engine {
    cfg: EngineConfig,
    paths: DataPaths,
    state: RwLock<State>,
    writer: Mutex<WalWriter>,
    manifest: Mutex<Manifest>,
    lock: Mutex<Option<FileLock>>,
    notes: Mutex<Vec<RecoveryNote>>,
    poisoned: AtomicBool,
    mutations_since_checkpoint: AtomicU64,
    pub metrics: Metrics,
    started_at_ms: i64,
}

impl Engine {
    /// Open (or create) a database directory, replaying the log as needed.
    pub fn open(cfg: EngineConfig) -> Result<Engine> {
        let paths = DataPaths::new(cfg.path.clone());
        create_private_dir(&paths.root)?;
        create_private_dir(&paths.wal_dir)?;
        create_private_dir(&paths.snap_dir)?;

        let lock = if cfg.take_lock && !cfg.read_only {
            Some(FileLock::acquire(&paths.root)?)
        } else {
            None
        };

        let mut manifest = Manifest::load(&paths.root)?.unwrap_or_default();
        let mut notes: Vec<RecoveryNote> = Vec::new();
        let mut store = Store::new(cfg.limits.clone());
        let mut seq: u64 = 0;

        // ---- 1. load the newest snapshot referenced by the manifest --------------
        if let Some(snap_ref) = manifest.snapshot.clone() {
            let path = paths.snap_dir.join(&snap_ref.file);
            if path.exists() {
                let data = snapshot::read_snapshot(&path)?;
                if !snap_ref.sha256.is_empty() && data.body_sha256 != snap_ref.sha256 {
                    return Err(DbError::corrupt(
                        path.display().to_string(),
                        "snapshot SHA-256 does not match the manifest",
                    ));
                }
                for (meta, docs) in data.collections {
                    let name = meta.name.clone();
                    store.create_collection(meta, ApplyMode::Replay)?;
                    if let Some(coll) = store.collections.get_mut(&name) {
                        for (key, doc) in docs {
                            coll.docs.insert(key, doc);
                        }
                        coll.rebuild_indexes()?;
                    }
                }
                seq = data.header.seq;
            } else {
                notes.push(RecoveryNote {
                    at: now_ms(),
                    kind: "snapshot_missing".into(),
                    detail: format!(
                        "manifest references '{}' which no longer exists; replaying the full log",
                        snap_ref.file
                    ),
                });
            }
        }

        // ---- 2. replay every WAL segment found on disk --------------------------
        let segments = wal::list_segments(&paths.wal_dir)?;
        let mut last_segment_index = 0u64;
        let mut replayed_records: u64 = 0;
        'segments: for segment in &segments {
            let index = segment
                .file_name()
                .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
                .unwrap_or(0);
            last_segment_index = last_segment_index.max(index);
            let scan = wal::scan_segment(segment)?;
            if let Some(torn) = &scan.torn_tail {
                notes.push(RecoveryNote {
                    at: now_ms(),
                    kind: "torn_tail".into(),
                    detail: format!(
                        "{}: {torn} (truncating to {} bytes)",
                        segment.display(),
                        scan.valid_len
                    ),
                });
                if !cfg.read_only {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(segment)
                        .map_err(|err| DbError::io("opening WAL segment for truncation", err))?;
                    file.set_len(scan.valid_len)
                        .map_err(|err| DbError::io("truncating torn WAL tail", err))?;
                    file.sync_all()
                        .map_err(|err| DbError::io("syncing truncated WAL", err))?;
                }
            }
            if let Some(corruption) = &scan.corruption {
                let detail = format!("{}: {corruption}", segment.display());
                if cfg.repair == RepairPolicy::Strict {
                    return Err(DbError::corrupt(
                        "write-ahead log",
                        format!(
                            "{detail}. Refusing to open. Inspect the file or re-open with repair=truncate to drop everything after the damaged record."
                        ),
                    ));
                }
                notes.push(RecoveryNote {
                    at: now_ms(),
                    kind: "corruption_truncated".into(),
                    detail: detail.clone(),
                });
                // Records before the damage are still usable.
                for record in &scan.records {
                    if record.seq > seq {
                        store.apply(&record.op, record.ts, record.seq, ApplyMode::Replay)?;
                        seq = record.seq;
                        replayed_records += 1;
                    }
                }
                break 'segments;
            }
            for record in &scan.records {
                if record.seq <= seq {
                    continue; // already contained in the snapshot
                }
                store.apply(&record.op, record.ts, record.seq, ApplyMode::Replay)?;
                seq = record.seq;
                replayed_records += 1;
            }
        }

        if manifest.last_seq > seq {
            notes.push(RecoveryNote {
                at: now_ms(),
                kind: "manifest_ahead".into(),
                detail: format!(
                    "manifest last_seq {} is ahead of the replayed log ({}); keeping the higher value so sequence numbers are never reused",
                    manifest.last_seq, seq
                ),
            });
            seq = manifest.last_seq;
        }

        // ---- 3. open the WAL for appending -------------------------------------
        let start_index = if last_segment_index == 0 {
            1
        } else {
            last_segment_index
        };
        let mut writer = WalWriter::open(
            &paths.wal_dir,
            start_index,
            cfg.max_segment_bytes,
            cfg.sync_mode,
        )?;
        if writer.current_len() > cfg.max_segment_bytes {
            writer.rotate()?;
        }

        manifest.last_seq = seq;

        if !notes.is_empty() && !cfg.read_only {
            let report = Json::obj()
                .with("generated_at", Json::int(now_ms()))
                .with(
                    "generated_at_rfc3339",
                    Json::str(crate::clock::format_rfc3339(now_ms())),
                )
                .with("replayed_records", Json::uint(replayed_records))
                .with("last_seq", Json::uint(seq))
                .with(
                    "notes",
                    Json::Array(notes.iter().map(|n| n.to_json()).collect()),
                );
            let path = paths
                .root
                .join(format!("recovery-report-{}.json", now_ms()));
            let _ = crate::lock::write_private(
                &path,
                preppydb_json::to_string_pretty(&report).as_bytes(),
            );
        }

        let generation = manifest.generation;
        Ok(Engine {
            cfg,
            paths,
            state: RwLock::new(State {
                store,
                seq,
                generation,
            }),
            writer: Mutex::new(writer),
            manifest: Mutex::new(manifest),
            lock: Mutex::new(lock),
            notes: Mutex::new(notes),
            poisoned: AtomicBool::new(false),
            mutations_since_checkpoint: AtomicU64::new(0),
            metrics: Metrics::default(),
            started_at_ms: now_ms(),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn paths(&self) -> &DataPaths {
        &self.paths
    }

    pub fn recovery_notes(&self) -> Vec<RecoveryNote> {
        self.lock_mutex(&self.notes).clone()
    }

    // ------------------------------------------------------------- lock helpers

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|err| err.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|err| err.into_inner())
    }

    fn lock_mutex<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn check_writable(&self, context: &str) -> Result<()> {
        if self.cfg.read_only {
            return Err(DbError::ReadOnly(context.to_string()));
        }
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(DbError::Internal(
                "engine is in a failed state after a WAL write error; restart required".to_string(),
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------- write path

    /// Serialise, log and apply one operation.
    ///
    /// `mutate` must either apply its change to the store and return the equivalent log
    /// operation, or return an error *without* leaving partial state behind.
    fn commit<F>(&self, mutate: F) -> Result<u64>
    where
        F: FnOnce(&mut Store, u64, i64) -> Result<Op>,
    {
        self.check_writable("write")?;
        let mut state = self.write();
        let seq = state.seq + 1;
        let now = now_ms();
        let op = mutate(&mut state.store, seq, now)?;
        let record = Record::new(seq, now, op);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            // The in-memory state is ahead of the log now; continuing could silently
            // diverge. Poison the engine and let the operator restart (recovery will
            // rebuild a consistent state from the log).
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(err);
        }
        self.metrics
            .wal_bytes
            .fetch_add(record.encode_payload().len() as u64, Ordering::Relaxed);
        state.seq = seq;
        drop(writer);
        drop(state);
        self.mutations_since_checkpoint
            .fetch_add(1, Ordering::Relaxed);
        self.maybe_auto_checkpoint();
        Ok(seq)
    }

    fn maybe_auto_checkpoint(&self) {
        if let Some(threshold) = self.cfg.checkpoint_every_mutations {
            if self.mutations_since_checkpoint.load(Ordering::Relaxed) >= threshold {
                // A checkpoint failure here must not fail the write that triggered it:
                // the write is already durable in the log. The failure shows up in the
                // stats/recovery notes, and the next attempt retries.
                let _ = self.checkpoint();
            }
        }
    }

    /// Insert or replace a document.
    pub fn put(
        &self,
        collection: &str,
        key: &str,
        value: Json,
        options: PutOptions,
    ) -> Result<u64> {
        let version = std::cell::Cell::new(0u64);
        self.commit(|store, seq, now| {
            let written = store.put(
                collection,
                key,
                value.clone(),
                options.expires_at,
                options.expected_version,
                now,
                seq,
                ApplyMode::Live,
            )?;
            version.set(written);
            Ok(Op::Put {
                collection: collection.to_string(),
                key: key.to_string(),
                value,
                expires_at: options.expires_at,
            })
        })?;
        self.metrics.puts.fetch_add(1, Ordering::Relaxed);
        Ok(version.get())
    }

    /// Delete a document. `expected_version` implements compare-and-delete.
    pub fn delete(&self, collection: &str, key: &str, expected_version: Option<u64>) -> Result<()> {
        self.commit(|store, _seq, _now| {
            store.delete(collection, key, expected_version, ApplyMode::Live)?;
            Ok(Op::Delete {
                collection: collection.to_string(),
                key: key.to_string(),
            })
        })?;
        self.metrics.deletes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Apply a list of operations atomically.
    pub fn batch(&self, ops: Vec<Op>) -> Result<usize> {
        if ops.is_empty() {
            return Err(DbError::validation(
                "batch must contain at least one operation",
            ));
        }
        if ops.len() > self.cfg.limits.max_batch_ops {
            return Err(DbError::LimitExceeded {
                limit: "max_batch_ops",
                max: self.cfg.limits.max_batch_ops as u64,
                actual: ops.len() as u64,
            });
        }
        let count = ops.len();
        let label = "batch".to_string();
        self.commit(|store, seq, now| {
            let op = Op::Batch {
                ops: ops.clone(),
                label: label.clone(),
            };
            store.apply(&op, now, seq, ApplyMode::Live)?;
            Ok(op)
        })?;
        self.metrics.batches.fetch_add(1, Ordering::Relaxed);
        Ok(count)
    }

    /// Begin an optimistic transaction.
    pub fn begin(&self) -> crate::txn::Transaction<'_> {
        crate::txn::Transaction::new(self)
    }

    /// Commit a transaction: validate the read set, then apply all writes atomically.
    pub(crate) fn commit_transaction(
        &self,
        reads: &[(String, String, u64)],
        ops: Vec<Op>,
    ) -> Result<u64> {
        self.check_writable("transaction commit")?;
        let mut state = self.write();
        let now = now_ms();
        let mut conflicts = Vec::new();
        for (collection, key, version) in reads {
            let current = state.store.version_of(collection, key, now);
            if current != *version {
                conflicts.push(format!(
                    "{collection}/{key}: expected version {version}, found {current}"
                ));
            }
        }
        if !conflicts.is_empty() {
            self.metrics.conflicts.fetch_add(1, Ordering::Relaxed);
            return Err(DbError::TransactionConflict { details: conflicts });
        }
        let seq = state.seq + 1;
        let op = Op::Batch {
            ops,
            label: "transaction".to_string(),
        };
        state.store.apply(&op, now, seq, ApplyMode::Live)?;
        let record = Record::new(seq, now, op);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(err);
        }
        state.seq = seq;
        drop(writer);
        drop(state);
        self.metrics.transactions.fetch_add(1, Ordering::Relaxed);
        self.mutations_since_checkpoint
            .fetch_add(1, Ordering::Relaxed);
        self.maybe_auto_checkpoint();
        Ok(seq)
    }

    /// Apply a batch where every operation may carry a compare-and-set expectation.
    ///
    /// The whole batch is validated against the current versions inside the write lock
    /// and then committed as one log record, so either every operation applies or none
    /// does.
    pub fn batch_checked(&self, ops: Vec<CheckedOp>) -> Result<usize> {
        if ops.is_empty() {
            return Err(DbError::validation(
                "batch must contain at least one operation",
            ));
        }
        if ops.len() > self.cfg.limits.max_batch_ops {
            return Err(DbError::LimitExceeded {
                limit: "max_batch_ops",
                max: self.cfg.limits.max_batch_ops as u64,
                actual: ops.len() as u64,
            });
        }
        let count = ops.len();
        let label = "batch";
        self.commit(|store, seq, now| {
            let mut staged = store.clone();
            let mut log_ops = Vec::with_capacity(ops.len());
            for checked in &ops {
                match &checked.op {
                    Op::Put {
                        collection,
                        key,
                        value,
                        expires_at,
                    } => {
                        staged.put(
                            collection,
                            key,
                            value.clone(),
                            *expires_at,
                            checked.expected_version,
                            now,
                            seq,
                            ApplyMode::Live,
                        )?;
                    }
                    Op::Delete { collection, key } => {
                        staged.delete(
                            collection,
                            key,
                            checked.expected_version,
                            ApplyMode::Live,
                        )?;
                    }
                    other => {
                        return Err(DbError::validation(format!(
                            "batch_checked only supports put/delete, got {:?}",
                            other.touched_collections()
                        )))
                    }
                }
                log_ops.push(checked.op.clone());
            }
            *store = staged;
            Ok(Op::Batch {
                ops: log_ops,
                label: label.to_string(),
            })
        })?;
        self.metrics.batches.fetch_add(1, Ordering::Relaxed);
        Ok(count)
    }

    /// Create a collection.
    pub fn create_collection(&self, meta: CollectionMeta) -> Result<()> {
        self.commit(|store, _seq, _now| {
            store.create_collection(meta.clone(), ApplyMode::Live)?;
            Ok(Op::CreateCollection { meta: meta.clone() })
        })?;
        Ok(())
    }

    /// Drop a collection. This is destructive and never implicit: callers must pass
    /// `confirm = true`.
    pub fn drop_collection(&self, collection: &str, confirm: bool) -> Result<()> {
        if !confirm {
            return Err(DbError::validation(
                "dropping a collection requires explicit confirmation (confirm=true)",
            ));
        }
        self.commit(|store, _seq, _now| {
            store.drop_collection(collection, ApplyMode::Live)?;
            Ok(Op::DropCollection {
                collection: collection.to_string(),
            })
        })?;
        Ok(())
    }

    /// Create a secondary index (builds it from the existing documents).
    pub fn create_index(&self, collection: &str, index: IndexMeta) -> Result<()> {
        self.commit(|store, _seq, _now| {
            store.create_index(collection, index.clone(), ApplyMode::Live)?;
            Ok(Op::CreateIndex {
                collection: collection.to_string(),
                index: index.clone(),
            })
        })?;
        Ok(())
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, collection: &str, name: &str) -> Result<()> {
        self.commit(|store, _seq, _now| {
            store.drop_index(collection, name, ApplyMode::Live)?;
            Ok(Op::DropIndex {
                collection: collection.to_string(),
                name: name.to_string(),
            })
        })?;
        Ok(())
    }

    // ------------------------------------------------------------- read path

    pub fn get(&self, collection: &str, key: &str) -> Result<Doc> {
        let state = self.read();
        self.metrics.gets.fetch_add(1, Ordering::Relaxed);
        match state.store.get(collection, key, now_ms())? {
            Some(doc) => Ok(doc.clone()),
            None => Err(DbError::NotFound {
                collection: collection.to_string(),
                key: key.to_string(),
            }),
        }
    }

    pub fn get_opt(&self, collection: &str, key: &str) -> Result<Option<Doc>> {
        let state = self.read();
        self.metrics.gets.fetch_add(1, Ordering::Relaxed);
        Ok(state.store.get(collection, key, now_ms())?.cloned())
    }

    pub fn list_collections(&self) -> Vec<CollectionMeta> {
        self.read()
            .store
            .collections
            .values()
            .map(|coll| coll.meta.clone())
            .collect()
    }

    pub fn collection_meta(&self, name: &str) -> Result<CollectionMeta> {
        Ok(self.read().store.collection(name)?.meta.clone())
    }

    /// Scan documents in key order.
    pub fn scan(&self, collection: &str, options: &ScanOptions) -> Result<Vec<(String, Doc)>> {
        let state = self.read();
        let now = now_ms();
        let coll = state.store.collection(collection)?;
        let mut items: Vec<(String, Doc)> = match &options.prefix {
            Some(prefix) => coll
                .docs
                .range(prefix.clone()..)
                .take_while(|(key, _)| key.starts_with(prefix))
                .filter(|(_, doc)| options.include_expired || !doc.is_expired(now))
                .map(|(key, doc)| (key.clone(), doc.clone()))
                .collect(),
            None => coll
                .docs
                .iter()
                .filter(|(_, doc)| options.include_expired || !doc.is_expired(now))
                .map(|(key, doc)| (key.clone(), doc.clone()))
                .collect(),
        };
        if options.reverse {
            items.reverse();
        }
        let limit = if options.limit == 0 {
            usize::MAX
        } else {
            options.limit
        };
        Ok(items.into_iter().skip(options.offset).take(limit).collect())
    }

    /// Look up documents by an exact secondary index value.
    pub fn find_by_index(
        &self,
        collection: &str,
        index: &str,
        value: &Json,
        limit: usize,
    ) -> Result<Vec<(String, Doc)>> {
        let state = self.read();
        let now = now_ms();
        let coll = state.store.collection(collection)?;
        let index_state = coll
            .indexes
            .get(index)
            .ok_or_else(|| DbError::IndexNotFound {
                collection: collection.to_string(),
                index: index.to_string(),
            })?;
        let mut out = Vec::new();
        if let Some(keys) = index_state.entries.get(&value.canonical_bytes()) {
            for key in keys {
                if let Some(doc) = coll.docs.get(key) {
                    if !doc.is_expired(now) {
                        out.push((key.clone(), doc.clone()));
                    }
                }
                if limit != 0 && out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Range scan on a secondary index (`from`/`to` inclusive, `None` = unbounded).
    pub fn range_by_index(
        &self,
        collection: &str,
        index: &str,
        from: Option<&Json>,
        to: Option<&Json>,
        limit: usize,
    ) -> Result<Vec<(String, Json, Doc)>> {
        let state = self.read();
        let now = now_ms();
        let coll = state.store.collection(collection)?;
        let index_state = coll
            .indexes
            .get(index)
            .ok_or_else(|| DbError::IndexNotFound {
                collection: collection.to_string(),
                index: index.to_string(),
            })?;
        let start = from.map(|v| v.canonical_bytes());
        let end = to.map(|v| v.canonical_bytes());
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &std::collections::BTreeSet<String>)>> =
            match (start, end) {
                (Some(start), Some(end)) => Box::new(index_state.entries.range(start..=end)),
                (Some(start), None) => Box::new(index_state.entries.range(start..)),
                (None, Some(end)) => Box::new(index_state.entries.range(..=end)),
                (None, None) => Box::new(index_state.entries.iter()),
            };
        let mut out = Vec::new();
        for (_, keys) in iter {
            for key in keys {
                if let Some(doc) = coll.docs.get(key) {
                    if doc.is_expired(now) {
                        continue;
                    }
                    let value = doc
                        .value
                        .get_path(&index_state.meta.field)
                        .cloned()
                        .unwrap_or(Json::Null);
                    out.push((key.clone(), value, doc.clone()));
                    if limit != 0 && out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Number of live documents in a collection.
    pub fn count(&self, collection: &str) -> Result<usize> {
        let state = self.read();
        let now = now_ms();
        Ok(state.store.collection(collection)?.live_len(now))
    }

    // ------------------------------------------------------------- TTL

    /// Remove expired documents, returning how many were removed. Each removal is
    /// logged as an explicit `ttl_expiry` batch, so it is auditable and never silent.
    pub fn sweep_expired(&self) -> Result<usize> {
        let expired = {
            let state = self.read();
            state.store.expired_keys(now_ms())
        };
        if expired.is_empty() {
            return Ok(0);
        }
        let ops: Vec<Op> = expired
            .iter()
            .map(|(collection, key)| Op::Delete {
                collection: collection.clone(),
                key: key.clone(),
            })
            .take(self.cfg.limits.max_batch_ops)
            .collect();
        let removed = ops.len();
        self.commit(|store, seq, now| {
            let op = Op::Batch {
                ops: ops.clone(),
                label: "ttl_expiry".to_string(),
            };
            store.apply(&op, now, seq, ApplyMode::Live)?;
            Ok(op)
        })?;
        self.metrics.sweeps.fetch_add(1, Ordering::Relaxed);
        Ok(removed)
    }

    // ------------------------------------------------------------- checkpoint

    /// Write a snapshot of the current state and retire the log segments it covers.
    pub fn checkpoint(&self) -> Result<WrittenSnapshot> {
        if self.cfg.read_only {
            return Err(DbError::ReadOnly("checkpoint".to_string()));
        }
        let mut state = self.write();
        let mut writer = self.lock_mutex(&self.writer);
        let mut manifest = self.lock_mutex(&self.manifest);

        // 1. Rotate so the snapshot can never overlap the segment we keep writing to.
        writer.rotate()?;
        let keep_index = writer.current_index();
        let generation = state.generation + 1;
        let seq = state.seq;

        // 2. Serialise the state.
        let payload: Vec<(CollectionMeta, Vec<(String, Doc)>)> = state
            .store
            .collections
            .values()
            .map(|coll| {
                (
                    coll.meta.clone(),
                    coll.docs
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            })
            .collect();
        let snapshot_path = self.paths.snapshot_file(generation);
        let written =
            snapshot::write_snapshot(&snapshot_path, generation, seq, now_ms(), &payload)?;

        // 3. Make the new snapshot authoritative.
        let previous_snapshot = manifest.snapshot.clone();
        manifest.generation = generation;
        manifest.last_seq = seq;
        manifest.snapshot = Some(SnapshotRef {
            file: snapshot_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            seq,
            generation,
            sha256: written.body_sha256.clone(),
            doc_count: written.header.doc_count,
            created_at: written.header.created_at,
            bytes: written.bytes,
        });
        manifest.wal = vec![wal::segment_name(keep_index)];
        manifest.save(&self.paths.root)?;
        state.generation = generation;

        // 4. Only now may the retired segments disappear.
        let retired = wal::list_segments(&self.paths.wal_dir)?;
        for segment in retired {
            let index = segment
                .file_name()
                .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
                .unwrap_or(u64::MAX);
            if index < keep_index {
                let _ = std::fs::remove_file(&segment);
            }
        }
        if let Some(previous) = previous_snapshot {
            if previous.file
                != manifest
                    .snapshot
                    .as_ref()
                    .map(|s| s.file.clone())
                    .unwrap_or_default()
            {
                let _ = std::fs::remove_file(self.paths.snap_dir.join(&previous.file));
            }
        }
        self.mutations_since_checkpoint.store(0, Ordering::Relaxed);
        self.metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
        Ok(written)
    }

    /// Sweep expired documents, checkpoint and clean up. Data is never dropped
    /// silently: the number of swept documents is returned in the report.
    pub fn compact(&self) -> Result<CompactReport> {
        let bytes_before = self.disk_usage();
        let expired_swept = self.sweep_expired()?;
        let before_segments = wal::list_segments(&self.paths.wal_dir)?.len();
        let snapshot = self.checkpoint()?;
        let after_segments = wal::list_segments(&self.paths.wal_dir)?.len();
        Ok(CompactReport {
            expired_swept,
            snapshot,
            segments_removed: before_segments.saturating_sub(after_segments),
            bytes_before,
            bytes_after: self.disk_usage(),
        })
    }

    // ------------------------------------------------------------- stats/verify

    /// Total bytes used by the database directory.
    pub fn disk_usage(&self) -> u64 {
        fn walk(path: &Path) -> u64 {
            let mut total = 0;
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    match entry.metadata() {
                        Ok(meta) if meta.is_dir() => total += walk(&entry.path()),
                        Ok(meta) => total += meta.len(),
                        Err(_) => {}
                    }
                }
            }
            total
        }
        walk(&self.paths.root)
    }

    pub fn stats(&self) -> EngineStats {
        let state = self.read();
        let now = now_ms();
        let collections = state
            .store
            .collections
            .values()
            .map(|coll| CollectionStats {
                name: coll.meta.name.clone(),
                documents: coll.docs.len(),
                live_documents: coll.live_len(now),
                deleted_pending: coll.docs.len() - coll.live_len(now),
                indexes: coll.meta.indexes.keys().cloned().collect(),
                has_schema: coll.meta.schema.is_some(),
                max_docs: coll.meta.max_docs,
            })
            .collect::<Vec<_>>();
        let total_documents = state.store.total_docs();
        let segments = wal::list_segments(&self.paths.wal_dir).unwrap_or_default();
        let wal_bytes: u64 = segments
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let manifest = self.lock_mutex(&self.manifest).clone();
        let snapshot_bytes = manifest
            .snapshot
            .as_ref()
            .map(|snap| self.paths.snap_dir.join(&snap.file))
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|meta| meta.len())
            .unwrap_or(0);
        let metrics = Json::obj()
            .with(
                "gets",
                Json::uint(self.metrics.gets.load(Ordering::Relaxed)),
            )
            .with(
                "puts",
                Json::uint(self.metrics.puts.load(Ordering::Relaxed)),
            )
            .with(
                "deletes",
                Json::uint(self.metrics.deletes.load(Ordering::Relaxed)),
            )
            .with(
                "batches",
                Json::uint(self.metrics.batches.load(Ordering::Relaxed)),
            )
            .with(
                "transactions",
                Json::uint(self.metrics.transactions.load(Ordering::Relaxed)),
            )
            .with(
                "conflicts",
                Json::uint(self.metrics.conflicts.load(Ordering::Relaxed)),
            )
            .with(
                "sweeps",
                Json::uint(self.metrics.sweeps.load(Ordering::Relaxed)),
            )
            .with(
                "checkpoints",
                Json::uint(self.metrics.checkpoints.load(Ordering::Relaxed)),
            )
            .with(
                "wal_bytes_written",
                Json::uint(self.metrics.wal_bytes.load(Ordering::Relaxed)),
            );
        EngineStats {
            path: self.paths.root.display().to_string(),
            last_seq: state.seq,
            generation: state.generation,
            sync_mode: self.cfg.sync_mode.as_str().to_string(),
            read_only: self.cfg.read_only,
            collections,
            total_documents,
            wal_segments: segments.len(),
            wal_bytes,
            snapshot_bytes,
            disk_bytes: self.disk_usage(),
            snapshot_seq: manifest.snapshot.as_ref().map(|snap| snap.seq),
            uptime_ms: (now - self.started_at_ms).max(0) as u64,
            metrics,
            recovery_notes: self.recovery_notes(),
        }
    }

    /// Verify all on-disk structures without modifying anything.
    pub fn verify(&self) -> Result<VerifyReport> {
        let mut issues = Vec::new();
        let manifest = self.lock_mutex(&self.manifest).clone();
        let mut snapshot_json = None;
        if let Some(snap_ref) = &manifest.snapshot {
            let path = self.paths.snap_dir.join(&snap_ref.file);
            match snapshot::read_snapshot(&path) {
                Ok(data) => {
                    let mut ok = true;
                    if !snap_ref.sha256.is_empty() && data.body_sha256 != snap_ref.sha256 {
                        ok = false;
                        issues.push(format!(
                            "snapshot {} SHA-256 mismatch with manifest",
                            snap_ref.file
                        ));
                    }
                    if data.header.seq != snap_ref.seq {
                        ok = false;
                        issues.push(format!(
                            "snapshot {} records seq {} but manifest says {}",
                            snap_ref.file, data.header.seq, snap_ref.seq
                        ));
                    }
                    snapshot_json = Some(
                        Json::obj()
                            .with("file", Json::str(snap_ref.file.clone()))
                            .with("ok", Json::Bool(ok))
                            .with("seq", Json::uint(data.header.seq))
                            .with("documents", Json::uint(data.header.doc_count))
                            .with("sha256", Json::str(data.body_sha256)),
                    );
                }
                Err(err) => {
                    issues.push(format!("snapshot {} is unreadable: {err}", snap_ref.file));
                    snapshot_json = Some(
                        Json::obj()
                            .with("file", Json::str(snap_ref.file.clone()))
                            .with("ok", Json::Bool(false))
                            .with("error", Json::str(err.to_string())),
                    );
                }
            }
        }

        let mut segments = Vec::new();
        let mut previous_seq = 0u64;
        for segment in wal::list_segments(&self.paths.wal_dir)? {
            let scan = wal::scan_segment(&segment)?;
            let name = segment
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let ok = scan.corruption.is_none() && scan.torn_tail.is_none();
            if let Some(corruption) = &scan.corruption {
                issues.push(format!("{name}: {corruption}"));
            }
            if let Some(torn) = &scan.torn_tail {
                issues.push(format!("{name}: torn tail ({torn})"));
            }
            for record in &scan.records {
                if record.seq <= previous_seq {
                    issues.push(format!(
                        "{name}: record sequence {} is not larger than the previous record ({})",
                        record.seq, previous_seq
                    ));
                }
                previous_seq = record.seq;
            }
            segments.push(
                Json::obj()
                    .with("file", Json::str(name))
                    .with("ok", Json::Bool(ok))
                    .with("records", Json::uint(scan.records.len() as u64))
                    .with("valid_bytes", Json::uint(scan.valid_len))
                    .with(
                        "torn_tail",
                        scan.torn_tail.clone().map(Json::str).unwrap_or(Json::Null),
                    )
                    .with(
                        "corruption",
                        scan.corruption.clone().map(Json::str).unwrap_or(Json::Null),
                    ),
            );
        }
        Ok(VerifyReport {
            ok: issues.is_empty(),
            snapshot: snapshot_json,
            segments,
            issues,
        })
    }

    /// A consistent, self-contained copy of the data (used by backups and the API).
    pub fn export_snapshot(&self) -> Json {
        let state = self.read();
        let now = now_ms();
        let mut collections = Json::obj();
        for (name, coll) in &state.store.collections {
            let mut docs = Json::obj();
            for (key, doc) in &coll.docs {
                if doc.is_expired(now) {
                    continue;
                }
                docs.set(key.clone(), doc.to_json());
            }
            collections.set(
                name.clone(),
                Json::obj()
                    .with("meta", coll.meta.to_json())
                    .with("documents", docs),
            );
        }
        Json::obj()
            .with("revision", Json::uint(state.seq))
            .with("generation", Json::uint(state.generation))
            .with("collections", collections)
    }

    /// Current log sequence number.
    pub fn last_seq(&self) -> u64 {
        self.read().seq
    }

    /// Flush the WAL to disk.
    pub fn flush(&self) -> Result<()> {
        if self.cfg.read_only {
            return Ok(());
        }
        self.lock_mutex(&self.writer).sync()
    }

    /// Graceful shutdown: flush, checkpoint (optional) and release the lock.
    pub fn shutdown(&self) -> Result<()> {
        if !self.cfg.read_only {
            self.flush()?;
        }
        let mut lock = self.lock_mutex(&self.lock);
        if let Some(lock) = lock.as_mut() {
            lock.release();
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.flush();
        if let Some(lock) = self
            .lock
            .get_mut()
            .unwrap_or_else(|err| err.into_inner())
            .as_mut()
        {
            lock.release();
        }
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("path", &self.paths.root)
            .field("last_seq", &self.read().seq)
            .field("read_only", &self.cfg.read_only)
            .finish()
    }
}

/// Convenience: build a `Store` view from a snapshot file (used by `restore --verify`).
pub fn load_snapshot_store(path: &Path, limits: Limits) -> Result<Store> {
    let data = snapshot::read_snapshot(path)?;
    let mut store = Store::new(limits);
    for (meta, docs) in data.collections {
        let name = meta.name.clone();
        store.create_collection(meta, ApplyMode::Replay)?;
        if let Some(coll) = store.collections.get_mut(&name) {
            for (key, doc) in docs {
                coll.docs.insert(key, doc);
            }
            coll.rebuild_indexes()?;
        }
    }
    Ok(store)
}

/// Helper for tests and tooling: the collection view without cloning the whole store.
pub fn collection_ref<'a>(store: &'a Store, name: &str) -> Result<&'a Collection> {
    store.collection(name)
}
