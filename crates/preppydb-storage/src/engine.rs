//! The storage engine: recovery, write path, checkpoints, transactions, stats.
//!
//! ## Durability model
//!
//! * Every mutation is appended to the write-ahead log as one framed record.
//!   Multi-document batches and transactions are a *single* record, so they are
//!   atomic even across a crash.
//! * `SyncMode::Always` (default) calls `sync_data` before a successful commit returns.
//!   An I/O error after a write reaches the WAL leaves the outcome unknown and requires
//!   reopen; flush success is not a universal power-loss guarantee.
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
use crate::store::{reserve_undo_bytes, ApplyMode, Collection, Doc, Limits, Store, StoreUndo};
use crate::verify_store::VerifyStore;
use crate::wal::{self, SyncMode, WalWriter};
use preppydb_crypto::{hex_encode, sha256::Sha256};
use preppydb_json::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type IndexEntryIter<'a> =
    dyn Iterator<Item = (&'a Vec<u8>, &'a std::collections::BTreeSet<Arc<str>>)> + 'a;

fn hash_len_bytes(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn inventory_record_digest(collection: &str, key: &str, document: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"PREPPYDB-INVENTORY-RECORD-V1\0");
    hash_len_bytes(&mut hash, collection.as_bytes());
    hash_len_bytes(&mut hash, key.as_bytes());
    hash_len_bytes(&mut hash, document);
    hash.finish()
}

enum VerifyReplayStore {
    Compact(VerifyStore),
    Full(Store),
}

impl VerifyReplayStore {
    fn apply(&mut self, op: &Op, ts: i64, seq: u64) -> Result<()> {
        match self {
            Self::Compact(store) => store.apply(op),
            Self::Full(store) => store.apply_for_verification(op, ts, seq).map(|_| ()),
        }
    }
}

struct VerifySnapshotData {
    header: snapshot::SnapshotHeader,
    body_sha256: String,
    store: VerifyReplayStore,
}

const MAX_VERIFY_INDEX_FIELDS: usize = snapshot::MAX_VERIFY_INDEX_FIELDS;
const MAX_VERIFY_INDEX_FIELD_BYTES: usize = snapshot::MAX_VERIFY_INDEX_FIELD_BYTES;

fn prefix_upper_bound(prefix: &str) -> Option<String> {
    for (index, ch) in prefix.char_indices().rev() {
        let codepoint = ch as u32;
        let next = match codepoint {
            0xD7FF => 0xE000,
            0x10FFFF => continue,
            value => value + 1,
        };
        let Some(next) = char::from_u32(next) else {
            continue;
        };
        let mut upper = String::with_capacity(index + next.len_utf8());
        upper.push_str(&prefix[..index]);
        upper.push(next);
        return Some(upper);
    }
    None
}

fn add_verify_index_field(
    fields: &mut BTreeMap<String, BTreeSet<String>>,
    total_bytes: &mut usize,
    total_count: &mut usize,
    collection: &str,
    field: &str,
) -> bool {
    if fields
        .get(collection)
        .is_some_and(|collection_fields| collection_fields.contains(field))
    {
        return true;
    }
    let Some(next_bytes) = total_bytes.checked_add(field.len()) else {
        return false;
    };
    let Some(next_count) = total_count.checked_add(1) else {
        return false;
    };
    if next_count > MAX_VERIFY_INDEX_FIELDS || next_bytes > MAX_VERIFY_INDEX_FIELD_BYTES {
        return false;
    }
    fields
        .entry(collection.to_string())
        .or_default()
        .insert(field.to_string());
    *total_bytes = next_bytes;
    *total_count = next_count;
    true
}

fn add_verify_index_meta(
    fields: &mut BTreeMap<String, BTreeSet<String>>,
    total_bytes: &mut usize,
    total_count: &mut usize,
    collection: &str,
    index: &IndexMeta,
) -> bool {
    add_verify_index_field(fields, total_bytes, total_count, collection, &index.field)
}

fn collect_verify_index_fields_from_op(
    op: &Op,
    fields: &mut BTreeMap<String, BTreeSet<String>>,
    total_bytes: &mut usize,
    total_count: &mut usize,
) -> bool {
    match op {
        Op::CreateCollection { meta } => {
            meta.indexes
                .values()
                .filter(|index| index.unique)
                .all(|index| {
                    add_verify_index_meta(fields, total_bytes, total_count, &meta.name, index)
                })
        }
        Op::CreateIndex { collection, index } if index.unique => {
            add_verify_index_meta(fields, total_bytes, total_count, collection, index)
        }
        Op::CreateIndex { .. } => true,
        Op::Batch { ops, .. } => ops
            .iter()
            .all(|op| collect_verify_index_fields_from_op(op, fields, total_bytes, total_count)),
        _ => true,
    }
}

/// Collect every index field used by the retained WAL so verification can replay
/// documents without retaining unrelated payload fields. If metadata exceeds the
/// explicit budget, callers use the established full-store verifier instead.
fn collect_verify_index_fields(
    segments: &[PathBuf],
) -> Result<Option<BTreeMap<String, BTreeSet<String>>>> {
    let mut fields = BTreeMap::new();
    let mut total_bytes = 0usize;
    let mut total_count = 0usize;
    for segment in segments {
        let scan = wal::scan_segment_visit(segment, |record| {
            if !collect_verify_index_fields_from_op(
                &record.op,
                &mut fields,
                &mut total_bytes,
                &mut total_count,
            ) {
                return Err(DbError::LimitExceeded {
                    limit: "verify_index_field_plan",
                    max: MAX_VERIFY_INDEX_FIELD_BYTES as u64,
                    actual: u64::try_from(total_bytes.saturating_add(1)).unwrap_or(u64::MAX),
                });
            }
            Ok(())
        });
        match scan {
            Ok(summary) if summary.corruption.is_none() && summary.torn_tail.is_none() => {}
            Ok(_) => return Ok(None),
            Err(DbError::LimitExceeded {
                limit: "verify_index_field_plan",
                ..
            }) => return Ok(None),
            Err(err) => return Err(err),
        }
    }
    Ok(Some(fields))
}

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
    /// Open read-only (no WAL writes or checkpoints). When `take_lock` is true,
    /// acquire a shared directory lock if the database has a lock file.
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

/// Document-native filter semantics for the experimental query boundary.
#[derive(Clone, Debug)]
pub struct DocumentFilter {
    pub field: String,
    pub operator: String,
    pub value: Json,
}

#[derive(Clone, Debug)]
pub enum DocumentAccess {
    FullScan,
    IndexEquality(String),
}

fn matches_document_filter(doc: &Doc, filter: Option<&DocumentFilter>) -> bool {
    let Some(filter) = filter else { return true };
    let Some(value) = doc.value.get_path(&filter.field) else {
        return false;
    };
    match (&filter.operator[..], value, &filter.value) {
        ("eq", left, right) if left.kind() == right.kind() => left == right,
        ("gte", Json::Number(left), Json::Number(right)) => left >= right,
        ("lte", Json::Number(left), Json::Number(right)) => left <= right,
        ("gte", Json::String(left), Json::String(right)) => left >= right,
        ("lte", Json::String(left), Json::String(right)) => left <= right,
        _ => false,
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

/// Independently computed inventory of the current persisted logical view.
/// Expired documents are included because they remain physically stored until swept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryManifest {
    pub snapshot_id: String,
    pub sequence: u64,
    pub generation: u64,
    pub collection_count: u64,
    pub document_count: u64,
    pub serialized_bytes: u64,
    pub sha256: String,
    pub collections: Vec<InventoryCollection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryCollection {
    pub name: String,
    pub document_count: u64,
    pub metadata_bytes: u64,
    pub document_bytes: u64,
    pub metadata_sha256: String,
    pub documents_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryRecord {
    pub key: String,
    pub serialized_bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryPage {
    pub sequence: u64,
    pub generation: u64,
    pub collection: String,
    pub records: Vec<InventoryRecord>,
    pub next_cursor: Option<String>,
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

fn require_existing_directory(path: &Path, optional: bool) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(DbError::corrupt(
            path.display().to_string(),
            "database storage path is not a real directory",
        )),
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DbError::io(
            format!("opening existing database directory {}", path.display()),
            error,
        )),
    }
}

/// The PreppyDB storage engine.
pub struct Engine {
    cfg: EngineConfig,
    paths: DataPaths,
    state: RwLock<State>,
    writer: Arc<Mutex<WalWriter>>,
    manifest: Mutex<Manifest>,
    lock: Mutex<Option<FileLock>>,
    notes: Mutex<Vec<RecoveryNote>>,
    poisoned: Arc<AtomicBool>,
    closed: AtomicBool,
    interval_sync_worker: Mutex<Option<IntervalSyncWorker>>,
    mutations_since_checkpoint: AtomicU64,
    pub metrics: Metrics,
    started_at_ms: i64,
}

/// Periodically syncs interval-mode WAL writes without adding work to each commit.
/// A failed background sync poisons the engine because previously acknowledged writes
/// may no longer have a known durability outcome.
struct IntervalSyncWorker {
    stop: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl IntervalSyncWorker {
    fn start(
        writer: Arc<Mutex<WalWriter>>,
        poisoned: Arc<AtomicBool>,
        interval: Duration,
    ) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("preppydb-wal-sync".into())
            .spawn(move || loop {
                match receiver.recv_timeout(interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let mut writer = writer.lock().unwrap_or_else(|err| err.into_inner());
                        if writer.has_unsynced_frames() && writer.sync().is_err() {
                            poisoned.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })
            .map_err(|err| DbError::io("starting WAL interval sync worker", err))?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    fn stop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for IntervalSyncWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn next_sequence(current: u64) -> Result<u64> {
    current.checked_add(1).ok_or(DbError::LimitExceeded {
        limit: "wal_sequence",
        max: u64::MAX,
        actual: u64::MAX,
    })
}

fn validate_snapshot_reference(manifest: &Manifest, reference: &SnapshotRef) -> Result<()> {
    let expected_file = format!("snap-{:012}.pdb", reference.generation);
    if reference.generation == 0
        || reference.file != expected_file
        || manifest.generation != reference.generation
        || manifest.last_seq < reference.seq
    {
        return Err(DbError::corrupt(
            "manifest",
            "snapshot filename, generation, or sequence metadata is inconsistent",
        ));
    }
    Ok(())
}

fn validate_snapshot_contents(
    reference: &SnapshotRef,
    header: &snapshot::SnapshotHeader,
    path: &Path,
) -> Result<()> {
    if header.generation != reference.generation
        || header.seq != reference.seq
        || header.doc_count != reference.doc_count
    {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "snapshot header does not match the manifest reference",
        ));
    }
    if reference.bytes != 0
        && std::fs::metadata(path)
            .map_err(|err| DbError::io("reading snapshot metadata", err))?
            .len()
            != reference.bytes
    {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "snapshot length does not match the manifest reference",
        ));
    }
    Ok(())
}

fn ensure_regular_snapshot_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| DbError::io(format!("reading snapshot metadata {}", path.display()), err))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DbError::corrupt(
            path.display().to_string(),
            "snapshot reference is not a regular file",
        ));
    }
    Ok(())
}

