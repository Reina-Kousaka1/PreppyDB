//! Optimistic multi-document transactions.
//!
//! Semantics (documented precisely because "transaction" means different things in
//! different databases):
//!
//! * Reads see the latest committed state (read committed) and every read version is
//!   remembered in a read set.
//! * Writes are buffered locally and are visible to subsequent reads inside the same
//!   transaction (read-your-writes).
//! * `commit` takes the engine write lock, re-checks every recorded version, and then
//!   appends **one** WAL record containing all writes. Readers cannot observe staged
//!   mutations before append succeeds. An I/O error after append begins can have an
//!   indeterminate recovery outcome: a complete frame may be replayed even though the
//!   caller did not receive success.
//! * If any version changed since it was read, the commit fails with
//!   `TransactionConflict` and the transaction is discarded. Callers may retry.
//!
//! There is no automatic retry: silent retries could turn a lost update into a
//! surprising success, so the decision stays with the client.

use crate::engine::Engine;
use crate::error::{DbError, Result};
use crate::record::Op;
use preppydb_json::Json;
use std::collections::BTreeMap;
use std::fmt::Write as FmtWrite;

/// Bound aggregate transaction staging below the WAL frame cap, leaving space for
/// record and operation metadata.
pub(crate) const MAX_TRANSACTION_BUFFERED_BYTES: usize = 32 * 1024 * 1024;
const TXN_OPERATION_OVERHEAD_BYTES: usize = 128;

struct BoundedSizeCounter {
    bytes: usize,
    limit: usize,
}

impl std::fmt::Write for BoundedSizeCounter {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let Some(next) = self.bytes.checked_add(value.len()) else {
            self.bytes = self.limit.saturating_add(1);
            return Err(std::fmt::Error);
        };
        if next > self.limit {
            self.bytes = self.limit.saturating_add(1);
            return Err(std::fmt::Error);
        }
        self.bytes = next;
        Ok(())
    }
}

/// Outcome of a committed transaction.
#[derive(Clone, Debug)]
pub struct CommitOutcome {
    pub seq: u64,
    pub puts: usize,
    pub deletes: usize,
}

