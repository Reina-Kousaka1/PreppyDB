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
//!   appends **one** WAL record containing all writes. Either every write becomes
//!   durable or none of them does.
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
    finished: bool,
    max_writes: usize,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(engine: &'a Engine) -> Transaction<'a> {
        let base_seq = engine.last_seq();
        let max_writes = engine.config().limits.max_batch_ops;
        Transaction {
            engine,
            base_seq,
            reads: Vec::new(),
            writes: BTreeMap::new(),
            expires: BTreeMap::new(),
            finished: false,
            max_writes,
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

    fn record_read(&mut self, collection: &str, key: &str, version: u64) {
        let entry = (collection.to_string(), key.to_string());
        if !self
            .reads
            .iter()
            .any(|(c, k, _)| c == &entry.0 && k == &entry.1)
        {
            self.reads.push((entry.0, entry.1, version));
        }
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
        self.record_read(collection, key, version);
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
        if !self.writes.contains_key(&entry) {
            // First touch: remember the committed version so a concurrent writer is
            // detected at commit time even for blind writes.
            let current = self.engine.get_opt(collection, key)?;
            let version = current.as_ref().map(|doc| doc.version).unwrap_or(0);
            self.record_read(collection, key, version);
        }
        if self.writes.len() >= self.max_writes && !self.writes.contains_key(&entry) {
            return Err(DbError::LimitExceeded {
                limit: "max_txn_writes",
                max: self.max_writes as u64,
                actual: self.writes.len() as u64 + 1,
            });
        }
        self.writes.insert(entry.clone(), Some(value));
        self.expires.insert(entry, expires_at);
        Ok(())
    }

    /// Buffer a delete.
    pub fn delete(&mut self, collection: &str, key: &str) -> Result<()> {
        self.check_open()?;
        let entry = (collection.to_string(), key.to_string());
        if !self.writes.contains_key(&entry) {
            let current = self.engine.get_opt(collection, key)?;
            let version = current.as_ref().map(|doc| doc.version).unwrap_or(0);
            self.record_read(collection, key, version);
        }
        self.writes.insert(entry, None);
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
        for ((collection, key), value) in &self.writes {
            match value {
                Some(value) => {
                    puts += 1;
                    ops.push(Op::Put {
                        collection: collection.clone(),
                        key: key.clone(),
                        value: value.clone(),
                        expires_at: self
                            .expires
                            .get(&(collection.clone(), key.clone()))
                            .copied()
                            .flatten(),
                    });
                }
                None => {
                    deletes += 1;
                    ops.push(Op::Delete {
                        collection: collection.clone(),
                        key: key.clone(),
                    });
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
        self.reads.clear();
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // Nothing to undo: writes are only ever applied inside `commit`.
        self.finished = true;
    }
}
