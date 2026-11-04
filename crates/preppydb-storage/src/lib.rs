//! PreppyDB storage engine.
//!
//! A persistent, single-node NoSQL store built around a checksummed write-ahead log,
//! periodic snapshots and an in-memory B-tree index.
//!
//! Design references (concepts, not code): Redis (logical command log, TTL, keyspace
//! organisation), WiredTiger/RocksDB (WAL + checkpoint, CRC per record), LMDB (single
//! writer, readers never block), FoundationDB (explicit transaction conflicts),
//! CouchDB (document-oriented JSON model), ScyllaDB (per-segment limits) and
//! CockroachDB (optimistic concurrency with a read set).
//!
//! What is **not** implemented here: replication, clustering, encryption at rest and
//! range partitioning. See `docs/ARCHITECTURE.md` and `docs/STATUS.md`.

#![forbid(unsafe_code)]

pub mod backup;
pub mod catalog;
pub mod clock;
pub mod engine;
pub mod error;
pub mod lock;
pub mod manifest;
pub mod record;
pub mod schema;
pub mod snapshot;
pub mod store;
pub mod txn;
pub mod wal;

pub use catalog::{CollectionMeta, IndexMeta};
pub use engine::{
    CheckedOp, CollectionStats, CompactReport, DataPaths, Engine, EngineConfig, EngineStats,
    PutOptions, RecoveryNote, RepairPolicy, ScanOptions, VerifyReport,
};
pub use error::{codes, DbError, Result};
pub use record::{Op, Record};
pub use store::{Doc, Limits};
pub use txn::{CommitOutcome, Transaction};
pub use wal::SyncMode;

/// Crate version, exposed for the health endpoint and backups.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