/// A transaction handle.
pub struct Transaction<'a> {
    engine: &'a Engine,
    base_seq: u64,
    reads: Vec<(String, String, u64)>,
    /// Buffered writes: `None` = delete.
    writes: BTreeMap<(String, String), Option<Json>>,
    expires: BTreeMap<(String, String), Option<i64>>,
    write_sizes: BTreeMap<(String, String), usize>,
    buffered_bytes: usize,
    finished: bool,
    max_writes: usize,
    max_reads: usize,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(engine: &'a Engine) -> Transaction<'a> {
        let base_seq = engine.last_seq();
        let max_writes = engine.config().limits.max_batch_ops;
        let max_reads = max_writes;
        Transaction {
            engine,
            base_seq,
            reads: Vec::new(),
            writes: BTreeMap::new(),
            expires: BTreeMap::new(),
            write_sizes: BTreeMap::new(),
            buffered_bytes: 0,
            finished: false,
            max_writes,
            max_reads,
        }
    }

    pub fn base_seq(&self) -> u64 {
        self.base_seq
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn check_open(&self) -> Result<()> {
        if self.finished {
            return Err(DbError::Internal(
                "transaction has already been committed or rolled back".to_string(),
            ));
        }
        Ok(())
    }

    fn record_read(&mut self, collection: &str, key: &str, version: u64) -> Result<()> {
        if self
            .reads
            .iter()
            .any(|(c, k, _)| c == collection && k == key)
        {
            return Ok(());
        }
        if self.reads.len() >= self.max_reads {
            return Err(DbError::LimitExceeded {
                limit: "max_txn_reads",
                max: self.max_reads as u64,
                actual: self.reads.len() as u64 + 1,
            });
        }
        self.reads
            .push((collection.to_string(), key.to_string(), version));
        Ok(())
    }

    fn staged_write_size(
        collection: &str,
        key: &str,
        value: Option<&Json>,
        budget: usize,
    ) -> Result<usize> {
        let metadata_bytes = collection
            .len()
            .checked_add(key.len())
            .and_then(|size| size.checked_add(TXN_OPERATION_OVERHEAD_BYTES))
            .ok_or_else(|| Self::txn_bytes_limit_error(u64::MAX))?;
        let available_for_value = budget.checked_sub(metadata_bytes).ok_or_else(|| {
            Self::txn_bytes_limit_error(MAX_TRANSACTION_BUFFERED_BYTES as u64 + 1)
        })?;
        let value_bytes = if let Some(value) = value {
            let mut counter = BoundedSizeCounter {
                bytes: 0,
                limit: available_for_value,
            };
            if write!(&mut counter, "{value}").is_err() {
                return Err(Self::txn_bytes_limit_error(
                    MAX_TRANSACTION_BUFFERED_BYTES as u64 + 1,
                ));
            }
            counter.bytes
        } else {
            0
        };
        metadata_bytes
            .checked_add(value_bytes)
            .ok_or_else(|| Self::txn_bytes_limit_error(u64::MAX))
    }

    fn txn_bytes_limit_error(actual: u64) -> DbError {
        DbError::LimitExceeded {
            limit: "max_txn_bytes",
            max: MAX_TRANSACTION_BUFFERED_BYTES as u64,
            actual,
        }
    }

    fn reserve_write_size(&mut self, entry: &(String, String), new_size: usize) -> Result<()> {
        let previous = self.write_sizes.get(entry).copied().unwrap_or(0);
        let next = self
            .buffered_bytes
            .checked_sub(previous)
            .and_then(|size| size.checked_add(new_size))
            .ok_or(DbError::LimitExceeded {
                limit: "max_txn_bytes",
                max: MAX_TRANSACTION_BUFFERED_BYTES as u64,
                actual: u64::MAX,
            })?;
        if next > MAX_TRANSACTION_BUFFERED_BYTES {
            return Err(DbError::LimitExceeded {
                limit: "max_txn_bytes",
                max: MAX_TRANSACTION_BUFFERED_BYTES as u64,
                actual: next as u64,
            });
        }
        self.buffered_bytes = next;
        self.write_sizes.insert(entry.clone(), new_size);
        Ok(())
    }

    /// Read a document inside the transaction (sees buffered writes).
    pub fn get(&mut self, collection: &str, key: &str) -> Result<Option<Json>> {
        self.check_open()?;
        let entry = (collection.to_string(), key.to_string());
        if let Some(buffered) = self.writes.get(&entry) {
            return Ok(buffered.clone());
        }
        let current = self.engine.get_opt(collection, key)?;
        let version = current.as_ref().map(|doc| doc.version).unwrap_or(0);
        self.record_read(collection, key, version)?;
        Ok(current.map(|doc| doc.value))
    }

    /// Buffer a write.
    pub fn put(
        &mut self,
        collection: &str,
        key: &str,
        value: Json,
        expires_at: Option<i64>,
    ) -> Result<()> {
        self.check_open()?;
        let entry = (collection.to_string(), key.to_string());
        if self.writes.len() >= self.max_writes && !self.writes.contains_key(&entry) {
            return Err(DbError::LimitExceeded {
                limit: "max_txn_writes",
                max: self.max_writes as u64,
                actual: self.writes.len() as u64 + 1,
            });
        }
        let previous_size = self.write_sizes.get(&entry).copied().unwrap_or(0);
        let current_bytes = self
            .buffered_bytes
            .checked_sub(previous_size)
            .ok_or_else(|| Self::txn_bytes_limit_error(u64::MAX))?;
        let size = Self::staged_write_size(
            collection,
            key,
            Some(&value),
            MAX_TRANSACTION_BUFFERED_BYTES - current_bytes,
        )?;
        if !self.writes.contains_key(&entry) {
            // First touch: remember the committed version so a concurrent writer is
            // detected at commit time even for blind writes.
            let current = self.engine.get_opt(collection, key)?;
            let version = current.as_ref().map(|doc| doc.version).unwrap_or(0);
            self.record_read(collection, key, version)?;
        }
        self.reserve_write_size(&entry, size)?;
        self.writes.insert(entry.clone(), Some(value));
        self.expires.insert(entry, expires_at);
        Ok(())
    }

    /// Buffer a delete.
    pub fn delete(&mut self, collection: &str, key: &str) -> Result<()> {
        self.check_open()?;
        let entry = (collection.to_string(), key.to_string());
        if self.writes.len() >= self.max_writes && !self.writes.contains_key(&entry) {
            return Err(DbError::LimitExceeded {
                limit: "max_txn_writes",
                max: self.max_writes as u64,
                actual: self.writes.len() as u64 + 1,
            });
        }
        let previous_size = self.write_sizes.get(&entry).copied().unwrap_or(0);
        let next_bytes = self
            .buffered_bytes
            .checked_sub(previous_size)
            .ok_or_else(|| Self::txn_bytes_limit_error(u64::MAX))?;
        let size = Self::staged_write_size(
            collection,
            key,
            None,
            MAX_TRANSACTION_BUFFERED_BYTES - next_bytes,
        )?;
        if !self.writes.contains_key(&entry) {
            let current = self.engine.get_opt(collection, key)?;
            let version = current.as_ref().map(|doc| doc.version).unwrap_or(0);
            self.record_read(collection, key, version)?;
        }
        self.reserve_write_size(&entry, size)?;
        self.writes.insert(entry.clone(), None);
        self.expires.remove(&entry);
        Ok(())
    }

    /// Number of buffered writes.
    pub fn len(&self) -> usize {
        self.writes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Commit atomically. Consumes the transaction.
    pub fn commit(mut self) -> Result<CommitOutcome> {
        self.check_open()?;
        self.finished = true;
        if self.writes.is_empty() {
            return Ok(CommitOutcome {
                seq: self.engine.last_seq(),
                puts: 0,
                deletes: 0,
            });
        }
        let mut ops = Vec::with_capacity(self.writes.len());
        let mut puts = 0;
        let mut deletes = 0;
        let mut expires = std::mem::take(&mut self.expires);
        for ((collection, key), value) in std::mem::take(&mut self.writes) {
            match value {
                Some(value) => {
                    puts += 1;
                    let expires_at = expires.remove(&(collection.clone(), key.clone())).flatten();
                    ops.push(Op::Put {
                        collection,
                        key,
                        value,
                        expires_at,
                    });
                }
                None => {
                    deletes += 1;
                    ops.push(Op::Delete { collection, key });
                }
            }
        }
        let seq = self.engine.commit_transaction(&self.reads, ops)?;
        Ok(CommitOutcome { seq, puts, deletes })
    }

    /// Discard all buffered writes.
    pub fn rollback(mut self) {
        self.finished = true;
        self.writes.clear();
        self.expires.clear();
        self.write_sizes.clear();
        self.buffered_bytes = 0;
        self.reads.clear();
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // Nothing to undo: writes are only ever applied inside `commit`.
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CollectionMeta;
    use crate::engine::{EngineConfig, PutOptions};
    use preppydb_json::Json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_engine(tag: &str) -> (PathBuf, Engine) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "preppydb-txn-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = Engine::open(EngineConfig::new(&path)).unwrap();
        engine
            .create_collection(CollectionMeta::new("c", 0))
            .unwrap();
        (path, engine)
    }

    #[test]
    fn read_set_is_bounded_by_configured_batch_operation_limit() {
        let (path, engine) = test_engine("read-bound");
        let mut txn = engine.begin();
        for index in 0..txn.max_reads {
            assert!(txn.get("c", &format!("missing-{index}")).unwrap().is_none());
        }
        assert!(matches!(
            txn.get("c", "one-too-many"),
            Err(DbError::LimitExceeded {
                limit: "max_txn_reads",
                ..
            })
        ));
        txn.rollback();
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn write_buffer_bytes_are_bounded_and_replacements_release_budget() {
        let (path, engine) = test_engine("write-byte-bound");
        let mut txn = engine.begin();
        let large = "x".repeat(16 * 1024 * 1024);
        txn.put("c", "first", Json::str(large.clone()), None)
            .unwrap();
        assert!(matches!(
            txn.put("c", "second", Json::str(large.clone()), None),
            Err(DbError::LimitExceeded {
                limit: "max_txn_bytes",
                ..
            })
        ));
        txn.put("c", "first", Json::str("small"), None).unwrap();
        txn.put("c", "second", Json::str(large), None).unwrap();
        assert!(txn.buffered_bytes < MAX_TRANSACTION_BUFFERED_BYTES);
        txn.rollback();
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn escaped_json_size_is_bounded_before_it_can_exceed_the_wal_budget() {
        let (path, engine) = test_engine("escaped-byte-bound");
        let mut txn = engine.begin();
        let escaped = "\n".repeat(17 * 1024 * 1024);
        assert!(matches!(
            txn.put("c", "escaped", Json::str(escaped), None),
            Err(DbError::LimitExceeded {
                limit: "max_txn_bytes",
                ..
            })
        ));
        assert!(txn.is_empty());
        assert_eq!(txn.buffered_bytes, 0);
        txn.rollback();
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn transaction_commit_moves_staged_values_and_preserves_expiry() {
        let (path, engine) = test_engine("commit-move");
        let mut txn = engine.begin();
        txn.put("c", "key", Json::str("value"), Some(4_000_000_000_000))
            .unwrap();
        let result = txn.commit().unwrap();
        assert_eq!(result.puts, 1);
        assert_eq!(
            engine.get("c", "key").unwrap().expires_at,
            Some(4_000_000_000_000)
        );
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn engine_api_remains_available_for_puts_after_a_rolled_back_buffer() {
        let (path, engine) = test_engine("rollback");
        let mut txn = engine.begin();
        txn.put("c", "key", Json::str("value"), None).unwrap();
        txn.rollback();
        engine
            .put("c", "key", Json::str("committed"), PutOptions::default())
            .unwrap();
        assert_eq!(
            engine.get("c", "key").unwrap().value.as_str(),
            Some("committed")
        );
        drop(engine);
        std::fs::remove_dir_all(path).unwrap();
    }
}