impl Engine {
    /// Open (or create) a database directory, replaying the log as needed.
    pub fn open(cfg: EngineConfig) -> Result<Engine> {
        if cfg.sync_mode == SyncMode::Interval(0) {
            return Err(DbError::Config(
                "sync interval must be greater than zero milliseconds".into(),
            ));
        }
        crate::backup::ensure_no_restore_intent(&cfg.path)?;
        let paths = DataPaths::new(cfg.path.clone());
        if cfg.read_only {
            require_existing_directory(&paths.root, false)?;
        } else {
            create_private_dir(&paths.root)?;
        }

        let lock = if cfg.take_lock {
            if cfg.read_only {
                FileLock::acquire_shared(&paths.root)?
            } else {
                Some(FileLock::acquire(&paths.root)?)
            }
        } else {
            None
        };
        // Restore may publish its durable intent after the first check while this open
        // creates the directory. Recheck after taking the directory lock so a writer
        // cannot race publication or initialize an empty replacement path.
        crate::backup::ensure_no_restore_intent(&cfg.path)?;

        // Take the target lock before creating child storage directories. Restore
        // reserves a previously absent target by creating and locking the root, then
        // publishes its durable intent before releasing that lock. Creating `wal/`
        // or `snap/` before acquiring the lock would let an opener that passed the
        // first intent check modify that reservation while restore is publishing.
        if cfg.read_only {
            require_existing_directory(&paths.wal_dir, false)?;
            require_existing_directory(&paths.snap_dir, true)?;
        } else {
            create_private_dir(&paths.wal_dir)?;
            create_private_dir(&paths.snap_dir)?;
        }

        let mut manifest = match Manifest::load(&paths.root)? {
            Some(manifest) => manifest,
            None => {
                // A missing manifest is normal for a new database, but never evidence
                // that previously retired snapshot/WAL history can be discarded.
                let has_snapshots = match std::fs::read_dir(&paths.snap_dir) {
                    Ok(mut entries) => entries
                        .next()
                        .transpose()
                        .map_err(|err| DbError::io("reading snapshot entry", err))?
                        .is_some(),
                    Err(error) if cfg.read_only && error.kind() == std::io::ErrorKind::NotFound => {
                        false
                    }
                    Err(error) => return Err(DbError::io("listing snapshots", error)),
                };
                if has_snapshots {
                    return Err(DbError::corrupt(
                        "manifest",
                        "manifest missing while snapshot files exist; refusing to initialize an empty database",
                    ));
                }
                let segments = wal::list_segments(&paths.wal_dir)?;
                let first_segment = segments
                    .first()
                    .and_then(|path| path.file_name())
                    .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()));
                if first_segment.is_some_and(|index| index > 1) {
                    return Err(DbError::corrupt(
                        "manifest",
                        "manifest missing after WAL history was retired; refusing incomplete recovery",
                    ));
                }
                Manifest::default()
            }
        };
        let mut notes: Vec<RecoveryNote> = Vec::new();
        let mut store = Store::new(cfg.limits.clone());
        let mut seq: u64 = 0;

        // ---- 1. load the newest snapshot referenced by the manifest --------------
        if let Some(snap_ref) = manifest.snapshot.clone() {
            // Snapshot paths are untrusted manifest data. Check the complete
            // generation-derived name before joining it to the snapshot directory.
            // This also prevents an otherwise valid snapshot outside `snap/` from
            // being substituted during recovery.
            validate_snapshot_reference(&manifest, &snap_ref)?;
            let path = paths.snap_dir.join(&snap_ref.file);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    ensure_regular_snapshot_file(&path)?;
                    let data = snapshot::read_snapshot_store(&path, cfg.limits.clone())?;
                    validate_snapshot_contents(&snap_ref, &data.header, &path)?;
                    if !snap_ref.sha256.is_empty() && data.body_sha256 != snap_ref.sha256 {
                        return Err(DbError::corrupt(
                            path.display().to_string(),
                            "snapshot SHA-256 does not match the manifest",
                        ));
                    }
                    store = data.store;
                    seq = data.header.seq;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(DbError::corrupt(
                        "manifest",
                        format!(
                            "referenced snapshot '{}' is missing; WAL may not contain its history",
                            snap_ref.file
                        ),
                    ));
                }
                Err(err) => {
                    return Err(DbError::io("reading snapshot metadata", err));
                }
            }
        }

        // ---- 2. replay every WAL segment found on disk --------------------------
        let segments = wal::list_segments(&paths.wal_dir)?;
        let mut last_segment_index = 0u64;
        let mut replayed_records: u64 = 0;
        let snapshot_seq = seq;
        let mut replay_started = false;
        'segments: for (segment_position, segment) in segments.iter().enumerate() {
            let index = segment
                .file_name()
                .and_then(|name| wal::parse_segment_name(&name.to_string_lossy()))
                .unwrap_or(0);
            let mut sequence_corruption = None;
            let mut scan = wal::scan_segment_visit_with_offset(segment, |record, frame_offset| {
                if sequence_corruption.is_some() {
                    return Ok(());
                }
                if record.seq <= snapshot_seq {
                    if replay_started {
                        sequence_corruption = Some((
                            frame_offset,
                            format!(
                                "record sequence {} regresses after replay reached {}",
                                record.seq, seq
                            ),
                        ));
                    }
                    return Ok(());
                }
                let expected_seq = seq.checked_add(1).ok_or_else(|| {
                    DbError::corrupt("write-ahead log", "record sequence overflow")
                })?;
                if record.seq != expected_seq {
                    sequence_corruption = Some((
                        frame_offset,
                        format!(
                            "record sequence {} does not continue from {}; refusing incomplete recovery",
                            record.seq, seq
                        ),
                    ));
                    return Ok(());
                }
                store.apply(&record.op, record.ts, record.seq, ApplyMode::Replay)?;
                seq = record.seq;
                replay_started = true;
                replayed_records += 1;
                Ok(())
            })?;
            if let Some((offset, detail)) = sequence_corruption {
                scan.valid_len = offset;
                scan.corruption = Some(detail);
                scan.torn_tail = None;
            }
            if scan.torn_tail.is_some() && segment_position + 1 < segments.len() {
                scan.corruption = Some(
                    "torn WAL tail is followed by another segment; refusing ambiguous recovery"
                        .into(),
                );
                scan.torn_tail = None;
            }
            if scan.torn_tail.is_some() && scan.valid_len == 0 && scan.corruption.is_none() {
                // A segment shorter than its header cannot contain a frame. If it is
                // the final contiguous segment, it represents an interrupted segment
                // creation, not a committed record. Do not try to append to that file.
                let expected_index = if last_segment_index > 0 {
                    last_segment_index.checked_add(1)
                } else if manifest.snapshot.is_some() {
                    Some(index)
                } else {
                    Some(1)
                };
                if expected_index != Some(index) {
                    scan.corruption = Some(format!(
                        "incomplete final segment index {index} does not continue the persisted WAL history"
                    ));
                    scan.torn_tail = None;
                } else if cfg.read_only {
                    return Err(DbError::corrupt(
                        "write-ahead log",
                        format!(
                            "{} has an incomplete final segment header; read-only open will not remove it",
                            segment.display()
                        ),
                    ));
                } else {
                    std::fs::remove_file(segment).map_err(|err| {
                        DbError::io("removing incomplete WAL segment header", err)
                    })?;
                    crate::snapshot::fsync_dir(&paths.wal_dir)?;
                    notes.push(RecoveryNote {
                        at: now_ms(),
                        kind: "torn_segment_header".into(),
                        detail: format!(
                            "{}: removed incomplete final segment header; it contained no record frames",
                            segment.display()
                        ),
                    });
                    break 'segments;
                }
            }
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
                last_segment_index = index;
                break 'segments;
            }
            if let Some(corruption) = &scan.corruption {
                let detail = format!("{}: {corruption}", segment.display());
                if cfg.repair == RepairPolicy::Strict || cfg.read_only {
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
                repair_corrupt_wal_suffix(
                    segment,
                    &segments[segment_position + 1..],
                    scan.valid_len,
                )?;
                last_segment_index = index;
                break 'segments;
            }
            last_segment_index = index;
        }

        if manifest.last_seq > seq {
            return Err(DbError::corrupt(
                "manifest",
                format!(
                    "manifest last_seq {} is ahead of the recovered sequence {}; WAL history is incomplete",
                    manifest.last_seq, seq
                ),
            ));
        }

        // ---- 3. open the WAL for appending -------------------------------------
        let start_index = if last_segment_index == 0 {
            1
        } else {
            last_segment_index
        };
        if cfg.read_only && last_segment_index == 0 {
            return Err(DbError::corrupt(
                "write-ahead log",
                "read-only open requires an existing WAL segment",
            ));
        }
        let mut writer = if cfg.read_only {
            WalWriter::open_read_only(
                &paths.wal_dir,
                start_index,
                cfg.max_segment_bytes,
                cfg.sync_mode,
            )?
        } else {
            WalWriter::open(
                &paths.wal_dir,
                start_index,
                cfg.max_segment_bytes,
                cfg.sync_mode,
            )?
        };
        if !cfg.read_only && writer.current_len() > cfg.max_segment_bytes {
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

        let writer = Arc::new(Mutex::new(writer));
        let poisoned = Arc::new(AtomicBool::new(false));
        let interval_sync_worker = match cfg.sync_mode {
            SyncMode::Interval(milliseconds) if !cfg.read_only => Some(IntervalSyncWorker::start(
                Arc::clone(&writer),
                Arc::clone(&poisoned),
                Duration::from_millis(milliseconds),
            )?),
            _ => None,
        };
        let generation = manifest.generation;
        Ok(Engine {
            cfg,
            paths,
            state: RwLock::new(State {
                store,
                seq,
                generation,
            }),
            writer,
            manifest: Mutex::new(manifest),
            lock: Mutex::new(lock),
            notes: Mutex::new(notes),
            poisoned,
            closed: AtomicBool::new(false),
            interval_sync_worker: Mutex::new(interval_sync_worker),
            mutations_since_checkpoint: AtomicU64::new(0),
            metrics: Metrics::default(),
            started_at_ms: now_ms(),
        })
    }

    /// The durability policy this engine was opened with.
    ///
    /// Only [`SyncMode::Always`] means "a write that returned success has been handed to
    /// the device with `fsync`". `Interval` and `Never` acknowledge before that, so a
    /// caller that needs durability (a ledger, for example) must refuse to run on them.
    pub fn sync_mode(&self) -> SyncMode {
        self.cfg.sync_mode
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
        self.check_healthy()
    }

    fn check_healthy(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::Internal("engine is shut down".into()));
        }
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(DbError::CommitOutcomeUnknown(
                "engine requires reopen after a WAL write error; the last commit outcome may be unknown"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn sync_wal(&self) -> Result<()> {
        if let Err(err) = self.lock_mutex(&self.writer).sync() {
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
        }
        self.check_healthy()
    }

    fn stop_interval_sync_worker(&self) {
        let worker = self.lock_mutex(&self.interval_sync_worker).take();
        drop(worker);
    }

    // ------------------------------------------------------------- write path

    /// Atomically apply a batch that contains schema operations.
    ///
    /// Document-only batches use `commit_document_batch`. Schema batches retain only
    /// per-document undo values; collections and indexes that are removed are moved
    /// into the journal. Creating an index still costs O(collection size) by design.
    fn commit_schema_batch(&self, ops: Vec<Op>, label: &str) -> Result<u64> {
        self.check_writable("batch write")?;
        let mut state = self.write();
        self.check_writable("batch write")?;
        let seq = next_sequence(state.seq)?;
        let now = now_ms();

        let mut undo = Vec::with_capacity(ops.len());
        let mut undo_bytes = 0;

        let mut pending = ops.iter().rev().collect::<Vec<_>>();
        while let Some(op) = pending.pop() {
            if let Op::Batch { ops, .. } = op {
                pending.extend(ops.iter().rev());
                continue;
            }
            match state
                .store
                .apply_with_undo(op, now, seq, ApplyMode::Live, &mut undo_bytes)
            {
                Ok((_, inverse)) => undo.push(inverse),
                Err(err) => {
                    if let Err(rollback) = state.store.rollback(StoreUndo::Sequence(undo)) {
                        self.poisoned.store(true, Ordering::SeqCst);
                        return Err(DbError::Internal(format!(
                            "schema batch rollback failed after apply error: {rollback}"
                        )));
                    }
                    return Err(err);
                }
            }
        }

        let op = Op::Batch {
            ops,
            label: label.to_string(),
        };
        let record = Record::new(seq, now, op);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            if let Err(rollback) = state.store.rollback(StoreUndo::Sequence(undo)) {
                return Err(DbError::Internal(format!(
                    "schema batch rollback failed after WAL error: {rollback}"
                )));
            }
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
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

    /// Mutate one document under the write lock and restore it if WAL append fails.
    /// The displaced document is moved into the undo value rather than cloned.
    fn commit_document<F>(&self, collection: &str, key: &str, mutate: F) -> Result<u64>
    where
        F: FnOnce(&mut Store, u64, i64) -> Result<(Op, Option<Doc>)>,
    {
        self.check_writable("write")?;
        let mut state = self.write();
        self.check_writable("write")?;
        let seq = next_sequence(state.seq)?;
        let now = now_ms();
        let (op, previous) = mutate(&mut state.store, seq, now)?;
        let record = Record::new(seq, now, op);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            state.store.restore_document(collection, key, previous)?;
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
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

    fn commit_schema<P, F, S, U>(&self, prepare_undo: P, mutate: F, undo: U) -> Result<u64>
    where
        P: FnOnce(&Store) -> Result<S>,
        F: FnOnce(&mut Store, u64, i64) -> Result<Op>,
        U: FnOnce(&mut Store, S) -> Result<()>,
    {
        self.check_writable("schema write")?;
        let mut state = self.write();
        self.check_writable("schema write")?;
        let undo_state = prepare_undo(&state.store)?;
        let seq = next_sequence(state.seq)?;
        let now = now_ms();
        let record = Record::new(seq, now, mutate(&mut state.store, seq, now)?);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            if let Err(rollback) = undo(&mut state.store, undo_state) {
                return Err(DbError::Internal(format!(
                    "schema rollback failed after WAL error: {rollback}"
                )));
            }
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
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

    /// Insert or replace a document.
    pub fn put(
        &self,
        collection: &str,
        key: &str,
        value: Json,
        options: PutOptions,
    ) -> Result<u64> {
        let version = std::cell::Cell::new(0u64);
        self.commit_document(collection, key, |store, seq, now| {
            let (written, previous) = store.put_with_undo(
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
            Ok((
                Op::Put {
                    collection: collection.to_string(),
                    key: key.to_string(),
                    value,
                    expires_at: options.expires_at,
                },
                previous,
            ))
        })?;
        self.metrics.puts.fetch_add(1, Ordering::Relaxed);
        Ok(version.get())
    }

    /// Delete a document. `expected_version` implements compare-and-delete.
    pub fn delete(&self, collection: &str, key: &str, expected_version: Option<u64>) -> Result<()> {
        self.commit_document(collection, key, |store, _seq, _now| {
            let (_, previous) =
                store.delete_with_undo(collection, key, expected_version, ApplyMode::Live)?;
            Ok((
                Op::Delete {
                    collection: collection.to_string(),
                    key: key.to_string(),
                },
                previous,
            ))
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
        if ops.iter().any(|op| matches!(op, Op::Batch { .. })) {
            return Err(DbError::validation(
                "nested batch operations are not supported",
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
        if ops
            .iter()
            .all(|op| matches!(op, Op::Put { .. } | Op::Delete { .. }))
        {
            self.commit_document_batch(&[], ops, "batch", None)?;
            self.metrics.batches.fetch_add(1, Ordering::Relaxed);
            return Ok(count);
        }
        self.commit_schema_batch(ops, "batch")?;
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
        let seq = self.commit_document_batch(reads, ops, "transaction", None)?;
        self.metrics.transactions.fetch_add(1, Ordering::Relaxed);
        Ok(seq)
    }

    fn commit_document_batch(
        &self,
        reads: &[(String, String, u64)],
        ops: Vec<Op>,
        label: &str,
        expectations: Option<&[Option<u64>]>,
    ) -> Result<u64> {
        if expectations.is_some_and(|values| values.len() != ops.len()) {
            return Err(DbError::Internal("batch expectation count mismatch".into()));
        }
        self.check_writable("transaction commit")?;
        let mut state = self.write();
        self.check_writable("transaction commit")?;
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
        let seq = next_sequence(state.seq)?;
        if ops.len() > state.store.limits.max_batch_ops {
            return Err(DbError::LimitExceeded {
                limit: "max_batch_ops",
                max: state.store.limits.max_batch_ops as u64,
                actual: ops.len() as u64,
            });
        }
        let mut undo = Vec::with_capacity(ops.len());
        let mut undo_bytes = 0;
        for (index, op) in ops.iter().enumerate() {
            let result = (|| {
                let (collection, key) = match op {
                    Op::Put {
                        collection, key, ..
                    }
                    | Op::Delete { collection, key } => (collection, key),
                    _ => {
                        return Err(DbError::validation(
                            "transaction supports document writes only",
                        ))
                    }
                };
                let previous_ref = state.store.collection(collection)?.docs.get(key.as_str());
                if let Some(previous) = previous_ref {
                    undo_bytes =
                        reserve_undo_bytes(undo_bytes, previous.to_json().to_string().len())?;
                }
                let previous = previous_ref.cloned();
                let expected = expectations
                    .and_then(|values| values.get(index))
                    .copied()
                    .flatten();
                match op {
                    Op::Put {
                        value, expires_at, ..
                    } => {
                        state.store.put(
                            collection,
                            key,
                            value.clone(),
                            *expires_at,
                            expected,
                            now,
                            seq,
                            ApplyMode::Live,
                        )?;
                    }
                    Op::Delete { .. } => {
                        state
                            .store
                            .delete(collection, key, expected, ApplyMode::Live)?;
                    }
                    _ => unreachable!("operation was checked above"),
                }
                undo.push((collection.clone(), key.clone(), previous));
                Ok(())
            })();
            if let Err(err) = result {
                for (collection, key, previous) in undo.into_iter().rev() {
                    if let Err(rollback_err) =
                        state.store.restore_document(&collection, &key, previous)
                    {
                        self.poisoned.store(true, Ordering::SeqCst);
                        return Err(rollback_err);
                    }
                }
                return Err(err);
            }
        }
        let op = Op::Batch {
            ops,
            label: label.to_string(),
        };
        let record = Record::new(seq, now, op);
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            for (collection, key, previous) in undo.into_iter().rev() {
                state.store.restore_document(&collection, &key, previous)?;
            }
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
        }
        state.seq = seq;
        drop(writer);
        drop(state);
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
        let mut log_ops = Vec::with_capacity(count);
        let mut expectations = Vec::with_capacity(count);
        for checked in ops {
            if !matches!(&checked.op, Op::Put { .. } | Op::Delete { .. }) {
                return Err(DbError::validation(
                    "batch_checked only supports put/delete",
                ));
            }
            log_ops.push(checked.op);
            expectations.push(checked.expected_version);
        }
        self.commit_document_batch(&[], log_ops, "batch", Some(&expectations))?;
        self.metrics.batches.fetch_add(1, Ordering::Relaxed);
        Ok(count)
    }

    /// Create a collection.
    pub fn create_collection(&self, meta: CollectionMeta) -> Result<()> {
        let name = meta.name.clone();
        self.commit_schema(
            |_| Ok(name.clone()),
            |store, _seq, _now| {
                store.create_collection(meta.clone(), ApplyMode::Live)?;
                Ok(Op::CreateCollection { meta: meta.clone() })
            },
            |store, name| {
                store.collections.remove(&name);
                Ok(())
            },
        )?;
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
        self.check_writable("schema write")?;
        let mut state = self.write();
        self.check_writable("schema write")?;
        let seq = next_sequence(state.seq)?;
        let previous = state
            .store
            .collections
            .remove(collection)
            .ok_or_else(|| DbError::CollectionNotFound(collection.to_string()))?;
        let record = Record::new(
            seq,
            now_ms(),
            Op::DropCollection {
                collection: collection.to_string(),
            },
        );
        let mut writer = self.lock_mutex(&self.writer);
        if let Err(err) = writer.append(&record) {
            self.poisoned.store(true, Ordering::SeqCst);
            state
                .store
                .collections
                .insert(collection.to_string(), previous);
            return Err(DbError::CommitOutcomeUnknown(err.to_string()));
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
        Ok(())
    }

    /// Create a secondary index (builds it from the existing documents).
    pub fn create_index(&self, collection: &str, index: IndexMeta) -> Result<()> {
        let index_name = index.name.clone();
        let collection_name = collection.to_string();
        self.commit_schema(
            |_| Ok((collection_name.clone(), index_name.clone())),
            |store, _seq, _now| {
                store.create_index(collection, index.clone(), ApplyMode::Live)?;
                Ok(Op::CreateIndex {
                    collection: collection.to_string(),
                    index: index.clone(),
                })
            },
            move |store, (collection_name, index_name)| {
                store.drop_index(&collection_name, &index_name, ApplyMode::Replay)
            },
        )?;
        Ok(())
    }

    /// Drop a secondary index.
    pub fn drop_index(&self, collection: &str, name: &str) -> Result<()> {
        let collection_name = collection.to_string();
        self.commit_schema(
            |store| {
                store
                    .collection(collection)?
                    .meta
                    .indexes
                    .get(name)
                    .cloned()
                    .ok_or_else(|| DbError::IndexNotFound {
                        collection: collection.to_string(),
                        index: name.to_string(),
                    })
            },
            |store, _seq, _now| {
                store.drop_index(collection, name, ApplyMode::Live)?;
                Ok(Op::DropIndex {
                    collection: collection.to_string(),
                    name: name.to_string(),
                })
            },
            move |store, previous| {
                store.create_index(&collection_name, previous, ApplyMode::Replay)
            },
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- read path

    pub fn get(&self, collection: &str, key: &str) -> Result<Doc> {
        let state = self.read();
        self.check_healthy()?;
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
        self.check_healthy()?;
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

    /// List collections only while the engine's transaction outcome is known.
    /// Prefer this checked form in request handlers and administrative tooling.
    pub fn try_list_collections(&self) -> Result<Vec<CollectionMeta>> {
        let state = self.read();
        self.check_healthy()?;
        Ok(state
            .store
            .collections
            .values()
            .map(|coll| coll.meta.clone())
            .collect())
    }

    /// Computes a complete deterministic inventory from the currently opened
    /// persisted view while holding one read lock. This is independent of any
    /// importer progress receipts. Expired-but-not-swept documents are included.
    pub fn inventory_manifest(&self) -> Result<InventoryManifest> {
        let state = self.read();
        self.check_healthy()?;
        let mut root = Sha256::new();
        root.update(b"PREPPYDB-INVENTORY-V1\0");
        root.update(&state.seq.to_be_bytes());

        let mut collections = Vec::with_capacity(state.store.collections.len());
        let mut document_count = 0_u64;
        let mut serialized_bytes = 0_u64;
        for (name, collection) in &state.store.collections {
            let metadata = collection.meta.to_json().canonical_bytes();
            let metadata_bytes =
                u64::try_from(metadata.len()).map_err(|_| DbError::LimitExceeded {
                    limit: "inventory_bytes",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
            let mut documents_hash = Sha256::new();
            documents_hash.update(b"PREPPYDB-COLLECTION-DOCUMENTS-V1\0");
            let mut collection_bytes = 0_u64;
            let mut count = 0_u64;
            for (key, doc) in &collection.docs {
                let encoded = doc.to_json().canonical_bytes();
                let size = u64::try_from(encoded.len()).map_err(|_| DbError::LimitExceeded {
                    limit: "inventory_bytes",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
                let record_hash = inventory_record_digest(name, key, &encoded);
                documents_hash.update(&record_hash);
                hash_len_bytes(&mut documents_hash, key.as_bytes());
                count = count.checked_add(1).ok_or(DbError::LimitExceeded {
                    limit: "inventory_documents",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
                collection_bytes =
                    collection_bytes
                        .checked_add(size)
                        .ok_or(DbError::LimitExceeded {
                            limit: "inventory_bytes",
                            max: u64::MAX,
                            actual: u64::MAX,
                        })?;
            }
            let metadata_digest = preppydb_crypto::sha256::sha256(&metadata);
            let documents_digest = documents_hash.finish();
            hash_len_bytes(&mut root, name.as_bytes());
            root.update(&count.to_be_bytes());
            root.update(&metadata_bytes.to_be_bytes());
            root.update(&collection_bytes.to_be_bytes());
            root.update(&metadata_digest);
            root.update(&documents_digest);
            document_count = document_count
                .checked_add(count)
                .ok_or(DbError::LimitExceeded {
                    limit: "inventory_documents",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
            serialized_bytes = serialized_bytes
                .checked_add(metadata_bytes)
                .and_then(|value| value.checked_add(collection_bytes))
                .ok_or(DbError::LimitExceeded {
                    limit: "inventory_bytes",
                    max: u64::MAX,
                    actual: u64::MAX,
                })?;
            collections.push(InventoryCollection {
                name: name.clone(),
                document_count: count,
                metadata_bytes,
                document_bytes: collection_bytes,
                metadata_sha256: hex_encode(&metadata_digest),
                documents_sha256: hex_encode(&documents_digest),
            });
        }
        let digest = root.finish();
        let snapshot_id = format!(
            "preppydb:{}:{}:{}",
            state.generation,
            state.seq,
            hex_encode(&digest)
        );
        Ok(InventoryManifest {
            snapshot_id,
            sequence: state.seq,
            generation: state.generation,
            collection_count: u64::try_from(collections.len()).unwrap_or(u64::MAX),
            document_count,
            serialized_bytes,
            sha256: hex_encode(&digest),
            collections,
        })
    }

    /// Returns a bounded page of record identities and SHA-256 digests only if
    /// the target still has the manifest's exact sequence and generation. Any
    /// intervening write/checkpoint forces the caller to restart reconciliation.
    pub fn inventory_page(
        &self,
        expected_sequence: u64,
        expected_generation: u64,
        collection: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<InventoryPage> {
        const MAX_INVENTORY_PAGE: usize = 500;
        if limit == 0 || limit > MAX_INVENTORY_PAGE {
            return Err(DbError::validation(format!(
                "inventory page limit must be between 1 and {MAX_INVENTORY_PAGE}"
            )));
        }
        if after.is_some_and(|cursor| cursor.len() > 1_024) {
            return Err(DbError::validation("inventory cursor is too long"));
        }
        let state = self.read();
        self.check_healthy()?;
        if state.seq != expected_sequence {
            return Err(DbError::Conflict {
                collection: "_inventory".into(),
                key: "sequence".into(),
                expected: Some(expected_sequence),
                actual: Some(state.seq),
            });
        }
        if state.generation != expected_generation {
            return Err(DbError::Conflict {
                collection: "_inventory".into(),
                key: "generation".into(),
                expected: Some(expected_generation),
                actual: Some(state.generation),
            });
        }
        let coll = state.store.collection(collection)?;
        let lower = after.map(Bound::Excluded).unwrap_or(Bound::Unbounded);
        let mut records = Vec::with_capacity(limit);
        let mut has_more = false;
        for (key, doc) in coll.docs.range::<str, _>((lower, Bound::Unbounded)) {
            if records.len() == limit {
                has_more = true;
                break;
            }
            let encoded = doc.to_json().canonical_bytes();
            let digest = inventory_record_digest(collection, key, &encoded);
            records.push(InventoryRecord {
                key: key.to_string(),
                serialized_bytes: u64::try_from(encoded.len()).map_err(|_| {
                    DbError::LimitExceeded {
                        limit: "inventory_record_bytes",
                        max: u64::MAX,
                        actual: u64::MAX,
                    }
                })?,
                sha256: hex_encode(&digest),
            });
        }
        let next_cursor = has_more
            .then(|| records.last().map(|record| record.key.clone()))
            .flatten();
        Ok(InventoryPage {
            sequence: state.seq,
            generation: state.generation,
            collection: collection.to_owned(),
            records,
            next_cursor,
        })
    }

    pub fn collection_meta(&self, name: &str) -> Result<CollectionMeta> {
        let state = self.read();
        self.check_healthy()?;
        Ok(state.store.collection(name)?.meta.clone())
    }

    /// Execute a bounded query while validating the access path against current metadata.
    pub fn query_documents(
        &self,
        collection: &str,
        filter: Option<&DocumentFilter>,
        access: &DocumentAccess,
        limit: usize,
    ) -> Result<Vec<(String, Doc)>> {
        if !(1..=1000).contains(&limit) {
            return Err(DbError::validation(
                "query limit must be between 1 and 1000",
            ));
        }
        if let Some(filter) = filter {
            if filter.field.len() > 128
                || Json::parse_path(&filter.field).is_err()
                || !matches!(filter.operator.as_str(), "eq" | "gte" | "lte")
                || !matches!(
                    filter.value,
                    Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_)
                )
            {
                return Err(DbError::validation("invalid query filter"));
            }
        }
        let state = self.read();
        self.check_healthy()?;
        let coll = state.store.collection(collection)?;
        let now = now_ms();
        let mut output = Vec::new();
        match access {
            DocumentAccess::FullScan => {
                for (key, doc) in &coll.docs {
                    if !doc.is_expired(now) && matches_document_filter(doc, filter) {
                        output.push((key.to_string(), doc.clone()));
                        if output.len() == limit {
                            break;
                        }
                    }
                }
            }
            DocumentAccess::IndexEquality(name) => {
                let filter =
                    filter.ok_or_else(|| DbError::validation("index query requires a filter"))?;
                if filter.operator != "eq"
                    || !matches!(filter.value, Json::String(_) | Json::Bool(_))
                {
                    return Err(DbError::validation("unsupported index comparison"));
                }
                let index = coll
                    .indexes
                    .get(name)
                    .ok_or_else(|| DbError::IndexNotFound {
                        collection: collection.into(),
                        index: name.clone(),
                    })?;
                if index.meta.field != filter.field {
                    return Err(DbError::validation("index field mismatch"));
                }
                if let Some(keys) = index.entries.get(&filter.value.canonical_bytes()) {
                    for key in keys {
                        if let Some(doc) = coll.docs.get(key.as_ref()) {
                            if !doc.is_expired(now) && matches_document_filter(doc, Some(filter)) {
                                output.push((key.to_string(), doc.clone()));
                                if output.len() == limit {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(output)
    }

    /// Scan documents in key order.
    pub fn scan(&self, collection: &str, options: &ScanOptions) -> Result<Vec<(String, Doc)>> {
        let state = self.read();
        self.check_healthy()?;
        let now = now_ms();
        let coll = state.store.collection(collection)?;
        let upper = options.prefix.as_deref().and_then(prefix_upper_bound);
        let lower_bound = options
            .prefix
            .as_deref()
            .map(Bound::Included)
            .unwrap_or(Bound::Unbounded);
        let upper_bound = upper
            .as_deref()
            .map(Bound::Excluded)
            .unwrap_or(Bound::Unbounded);
        let items = coll.docs.range::<str, _>((lower_bound, upper_bound));
        let limit = if options.limit == 0 {
            usize::MAX
        } else {
            options.limit
        };
        let visible =
            |(_, doc): &(&Arc<str>, &Doc)| options.include_expired || !doc.is_expired(now);
        if options.reverse {
            Ok(items
                .rev()
                .filter(visible)
                .skip(options.offset)
                .take(limit)
                .map(|(key, doc)| (key.to_string(), doc.clone()))
                .collect())
        } else {
            Ok(items
                .filter(visible)
                .skip(options.offset)
                .take(limit)
                .map(|(key, doc)| (key.to_string(), doc.clone()))
                .collect())
        }
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
        self.check_healthy()?;
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
                        out.push((key.to_string(), doc.clone()));
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
        self.check_healthy()?;
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
        let iter: Box<IndexEntryIter<'_>> = match (start, end) {
            (Some(start), Some(end)) => Box::new(index_state.entries.range(start..=end)),
            (Some(start), None) => Box::new(index_state.entries.range(start..)),
            (None, Some(end)) => Box::new(index_state.entries.range(..=end)),
            (None, None) => Box::new(index_state.entries.iter()),
        };
        let mut out = Vec::new();
        for (_, keys) in iter {
            for key in keys {
                if let Some(doc) = coll.docs.get(key.as_ref()) {
                    if doc.is_expired(now) {
                        continue;
                    }
                    let value = doc
                        .value
                        .get_path(&index_state.meta.field)
                        .cloned()
                        .unwrap_or(Json::Null);
                    out.push((key.to_string(), value, doc.clone()));
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
        self.check_healthy()?;
        let now = now_ms();
        Ok(state.store.collection(collection)?.live_len(now))
    }

    // ------------------------------------------------------------- TTL

    /// Remove expired documents, returning how many were removed. Each removal is
    /// logged as an explicit `ttl_expiry` batch, so it is auditable and never silent.
    pub fn sweep_expired(&self) -> Result<usize> {
        self.check_healthy()?;
        let expired = {
            let state = self.read();
            state
                .store
                .expired_keys(now_ms(), self.cfg.limits.max_batch_ops)
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
            .collect();
        let removed = ops.len();
        self.commit_document_batch(&[], ops, "ttl_expiry", None)?;
        self.metrics.sweeps.fetch_add(1, Ordering::Relaxed);
        Ok(removed)
    }

    // ------------------------------------------------------------- checkpoint

    /// Write a snapshot of the current state and retire the log segments it covers.
    pub fn checkpoint(&self) -> Result<WrittenSnapshot> {
        self.check_writable("checkpoint")?;
        let mut state = self.write();
        self.check_writable("checkpoint")?;
        let mut writer = self.lock_mutex(&self.writer);
        let mut manifest = self.lock_mutex(&self.manifest);

        // 1. Rotate so the snapshot can never overlap the segment we keep writing to.
        writer.rotate()?;
        let keep_index = writer.current_index();
        let generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| DbError::LimitExceeded {
                limit: "manifest_generation",
                max: 999_999_999_999,
                actual: state.generation,
            })?;
        let seq = state.seq;

        // 2. Serialise directly from the write-locked state. The snapshot writer
        // only borrows documents, avoiding a second full copy of the database.
        let snapshot_path = self.paths.snapshot_file(generation);
        // A crash after snapshot publication but before manifest publication leaves
        // an unreferenced candidate. The active manifest still points elsewhere, so
        // removing that candidate is safe before retrying the checkpoint.
        match std::fs::symlink_metadata(&snapshot_path) {
            Ok(_) => std::fs::remove_file(&snapshot_path)
                .map_err(|err| DbError::io("removing orphan snapshot generation", err))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(DbError::io("inspecting snapshot destination", err)),
        }
        let written = snapshot::write_snapshot_from_store(
            &snapshot_path,
            generation,
            seq,
            now_ms(),
            &state.store,
        )?;

        // 3. Make the new snapshot authoritative.
        let previous_snapshot = manifest.snapshot.clone();
        let mut candidate = manifest.clone();
        candidate.generation = generation;
        candidate.last_seq = seq;
        candidate.snapshot = Some(SnapshotRef {
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
        candidate.wal = vec![wal::segment_name(keep_index)];
        if let Err(err) = candidate.publish_generation(&self.paths.root) {
            // A failure after rename can leave publication outcome uncertain.
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(err);
        }
        *manifest = candidate;
        state.generation = generation;
        Manifest::remove_superseded_generations(&self.paths.root, generation);

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
        self.stats_for_state(&state)
    }

    /// Return statistics only when the transaction outcome is known.
    ///
    /// `stats` remains available for source compatibility; request handlers and
    /// administrative tools should use this checked form so a poisoned engine cannot
    /// report stale counts as if they described the current committed state.
    pub fn try_stats(&self) -> Result<EngineStats> {
        let state = self.read();
        self.check_healthy()?;
        Ok(self.stats_for_state(&state))
    }

    fn stats_for_state(&self, state: &State) -> EngineStats {
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
        // Keep the snapshot, WAL, and manifest at one logical point in time. The lock
        // order matches commits/checkpoint (state -> writer -> manifest), avoiding a
        // report assembled across a concurrent commit or checkpoint publication.
        let _state = self.read();
        let _writer = self.lock_mutex(&self.writer);
        let mut issues = Vec::new();
        let manifest = self.lock_mutex(&self.manifest).clone();
        let wal_segments = wal::list_segments(&self.paths.wal_dir)?;
        // Verification is an administrative path, so a streaming metadata prepass
        // is preferable to keeping a second full document store beside the live one.
        // If metadata is damaged or exceeds the bounded plan, retain the established
        // full replay path so the report can still diagnose the original condition.
        let snapshot_path = manifest.snapshot.as_ref().and_then(|snapshot| {
            validate_snapshot_reference(&manifest, snapshot).ok()?;
            let path = self.paths.snap_dir.join(&snapshot.file);
            ensure_regular_snapshot_file(&path).ok()?;
            Some(path)
        });
        let verify_index_fields = if manifest.snapshot.is_none() || snapshot_path.is_some() {
            collect_verify_index_fields(&wal_segments)?
        } else {
            None
        };
        // Framing and sequence checks alone are insufficient: a checksummed record
        // can still be impossible to apply (for example, a unique index that conflicts
        // with existing documents). Reconstruct the snapshot baseline and replay the
        // retained suffix just as open() does, so a clean report predicts reopenability.
        let mut replay_store = match &verify_index_fields {
            Some(fields) => VerifyReplayStore::Compact(VerifyStore::with_field_plan(
                self.cfg.limits.clone(),
                fields.clone(),
            )),
            None => VerifyReplayStore::Full(Store::new(self.cfg.limits.clone())),
        };
        let mut replay_state_valid = manifest.snapshot.is_none();
        let mut snapshot_json = None;
        if let Some(snap_ref) = &manifest.snapshot {
            let path = match validate_snapshot_reference(&manifest, snap_ref) {
                Ok(()) => Some(self.paths.snap_dir.join(&snap_ref.file)),
                Err(err) => {
                    issues.push(err.to_string());
                    snapshot_json = Some(
                        Json::obj()
                            .with("file", Json::str(snap_ref.file.clone()))
                            .with("ok", Json::Bool(false))
                            .with("error", Json::str(err.to_string())),
                    );
                    None
                }
            };
            if let Some(path) = path {
                if let Err(err) = ensure_regular_snapshot_file(&path) {
                    issues.push(err.to_string());
                    snapshot_json = Some(
                        Json::obj()
                            .with("file", Json::str(snap_ref.file.clone()))
                            .with("ok", Json::Bool(false))
                            .with("error", Json::str(err.to_string())),
                    );
                } else {
                    let snapshot_result =
                        match verify_index_fields.as_ref() {
                            Some(fields) => match snapshot::read_snapshot_verification_projected(
                                &path,
                                self.cfg.limits.clone(),
                                fields,
                            ) {
                                Ok(data) => Ok(VerifySnapshotData {
                                    header: data.header,
                                    body_sha256: data.body_sha256,
                                    store: VerifyReplayStore::Compact(data.store),
                                }),
                                Err(DbError::LimitExceeded {
                                    limit: "verify_index_field_plan",
                                    ..
                                }) => {
                                    // Snapshot and WAL metadata together exceeded the
                                    // bounded projection plan. Fall back to the established
                                    // full-document replay instead of omitting index fields.
                                    snapshot::read_snapshot_store(&path, self.cfg.limits.clone())
                                        .map(|data| VerifySnapshotData {
                                            header: data.header,
                                            body_sha256: data.body_sha256,
                                            store: VerifyReplayStore::Full(data.store),
                                        })
                                }
                                Err(err) => Err(err),
                            },
                            None => snapshot::read_snapshot_store(&path, self.cfg.limits.clone())
                                .map(|data| VerifySnapshotData {
                                    header: data.header,
                                    body_sha256: data.body_sha256,
                                    store: VerifyReplayStore::Full(data.store),
                                }),
                        };
                    match snapshot_result {
                        Ok(data) => {
                            let mut ok = true;
                            if let Err(err) =
                                validate_snapshot_contents(snap_ref, &data.header, &path)
                            {
                                ok = false;
                                issues.push(err.to_string());
                            }
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
                            if ok {
                                replay_store = data.store;
                            }
                            replay_state_valid = ok;
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
            }
        }

        let mut segments = Vec::new();
        let snapshot_seq = manifest
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.seq);
        let mut previous_seq = snapshot_seq;
        let mut replay_started = false;
        for (position, segment) in wal_segments.iter().enumerate() {
            let name = segment
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut record_count = 0u64;
            let mut sequence_corruption = None;
            let scan = wal::scan_segment_visit(segment, |record| {
                if record.seq <= snapshot_seq {
                    if replay_started {
                        sequence_corruption = Some(format!(
                            "record sequence {} regresses after replay reached {}",
                            record.seq, previous_seq
                        ));
                    }
                    record_count = record_count
                        .checked_add(1)
                        .ok_or_else(|| DbError::corrupt(&name, "WAL record count overflow"))?;
                    return Ok(());
                }
                let expected = previous_seq
                    .checked_add(1)
                    .ok_or_else(|| DbError::corrupt(&name, "WAL record sequence overflow"))?;
                if record.seq != expected {
                    sequence_corruption = Some(format!(
                        "record sequence {} does not continue from {}",
                        record.seq, previous_seq
                    ));
                    record_count = record_count
                        .checked_add(1)
                        .ok_or_else(|| DbError::corrupt(&name, "WAL record count overflow"))?;
                    return Ok(());
                }
                previous_seq = record.seq;
                if replay_state_valid {
                    if let Err(err) = replay_store.apply(&record.op, record.ts, record.seq) {
                        issues.push(format!(
                            "{name}: record sequence {} cannot be replayed: {err}",
                            record.seq
                        ));
                        replay_state_valid = false;
                    }
                }
                replay_started = true;
                record_count = record_count
                    .checked_add(1)
                    .ok_or_else(|| DbError::corrupt(&name, "WAL record count overflow"))?;
                Ok(())
            })?;
            if let Some(corruption) = sequence_corruption.as_ref() {
                issues.push(format!("{name}: {corruption}"));
            }
            if let Some(corruption) = &scan.corruption {
                issues.push(format!("{name}: {corruption}"));
            }
            if let Some(torn) = &scan.torn_tail {
                issues.push(format!("{name}: torn tail ({torn})"));
            }
            let torn_followed_by_segment =
                scan.torn_tail.is_some() && position + 1 < wal_segments.len();
            if torn_followed_by_segment {
                issues.push(format!(
                    "{name}: torn WAL tail is followed by another segment"
                ));
            }
            let ok = scan.corruption.is_none()
                && scan.torn_tail.is_none()
                && sequence_corruption.is_none()
                && !torn_followed_by_segment;
            segments.push(
                Json::obj()
                    .with("file", Json::str(name))
                    .with("ok", Json::Bool(ok))
                    .with("records", Json::uint(record_count))
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
        if previous_seq < manifest.last_seq {
            issues.push(format!(
                "manifest last_seq {} is ahead of the verifiable WAL sequence {}; WAL history is incomplete",
                manifest.last_seq, previous_seq
            ));
        }
        Ok(VerifyReport {
            ok: issues.is_empty(),
            snapshot: snapshot_json,
            segments,
            issues,
        })
    }

    /// A consistent, self-contained copy of the data when the engine outcome is known.
    pub fn try_export_snapshot(&self) -> Result<Json> {
        let state = self.read();
        self.check_healthy()?;
        Ok(self.export_snapshot_from_state(&state))
    }

    /// Export the current in-memory view without checking whether the WAL outcome is known.
    ///
    /// This method remains for source compatibility. New callers should use
    /// [`Engine::try_export_snapshot`], which refuses to return a possibly stale view after
    /// a WAL append or sync failure.
    #[deprecated(note = "use try_export_snapshot to reject unknown WAL outcomes")]
    pub fn export_snapshot(&self) -> Json {
        let state = self.read();
        self.export_snapshot_from_state(&state)
    }

    fn export_snapshot_from_state(&self, state: &State) -> Json {
        let now = now_ms();
        let mut collections = Json::obj();
        for (name, coll) in &state.store.collections {
            let mut docs = Json::obj();
            for (key, doc) in &coll.docs {
                if doc.is_expired(now) {
                    continue;
                }
                docs.set(key.to_string(), doc.to_json());
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

    /// Hold a stable view of on-disk files while a backup reads them.
    pub(crate) fn with_stable_files<T>(&self, action: impl FnOnce(u64) -> Result<T>) -> Result<T> {
        if self.cfg.read_only {
            return Err(DbError::Unsupported(
                "consistent backup requires a writable engine holding the database lock".into(),
            ));
        }
        let state = self.read();
        self.check_healthy()?;
        self.sync_wal()?;
        action(state.seq)
    }

    /// Flush the WAL to disk.
    pub fn flush(&self) -> Result<()> {
        if self.cfg.read_only {
            return Ok(());
        }
        self.sync_wal()
    }

    /// Graceful shutdown: flush, checkpoint (optional) and release the lock.
    pub fn shutdown(&self) -> Result<()> {
        let _state = self.write();
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.stop_interval_sync_worker();
        if !self.cfg.read_only {
            self.flush()?;
        }
        self.closed.store(true, Ordering::SeqCst);
        let mut lock = self.lock_mutex(&self.lock);
        if let Some(lock) = lock.as_mut() {
            lock.release();
        }
        Ok(())
    }
}

fn repair_corrupt_wal_suffix(
    segment: &Path,
    later_segments: &[PathBuf],
    valid_len: u64,
) -> Result<()> {
    repair_corrupt_wal_suffix_with(
        segment,
        later_segments,
        valid_len,
        |segment, valid_len| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(segment)
                .map_err(|err| DbError::io("opening corrupt WAL segment for repair", err))?;
            file.set_len(valid_len)
                .map_err(|err| DbError::io("truncating corrupt WAL suffix", err))?;
            file.sync_all()
                .map_err(|err| DbError::io("syncing repaired WAL segment", err))
        },
        |path| std::fs::remove_file(path),
        crate::snapshot::fsync_dir,
    )
}

fn repair_corrupt_wal_suffix_with<T, R, D>(
    segment: &Path,
    later_segments: &[PathBuf],
    valid_len: u64,
    truncate_and_sync: T,
    mut remove_segment: R,
    sync_directory: D,
) -> Result<()>
where
    T: FnOnce(&Path, u64) -> Result<()>,
    R: FnMut(&Path) -> std::io::Result<()>,
    D: FnOnce(&Path) -> Result<()>,
{
    truncate_and_sync(segment, valid_len)?;
    for later_segment in later_segments {
        match remove_segment(later_segment) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(DbError::io(
                    format!(
                        "removing WAL segment {} after corruption",
                        later_segment.display()
                    ),
                    err,
                ));
            }
        }
    }
    sync_directory(segment.parent().unwrap_or_else(|| Path::new(".")))
}

impl Drop for Engine {
    fn drop(&mut self) {
        let worker = self
            .interval_sync_worker
            .get_mut()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        drop(worker);
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
    Ok(snapshot::read_snapshot_store(path, limits)?.store)
}

/// Helper for tests and tooling: the collection view without cloning the whole store.
pub fn collection_ref<'a>(store: &'a Store, name: &str) -> Result<&'a Collection> {
    store.collection(name)
}

#[cfg(test)]
mod document_undo_tests {
    use super::*;

    #[test]
    fn verifier_field_plan_retains_nested_values_for_future_unique_indexes() {
        let fields = BTreeMap::from([(
            "users".to_string(),
            BTreeSet::from(["profile.level".to_string()]),
        )]);
        let mut verify = VerifyStore::with_field_plan(Limits::default(), fields);
        verify
            .create_collection(CollectionMeta::new("users", 1))
            .unwrap();
        verify
            .insert_snapshot_document(
                "users",
                "a".to_string(),
                &Json::obj().with(
                    "profile",
                    Json::obj()
                        .with("level", Json::uint(17))
                        .with("name", Json::str("Charlotte"))
                        .with("unused", Json::Bool(true)),
                ),
            )
            .unwrap();
        verify
            .apply(&Op::CreateIndex {
                collection: "users".to_string(),
                index: IndexMeta {
                    name: "level".to_string(),
                    field: "profile.level".to_string(),
                    unique: true,
                    sparse: false,
                    created_at: 2,
                },
            })
            .unwrap();
        assert!(verify
            .apply(&Op::Put {
                collection: "users".to_string(),
                key: "b".to_string(),
                value: Json::obj().with("profile", Json::obj().with("level", Json::uint(17))),
                expires_at: None,
            })
            .is_err());
    }

    #[test]
    fn verifier_index_projection_plan_is_bounded() {
        let mut fields = BTreeMap::new();
        let mut total_bytes = 0;
        let mut total_count = 0;
        for index in 0..MAX_VERIFY_INDEX_FIELDS {
            assert!(add_verify_index_field(
                &mut fields,
                &mut total_bytes,
                &mut total_count,
                "c",
                &format!("field_{index}")
            ));
        }
        let bounded_bytes = total_bytes;
        assert_eq!(total_count, MAX_VERIFY_INDEX_FIELDS);
        assert!(add_verify_index_field(
            &mut fields,
            &mut total_bytes,
            &mut total_count,
            "c",
            "field_0"
        ));
        assert_eq!(total_count, MAX_VERIFY_INDEX_FIELDS);
        assert!(!add_verify_index_field(
            &mut fields,
            &mut total_bytes,
            &mut total_count,
            "c",
            "one_more_field"
        ));
        assert_eq!(total_bytes, bounded_bytes);
        assert_eq!(
            fields.values().map(BTreeSet::len).sum::<usize>(),
            MAX_VERIFY_INDEX_FIELDS
        );

        let mut byte_limited = BTreeMap::new();
        let mut byte_count = 0;
        let mut field_count = 0;
        let too_long = "x".repeat(MAX_VERIFY_INDEX_FIELD_BYTES + 1);
        assert!(!add_verify_index_field(
            &mut byte_limited,
            &mut byte_count,
            &mut field_count,
            "c",
            &too_long
        ));
        assert!(byte_limited.is_empty());
        assert_eq!(byte_count, 0);
        assert_eq!(field_count, 0);
    }

    #[test]
    fn drop_collection_sequence_overflow_preserves_live_collection() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-drop-sequence-overflow-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        let original_seq = engine.read().seq;
        engine.write().seq = u64::MAX;

        assert!(engine.drop_collection("c", true).is_err());
        assert!(engine.read().store.has_collection("c"));

        engine.write().seq = original_seq;
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn interval_config(dir: &Path) -> EngineConfig {
        let mut config = EngineConfig::new(dir);
        config.sync_mode = SyncMode::Interval(20);
        config
    }

    #[test]
    fn single_document_append_failure_restores_moved_previous_value() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-single-document-undo-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        let old_value = Json::obj()
            .with("payload", Json::str("old"))
            .with("revision", Json::int(1));
        engine
            .put("c", "key", old_value.clone(), PutOptions::default())
            .unwrap();

        engine.lock_mutex(&engine.writer).inject_append_failure();
        assert!(engine
            .put(
                "c",
                "key",
                Json::obj()
                    .with("payload", Json::str("new"))
                    .with("revision", Json::int(2)),
                PutOptions::default(),
            )
            .is_err());

        let state = engine.read();
        assert_eq!(
            state.store.collection("c").unwrap().docs["key"].value,
            old_value
        );
        drop(state);
        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(reopened.get("c", "key").unwrap().value, old_value);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_interval_is_rejected_before_creating_database_files() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-zero-interval-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut config = EngineConfig::new(&dir);
        config.sync_mode = SyncMode::Interval(0);
        assert!(matches!(Engine::open(config), Err(DbError::Config(_))));
        assert!(!dir.exists());
    }

    #[test]
    fn interval_sync_worker_flushes_acknowledged_writes() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-interval-sync-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(interval_config(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2)
            && engine.lock_mutex(&engine.writer).successful_syncs() == 0
        {
            thread::sleep(Duration::from_millis(2));
        }
        let syncs_before_put = engine.lock_mutex(&engine.writer).successful_syncs();
        engine
            .put("c", "key", Json::str("value"), PutOptions::default())
            .unwrap();

        let start = std::time::Instant::now();
        let mut observed_sync = false;
        while start.elapsed() < Duration::from_secs(2) {
            if engine.lock_mutex(&engine.writer).successful_syncs() > syncs_before_put {
                observed_sync = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            observed_sync,
            "interval worker did not sync pending WAL writes"
        );
        engine.shutdown().unwrap();
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interval_sync_failure_poisons_engine_and_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-interval-sync-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(interval_config(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine.lock_mutex(&engine.writer).inject_sync_failure();
        engine
            .put("c", "key", Json::str("value"), PutOptions::default())
            .unwrap();

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) && !engine.poisoned.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(engine.poisoned.load(Ordering::SeqCst));
        assert!(matches!(
            engine.get("c", "key"),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        assert!(matches!(
            engine.shutdown(),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn synchronous_commit_sync_failure_is_unknown_and_requires_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-sync-commit-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "key", Json::str("old"), PutOptions::default())
            .unwrap();

        engine.lock_mutex(&engine.writer).inject_sync_failure();
        let error = engine
            .put("c", "key", Json::str("new"), PutOptions::default())
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::COMMIT_OUTCOME_UNKNOWN);
        assert!(matches!(
            engine.get("c", "key"),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        assert!(matches!(
            engine.put("c", "other", Json::Null, PutOptions::default()),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        drop(engine);

        // The failed barrier leaves the outcome uncertain. Drop may subsequently sync
        // the complete WAL frame; after reopen recovery is the source of truth.
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(
            reopened.get("c", "key").unwrap().value.as_str(),
            Some("new")
        );
        assert!(reopened.get_opt("c", "other").unwrap().is_none());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transaction_sync_failure_recovers_all_or_none_of_the_wal_batch() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-transaction-sync-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();

        let mut transaction = engine.begin();
        transaction.put("c", "first", Json::int(1), None).unwrap();
        transaction.put("c", "second", Json::int(2), None).unwrap();
        engine.lock_mutex(&engine.writer).inject_sync_failure();
        let error = transaction.commit().unwrap_err();
        assert_eq!(error.code(), crate::error::codes::COMMIT_OUTCOME_UNKNOWN);
        assert!(matches!(
            engine.get_opt("c", "first"),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        drop(engine);

        // The failed durability barrier leaves the transaction outcome unknown,
        // but the one-frame transaction must never recover only half its writes.
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        let first = reopened.get_opt("c", "first").unwrap();
        let second = reopened.get_opt("c", "second").unwrap();
        assert_eq!(first.is_some(), second.is_some());
        if let (Some(first), Some(second)) = (first, second) {
            assert_eq!(first.value.as_i64(), Some(1));
            assert_eq!(second.value.as_i64(), Some(2));
        }
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checked_snapshot_export_refuses_unknown_wal_outcome() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-export-sync-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "key", Json::str("old"), PutOptions::default())
            .unwrap();

        engine.lock_mutex(&engine.writer).inject_sync_failure();
        let error = engine
            .put("c", "key", Json::str("new"), PutOptions::default())
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::COMMIT_OUTCOME_UNKNOWN);
        assert!(matches!(
            engine.try_export_snapshot(),
            Err(DbError::CommitOutcomeUnknown(_))
        ));

        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(
            reopened
                .try_export_snapshot()
                .unwrap()
                .get("collections")
                .and_then(|collections| collections.get("c"))
                .and_then(|collection| collection.get("documents"))
                .and_then(|documents| documents.get("key"))
                .and_then(|document| document.get("value"))
                .and_then(Json::as_str),
            Some("new")
        );
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_after_wal_sync_failure_preserves_unknown_outcome_error() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-backup-sync-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let archive = dir.with_extension("bak");
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine.lock_mutex(&engine.writer).inject_sync_failure();
        assert_eq!(
            engine
                .put("c", "key", Json::int(1), PutOptions::default())
                .unwrap_err()
                .code(),
            crate::error::codes::COMMIT_OUTCOME_UNKNOWN
        );

        let error = crate::backup::create_backup(&engine, &archive, false).unwrap_err();
        assert_eq!(error.code(), crate::error::codes::COMMIT_OUTCOME_UNKNOWN);
        assert!(!archive.exists());
        drop(engine);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exhausted_wal_sequence_returns_a_limit_error_without_mutating() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-sequence-overflow-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine.write().seq = u64::MAX;

        let error = engine
            .put("c", "key", Json::str("value"), PutOptions::default())
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert_eq!(engine.read().seq, u64::MAX);
        assert!(!engine
            .read()
            .store
            .collection("c")
            .unwrap()
            .docs
            .contains_key("key"));

        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nested_batch_is_rejected_before_persistence() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-nested-batch-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert!(engine
            .batch(vec![Op::Batch {
                ops: vec![Op::CreateCollection {
                    meta: CollectionMeta::new("nested", 0),
                }],
                label: "nested".into(),
            }])
            .is_err());
        drop(engine);

        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert!(reopened.collection_meta("nested").is_err());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn schema_append_failures_restore_collection_and_index_state() {
        for operation in [
            "create_collection",
            "drop_collection",
            "create_index",
            "drop_index",
        ] {
            let dir = std::env::temp_dir().join(format!(
                "preppydb-schema-undo-{operation}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .put(
                    "c",
                    "a",
                    Json::obj().with("v", Json::int(1)),
                    PutOptions::default(),
                )
                .unwrap();
            let index = IndexMeta {
                name: "idx_v".into(),
                field: "v".into(),
                unique: false,
                sparse: false,
                created_at: now_ms(),
            };
            if operation == "drop_index" {
                engine.create_index("c", index.clone()).unwrap();
            }
            engine.lock_mutex(&engine.writer).inject_append_failure();
            match operation {
                "create_collection" => {
                    assert!(engine
                        .create_collection(CollectionMeta::new("new", 0))
                        .is_err());
                }
                "drop_collection" => {
                    assert!(engine.drop_collection("c", true).is_err());
                }
                "create_index" => {
                    assert!(engine.create_index("c", index).is_err());
                }
                "drop_index" => {
                    assert!(engine.drop_index("c", "idx_v").is_err());
                }
                _ => unreachable!(),
            }
            drop(engine);
            let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
            assert_eq!(reopened.count("c").unwrap(), 1);
            assert_eq!(
                reopened
                    .collection_meta("c")
                    .unwrap()
                    .indexes
                    .contains_key("idx_v"),
                operation == "drop_index"
            );
            assert!(reopened.collection_meta("new").is_err());
            drop(reopened);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn mixed_schema_batch_preserves_order_and_rolls_back_only_touched_collections() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-schema-batch-undo-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("existing", 0))
            .unwrap();
        engine
            .create_collection(CollectionMeta::new("untouched", 0))
            .unwrap();
        engine
            .put(
                "existing",
                "old",
                Json::obj().with("value", Json::int(1)),
                PutOptions::default(),
            )
            .unwrap();
        engine
            .put(
                "untouched",
                "kept",
                Json::obj().with("value", Json::int(2)),
                PutOptions::default(),
            )
            .unwrap();

        // An ordered mixed batch can create a collection and then write into it.
        engine
            .batch(vec![
                Op::CreateCollection {
                    meta: CollectionMeta::new("created", 0),
                },
                Op::Put {
                    collection: "created".into(),
                    key: "new".into(),
                    value: Json::obj().with("value", Json::int(3)),
                    expires_at: None,
                },
            ])
            .unwrap();
        assert_eq!(engine.count("created").unwrap(), 1);

        let index = IndexMeta {
            name: "idx_value".into(),
            field: "value".into(),
            unique: false,
            sparse: false,
            created_at: now_ms(),
        };
        engine.lock_mutex(&engine.writer).inject_append_failure();
        assert!(engine
            .batch(vec![
                Op::CreateIndex {
                    collection: "existing".into(),
                    index,
                },
                Op::DropCollection {
                    collection: "untouched".into(),
                },
                Op::CreateCollection {
                    meta: CollectionMeta::new("created_then_failed", 0),
                },
            ])
            .is_err());
        assert!(engine.count("untouched").is_err());
        {
            let state = engine.read();
            assert!(!state
                .store
                .collection("existing")
                .unwrap()
                .indexes
                .contains_key("idx_value"));
            assert!(state.store.collections.contains_key("untouched"));
            assert!(!state.store.collections.contains_key("created_then_failed"));
        }

        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(reopened.count("existing").unwrap(), 1);
        assert_eq!(reopened.count("untouched").unwrap(), 1);
        assert_eq!(reopened.count("created").unwrap(), 1);
        assert!(!reopened
            .collection_meta("existing")
            .unwrap()
            .indexes
            .contains_key("idx_value"));
        assert!(reopened.collection_meta("created_then_failed").is_err());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checked_batch_conflict_restores_prior_mutations() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-checked-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "a", Json::int(1), PutOptions::default())
            .unwrap();
        let put = |value| Op::Put {
            collection: "c".into(),
            key: "a".into(),
            value: Json::int(value),
            expires_at: None,
        };
        let ops = vec![
            CheckedOp {
                op: put(2),
                expected_version: Some(1),
            },
            CheckedOp {
                op: put(3),
                expected_version: Some(1),
            },
        ];
        assert!(matches!(
            engine.batch_checked(ops),
            Err(DbError::Conflict { .. })
        ));
        assert_eq!(engine.get("c", "a").unwrap().value.as_i64(), Some(1));
        assert_eq!(engine.get("c", "a").unwrap().version, 1);
        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(reopened.get("c", "a").unwrap().value.as_i64(), Some(1));
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_document_batch_restores_repeated_key_and_other_documents() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-batch-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "a", Json::int(1), PutOptions::default())
            .unwrap();
        let ops = vec![
            Op::Put {
                collection: "c".into(),
                key: "a".into(),
                value: Json::int(2),
                expires_at: None,
            },
            Op::Put {
                collection: "c".into(),
                key: "a".into(),
                value: Json::int(3),
                expires_at: None,
            },
            Op::Put {
                collection: "c".into(),
                key: "b".into(),
                value: Json::int(4),
                expires_at: None,
            },
        ];
        engine.lock_mutex(&engine.writer).inject_append_failure();
        assert!(engine.batch(ops).is_err());
        assert!(engine.get("c", "a").is_err());
        {
            let state = engine.read();
            assert_eq!(
                state.store.collection("c").unwrap().docs["a"]
                    .value
                    .as_i64(),
                Some(1)
            );
            assert!(!state.store.collection("c").unwrap().docs.contains_key("b"));
        }
        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(reopened.get("c", "a").unwrap().value.as_i64(), Some(1));
        assert!(reopened.get_opt("c", "b").unwrap().is_none());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transaction_append_failure_restores_all_documents() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-txn-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "a", Json::int(1), PutOptions::default())
            .unwrap();
        engine
            .put("c", "b", Json::int(2), PutOptions::default())
            .unwrap();
        let mut txn = engine.begin();
        txn.put("c", "a", Json::int(3), None).unwrap();
        txn.delete("c", "b").unwrap();
        txn.put("c", "new", Json::int(4), None).unwrap();
        engine.lock_mutex(&engine.writer).inject_append_failure();
        assert!(txn.commit().is_err());
        assert!(engine.get("c", "a").is_err());
        {
            let state = engine.read();
            assert_eq!(
                state.store.collection("c").unwrap().docs["a"]
                    .value
                    .as_i64(),
                Some(1)
            );
            assert_eq!(
                state.store.collection("c").unwrap().docs["b"]
                    .value
                    .as_i64(),
                Some(2)
            );
            assert!(!state
                .store
                .collection("c")
                .unwrap()
                .docs
                .contains_key("new"));
        }
        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(reopened.get("c", "a").unwrap().value.as_i64(), Some(1));
        assert_eq!(reopened.get("c", "b").unwrap().value.as_i64(), Some(2));
        assert!(reopened.get_opt("c", "new").unwrap().is_none());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_write_and_pre_sync_failure_have_distinct_recovery_outcomes() {
        for (tag, fault, expected_after_reopen) in [
            ("partial", crate::wal::AppendFault::PartialWrite, "old"),
            ("pre-sync", crate::wal::AppendFault::BeforeSync, "new"),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "preppydb-wal-fault-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .put("c", "key", Json::str("old"), PutOptions::default())
                .unwrap();
            engine.lock_mutex(&engine.writer).inject_fault(fault);
            let commit_error = engine
                .put("c", "key", Json::str("new"), PutOptions::default())
                .unwrap_err();
            assert_eq!(
                commit_error.code(),
                crate::error::codes::COMMIT_OUTCOME_UNKNOWN,
                "unexpected append error classification: {commit_error}"
            );
            assert!(engine.get("c", "key").is_err());
            assert_eq!(
                engine.try_list_collections().unwrap_err().code(),
                crate::error::codes::COMMIT_OUTCOME_UNKNOWN
            );
            assert_eq!(
                engine.try_stats().unwrap_err().code(),
                crate::error::codes::COMMIT_OUTCOME_UNKNOWN
            );
            assert!(engine
                .put("c", "another", Json::Null, PutOptions::default())
                .is_err());
            drop(engine);
            let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
            assert_eq!(
                reopened.get("c", "key").unwrap().value.as_str(),
                Some(expected_after_reopen)
            );
            drop(reopened);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn wal_append_faults_after_segment_rotation_preserve_commit_outcome_rules() {
        for (tag, fault, expected_after_reopen) in [
            ("partial", crate::wal::AppendFault::PartialWrite, "old"),
            ("pre-sync", crate::wal::AppendFault::BeforeSync, "new"),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "preppydb-wal-rotated-fault-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut config = EngineConfig::new(&dir);
            config.max_segment_bytes = 64;
            let engine = Engine::open(config).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .put("c", "key", Json::str("old"), PutOptions::default())
                .unwrap();

            let before_index = engine.lock_mutex(&engine.writer).current_index();
            engine.lock_mutex(&engine.writer).inject_fault(fault);
            let commit_error = engine
                .put("c", "key", Json::str("new"), PutOptions::default())
                .unwrap_err();
            assert_eq!(
                commit_error.code(),
                crate::error::codes::COMMIT_OUTCOME_UNKNOWN,
                "unexpected append error classification after rotation: {commit_error}"
            );
            assert!(
                engine.lock_mutex(&engine.writer).current_index() > before_index,
                "fault must occur in the rotated segment"
            );
            assert!(engine.get("c", "key").is_err());
            drop(engine);

            let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
            assert_eq!(
                reopened.get("c", "key").unwrap().value.as_str(),
                Some(expected_after_reopen),
                "recovery outcome after {tag} failure"
            );
            drop(reopened);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn wal_rotation_sync_failure_does_not_publish_a_new_segment() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-wal-rotation-sync-failure-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut config = EngineConfig::new(&dir);
        config.max_segment_bytes = 64;
        let engine = Engine::open(config).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        engine
            .put("c", "key", Json::str("old"), PutOptions::default())
            .unwrap();

        let current_index = engine.lock_mutex(&engine.writer).current_index();
        let next_segment = dir
            .join("wal")
            .join(crate::wal::segment_name(current_index + 1));
        assert!(!next_segment.exists());
        engine.lock_mutex(&engine.writer).inject_sync_failure();
        let error = engine
            .put("c", "key", Json::str("new"), PutOptions::default())
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::COMMIT_OUTCOME_UNKNOWN);
        assert_eq!(
            engine.lock_mutex(&engine.writer).current_index(),
            current_index,
            "failed rotation sync must retain the old writer"
        );
        assert!(
            !next_segment.exists(),
            "failed rotation must not publish a segment"
        );
        assert!(matches!(
            engine.get("c", "key"),
            Err(DbError::CommitOutcomeUnknown(_))
        ));
        drop(engine);

        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        assert_eq!(
            reopened.get("c", "key").unwrap().value.as_str(),
            Some("old")
        );
        assert!(!next_segment.exists());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_append_restores_insert_update_delete_and_index() {
        for operation in ["insert", "update", "delete"] {
            let dir = std::env::temp_dir().join(format!(
                "preppydb-undo-{operation}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .create_index(
                    "c",
                    IndexMeta {
                        name: "by_value".into(),
                        field: "value".into(),
                        unique: true,
                        sparse: false,
                        created_at: 0,
                    },
                )
                .unwrap();
            let old = Json::obj().with("value", Json::str("old"));
            engine
                .put("c", "existing", old.clone(), PutOptions::default())
                .unwrap();
            engine.lock_mutex(&engine.writer).inject_append_failure();
            let result = match operation {
                "insert" => engine
                    .put(
                        "c",
                        "new",
                        Json::obj().with("value", Json::str("new")),
                        PutOptions::default(),
                    )
                    .map(|_| ()),
                "update" => engine
                    .put(
                        "c",
                        "existing",
                        Json::obj().with("value", Json::str("new")),
                        PutOptions::default(),
                    )
                    .map(|_| ()),
                _ => engine.delete("c", "existing", None),
            };
            assert!(result.is_err());
            assert!(engine.get("c", "existing").is_err());
            {
                let state = engine.read();
                let collection = state.store.collection("c").unwrap();
                assert_eq!(collection.docs["existing"].value, old);
                assert!(!collection.docs.contains_key("new"));
                assert_eq!(
                    collection.indexes["by_value"]
                        .entries
                        .get(&Json::str("old").canonical_bytes())
                        .unwrap()
                        .len(),
                    1
                );
            }
            drop(engine);
            let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
            assert_eq!(reopened.get("c", "existing").unwrap().value, old);
            drop(reopened);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "preppydb-inventory-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn inventory_is_complete_deterministic_and_rebuilt_after_reopen() {
        let dir = test_dir("reopen");
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("profiles", 1))
            .unwrap();
        engine
            .create_collection(CollectionMeta::new("empty", 1))
            .unwrap();
        engine
            .put(
                "profiles",
                "guild-2",
                Json::obj().with("guild", Json::str("2")),
                PutOptions::default(),
            )
            .unwrap();
        engine
            .put(
                "profiles",
                "guild-1",
                Json::obj().with("guild", Json::str("1")),
                PutOptions::default(),
            )
            .unwrap();

        let first = engine.inventory_manifest().unwrap();
        let second = engine.inventory_manifest().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.collections[0].name, "empty");
        assert_eq!(first.collections[0].document_count, 0);
        assert_eq!(first.collections[1].name, "profiles");
        assert_eq!(first.collections[1].document_count, 2);
        assert_eq!(first.document_count, 2);
        assert_eq!(first.collection_count, 2);
        assert_eq!(first.sha256.len(), 64);

        let page = engine
            .inventory_page(first.sequence, first.generation, "profiles", None, 1)
            .unwrap();
        assert_eq!(page.records[0].key, "guild-1");
        assert!(page.next_cursor.is_some());
        assert_eq!(page.records[0].sha256.len(), 64);
        let second_page = engine
            .inventory_page(
                first.sequence,
                first.generation,
                "profiles",
                page.next_cursor.as_deref(),
                1,
            )
            .unwrap();
        assert_eq!(second_page.records[0].key, "guild-2");
        assert_eq!(second_page.next_cursor, None);

        engine.shutdown().unwrap();
        drop(engine);
        let reopened = Engine::open(EngineConfig::new(&dir)).unwrap();
        let after_restart = reopened.inventory_manifest().unwrap();
        assert_eq!(after_restart.document_count, 2);
        assert_eq!(after_restart.sha256, first.sha256);
        assert_eq!(
            reopened
                .inventory_page(
                    after_restart.sequence,
                    after_restart.generation,
                    "profiles",
                    None,
                    2
                )
                .unwrap()
                .records
                .len(),
            2
        );
        reopened.shutdown().unwrap();
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn inventory_pages_fail_closed_after_any_state_change() {
        let dir = test_dir("stale");
        let engine = Engine::open(EngineConfig::new(&dir)).unwrap();
        engine
            .create_collection(CollectionMeta::new("profiles", 1))
            .unwrap();
        engine
            .put(
                "profiles",
                "one",
                Json::obj().with("value", Json::int(1)),
                PutOptions::default(),
            )
            .unwrap();
        let manifest = engine.inventory_manifest().unwrap();
        engine
            .put(
                "profiles",
                "two",
                Json::obj().with("value", Json::int(2)),
                PutOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            engine.inventory_page(manifest.sequence, manifest.generation, "profiles", None, 10),
            Err(DbError::Conflict { .. })
        ));
        engine.shutdown().unwrap();
        drop(engine);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod wal_repair_tests {
    use super::*;

    #[test]
    fn failed_suffix_removal_leaves_repair_state_rejected_until_repaired_again() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-wal-repair-fault-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut config = EngineConfig::new(&dir);
        config.sync_mode = SyncMode::Always;
        config.max_segment_bytes = wal::WAL_HEADER_LEN + 1;
        {
            let engine = Engine::open(config.clone()).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .put("c", "baseline", Json::int(0), PutOptions::default())
                .unwrap();
            engine.checkpoint().unwrap();
            for index in 0..4 {
                engine
                    .put(
                        "c",
                        &format!("post-checkpoint-{index}"),
                        Json::int(index),
                        PutOptions::default(),
                    )
                    .unwrap();
            }
        }

        let segments = wal::list_segments(&dir.join("wal")).unwrap();
        assert!(
            segments.len() >= 4,
            "expected rotated segments: {segments:?}"
        );
        let damaged_segment = &segments[0];
        let mut bytes = std::fs::read(damaged_segment).unwrap();
        bytes[wal::WAL_HEADER_LEN as usize + 4] ^= 0x80;
        std::fs::write(damaged_segment, bytes).unwrap();
        let scan = wal::scan_segment(damaged_segment).unwrap();
        assert!(scan.corruption.is_some());
        assert_eq!(scan.valid_len, wal::WAL_HEADER_LEN);

        let mut directory_synced = false;
        let error = repair_corrupt_wal_suffix_with(
            damaged_segment,
            &segments[1..],
            scan.valid_len,
            |segment, valid_len| {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(segment)
                    .map_err(|err| DbError::io("opening corrupt WAL segment for repair", err))?;
                file.set_len(valid_len)
                    .map_err(|err| DbError::io("truncating corrupt WAL suffix", err))?;
                file.sync_all()
                    .map_err(|err| DbError::io("syncing repaired WAL segment", err))
            },
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_| {
                directory_synced = true;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("after corruption"));
        assert_eq!(
            std::fs::metadata(damaged_segment).unwrap().len(),
            scan.valid_len
        );
        assert!(segments[1..].iter().all(|segment| segment.exists()));
        assert!(!directory_synced, "failed repair must not sync as complete");
        assert!(matches!(
            Engine::open(config.clone()),
            Err(DbError::Corrupt { .. })
        ));

        config.repair = RepairPolicy::TruncateCorruptTail;
        let repaired = Engine::open(config.clone()).unwrap();
        assert_eq!(repaired.count("c").unwrap(), 1);
        assert_eq!(
            repaired.get("c", "baseline").unwrap().value.as_i64(),
            Some(0)
        );
        assert!(repaired
            .recovery_notes()
            .iter()
            .any(|note| note.kind == "corruption_truncated"));
        repaired
            .put("c", "after-repair", Json::int(9), PutOptions::default())
            .unwrap();
        drop(repaired);

        config.repair = RepairPolicy::Strict;
        let reopened = Engine::open(config).unwrap();
        assert_eq!(reopened.count("c").unwrap(), 2);
        assert_eq!(
            reopened.get("c", "after-repair").unwrap().value.as_i64(),
            Some(9)
        );
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_repair_directory_sync_returns_error_and_reopen_rechecks_barrier() {
        let dir = std::env::temp_dir().join(format!(
            "preppydb-wal-repair-directory-sync-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut config = EngineConfig::new(&dir);
        config.sync_mode = SyncMode::Always;
        config.max_segment_bytes = wal::WAL_HEADER_LEN + 1;
        {
            let engine = Engine::open(config.clone()).unwrap();
            engine
                .create_collection(CollectionMeta::new("c", 0))
                .unwrap();
            engine
                .put("c", "baseline", Json::int(0), PutOptions::default())
                .unwrap();
            engine.checkpoint().unwrap();
            for index in 0..3 {
                engine
                    .put(
                        "c",
                        &format!("post-checkpoint-{index}"),
                        Json::int(index),
                        PutOptions::default(),
                    )
                    .unwrap();
            }
        }

        let segments = wal::list_segments(&dir.join("wal")).unwrap();
        assert!(
            segments.len() >= 3,
            "expected rotated segments: {segments:?}"
        );
        let damaged_segment = &segments[0];
        let mut bytes = std::fs::read(damaged_segment).unwrap();
        bytes[wal::WAL_HEADER_LEN as usize + 4] ^= 0x80;
        std::fs::write(damaged_segment, bytes).unwrap();
        let scan = wal::scan_segment(damaged_segment).unwrap();
        assert!(scan.corruption.is_some());
        assert_eq!(scan.valid_len, wal::WAL_HEADER_LEN);

        let mut sync_attempts = 0;
        let error = repair_corrupt_wal_suffix_with(
            damaged_segment,
            &segments[1..],
            scan.valid_len,
            |segment, valid_len| {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(segment)
                    .map_err(|err| DbError::io("opening corrupt WAL segment for repair", err))?;
                file.set_len(valid_len)
                    .map_err(|err| DbError::io("truncating corrupt WAL suffix", err))?;
                file.sync_all()
                    .map_err(|err| DbError::io("syncing repaired WAL segment", err))
            },
            |path| std::fs::remove_file(path),
            |_| {
                sync_attempts += 1;
                Err(DbError::io(
                    "injected WAL repair directory sync failure",
                    std::io::Error::other("injected"),
                ))
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), crate::error::codes::IO_ERROR);
        assert_eq!(sync_attempts, 1);
        assert_eq!(
            std::fs::metadata(damaged_segment).unwrap().len(),
            wal::WAL_HEADER_LEN
        );
        assert!(segments[1..].iter().all(|segment| !segment.exists()));

        // Recovery must reach WalWriter::open, which repeats the directory
        // durability barrier before exposing an append-capable writer.
        let reopened = Engine::open(config.clone()).unwrap();
        assert_eq!(reopened.count("c").unwrap(), 1);
        assert_eq!(
            reopened.get("c", "baseline").unwrap().value.as_i64(),
            Some(0)
        );
        reopened
            .put("c", "after-repair", Json::int(9), PutOptions::default())
            .unwrap();
        drop(reopened);

        let verified = Engine::open(config).unwrap();
        assert_eq!(verified.count("c").unwrap(), 2);
        assert_eq!(
            verified.get("c", "after-repair").unwrap().value.as_i64(),
            Some(9)
        );
        drop(verified);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
