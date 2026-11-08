//! In-memory state: collections, documents, secondary indexes and their invariants.
//!
//! All persistence happens through the WAL; this module is the deterministic state
//! machine that the WAL records drive. It contains no I/O so it can be unit tested in
//! isolation and replayed identically during recovery.

use crate::catalog::{CollectionMeta, IndexMeta};
use crate::error::{DbError, Result};
use crate::record::Op;
use crate::schema;
use preppydb_json::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Maximum serialized old-document data retained for rollback by one batch.
pub(crate) const MAX_BATCH_UNDO_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn reserve_undo_bytes(current: usize, additional: usize) -> Result<usize> {
    let next = current
        .checked_add(additional)
        .ok_or(DbError::LimitExceeded {
            limit: "batch_undo_bytes",
            max: MAX_BATCH_UNDO_BYTES as u64,
            actual: u64::MAX,
        })?;
    if next > MAX_BATCH_UNDO_BYTES {
        return Err(DbError::LimitExceeded {
            limit: "batch_undo_bytes",
            max: MAX_BATCH_UNDO_BYTES as u64,
            actual: u64::try_from(next).unwrap_or(u64::MAX),
        });
    }
    Ok(next)
}

/// A stored document together with its version metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub value: Json,
    /// Per-document version, incremented on every successful write (starts at 1).
    pub version: u64,
    /// Global log sequence number of the write that produced this version.
    pub revision: u64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Absolute expiry (ms since epoch). Expired documents are invisible to readers.
    pub expires_at: Option<i64>,
}

impl Doc {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("value", self.value.clone())
            .with("version", Json::uint(self.version))
            .with("revision", Json::uint(self.revision))
            .with("created_at", Json::int(self.created_at))
            .with("updated_at", Json::int(self.updated_at))
            .with(
                "expires_at",
                self.expires_at.map(Json::int).unwrap_or(Json::Null),
            )
    }

    pub fn from_json(value: &Json) -> Result<Doc> {
        let doc = value
            .get("value")
            .cloned()
            .ok_or_else(|| DbError::corrupt("snapshot", "document is missing 'value'"))?;
        let version = match value.get("version") {
            None => 1,
            Some(value) => value
                .as_u64()
                .filter(|version| *version > 0)
                .ok_or_else(|| DbError::corrupt("snapshot", "invalid document version"))?,
        };
        let revision = match value.get("revision") {
            None => 0,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| DbError::corrupt("snapshot", "invalid document revision"))?,
        };
        let created_at = match value.get("created_at") {
            None => 0,
            Some(value) => value
                .as_i64()
                .ok_or_else(|| DbError::corrupt("snapshot", "invalid document created_at"))?,
        };
        let updated_at = match value.get("updated_at") {
            None => 0,
            Some(value) => value
                .as_i64()
                .ok_or_else(|| DbError::corrupt("snapshot", "invalid document updated_at"))?,
        };
        let expires_at = match value.get("expires_at") {
            None | Some(Json::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| DbError::corrupt("snapshot", "invalid document expires_at"))?,
            ),
        };
        Ok(Doc {
            value: doc,
            version,
            revision,
            created_at,
            updated_at,
            expires_at,
        })
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        matches!(self.expires_at, Some(at) if at <= now_ms)
    }
}

/// One secondary index with its in-memory `value -> keys` map.
#[derive(Clone, Debug)]
pub struct IndexState {
    pub meta: IndexMeta,
    pub entries: BTreeMap<Vec<u8>, BTreeSet<Arc<str>>>,
}

impl IndexState {
    pub fn new(meta: IndexMeta) -> IndexState {
        IndexState {
            meta,
            entries: BTreeMap::new(),
        }
    }

    fn validate_insert(&self, doc_key: &str, doc: &Doc) -> Result<()> {
        let value = match doc.value.get_path(&self.meta.field) {
            Some(value) => value,
            None => {
                if self.meta.sparse {
                    return Ok(());
                }
                &Json::Null
            }
        };
        let encoded = value.canonical_bytes();
        if let Some(bucket) = self
            .entries
            .get(&encoded)
            .filter(|bucket| self.meta.unique && !bucket.is_empty() && !bucket.contains(doc_key))
        {
            return Err(DbError::validation(format!(
                "unique index '{}' already contains value {} for key '{}'",
                self.meta.name,
                value,
                bucket.iter().next().map(AsRef::as_ref).unwrap_or("")
            )));
        }
        Ok(())
    }

    fn insert(&mut self, doc_key: &Arc<str>, doc: &Doc) -> Result<()> {
        self.validate_insert(doc_key, doc)?;
        let value = match doc.value.get_path(&self.meta.field) {
            Some(value) => value,
            None if self.meta.sparse => return Ok(()),
            None => &Json::Null,
        };
        let bucket = self.entries.entry(value.canonical_bytes()).or_default();
        bucket.insert(Arc::clone(doc_key));
        Ok(())
    }

    fn remove(&mut self, doc_key: &Arc<str>, doc: &Doc) {
        let value = match doc.value.get_path(&self.meta.field) {
            Some(value) => value,
            None if self.meta.sparse => return,
            None => &Json::Null,
        };
        {
            let encoded = value.canonical_bytes();
            if let Some(bucket) = self.entries.get_mut(&encoded) {
                bucket.remove(doc_key.as_ref());
                if bucket.is_empty() {
                    self.entries.remove(&encoded);
                }
            }
        }
    }
}

/// A collection: metadata, documents and maintained indexes.
#[derive(Clone, Debug)]
pub struct Collection {
    pub meta: CollectionMeta,
    pub docs: BTreeMap<Arc<str>, Doc>,
    pub indexes: BTreeMap<String, IndexState>,
}

impl Collection {
    pub fn new(meta: CollectionMeta) -> Collection {
        Self::new_with_unique_indexes_only(meta, false)
    }

    fn new_with_unique_indexes_only(meta: CollectionMeta, unique_only: bool) -> Collection {
        let mut indexes = BTreeMap::new();
        for index in meta.indexes.values() {
            if !unique_only || index.unique {
                indexes.insert(index.name.clone(), IndexState::new(index.clone()));
            }
        }
        Collection {
            meta,
            docs: BTreeMap::new(),
            indexes,
        }
    }

    /// Rebuild all index maps from the current documents (used after loading a
    /// snapshot, which stores documents but not index structures).
    pub fn rebuild_indexes(&mut self) -> Result<()> {
        self.rebuild_indexes_with_unique_only(false)
    }

    pub(crate) fn rebuild_indexes_with_unique_only(&mut self, unique_only: bool) -> Result<()> {
        let mut indexes = BTreeMap::new();
        for index in self.meta.indexes.values() {
            if !unique_only || index.unique {
                indexes.insert(index.name.clone(), IndexState::new(index.clone()));
            }
        }
        for (key, doc) in &self.docs {
            Self::index_document_into(&mut indexes, key, doc)?;
        }
        self.indexes = indexes;
        Ok(())
    }

    fn index_document(&mut self, key: &Arc<str>, doc: &Doc) -> Result<()> {
        Self::index_document_into(&mut self.indexes, key, doc)
    }

    fn validate_index_document(&self, key: &Arc<str>, doc: &Doc) -> Result<()> {
        for index in self.indexes.values() {
            index.validate_insert(key, doc)?;
        }
        Ok(())
    }

    fn index_document_into(
        indexes: &mut BTreeMap<String, IndexState>,
        key: &Arc<str>,
        doc: &Doc,
    ) -> Result<()> {
        // Validate every index before modifying any, so a later unique conflict
        // cannot leave entries in earlier indexes.
        for index in indexes.values() {
            index.validate_insert(key, doc)?;
        }
        for index in indexes.values_mut() {
            index.insert(key, doc)?;
        }
        Ok(())
    }

    fn unindex_document(&mut self, key: &Arc<str>, doc: &Doc) {
        for index in self.indexes.values_mut() {
            index.remove(key, doc);
        }
    }

    pub fn live_len(&self, now_ms: i64) -> usize {
        self.docs
            .values()
            .filter(|doc| !doc.is_expired(now_ms))
            .count()
    }
}

/// Resource limits enforced by the engine.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_collections: usize,
    pub max_docs_per_collection: u64,
    pub max_doc_bytes: u64,
    pub max_key_len: usize,
    pub max_batch_ops: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_collections: 256,
            max_docs_per_collection: 5_000_000,
            max_doc_bytes: 4 * 1024 * 1024,
            max_key_len: 512,
            max_batch_ops: 1000,
        }
    }
}

/// How an operation is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyMode {
    /// Live write: full validation (schema, limits, unique indexes).
    Live,
    /// Recovery replay: records were validated when they were first written.
    Replay,
}

/// Outcome counters of one applied operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub puts: usize,
    pub deletes: usize,
    pub collections_created: usize,
    pub collections_dropped: usize,
}

/// Reversible state captured for one applied operation. Collections and indexes are
/// moved into this journal when removed, so rollback does not duplicate their contents.
pub(crate) enum StoreUndo {
    None,
    Document {
        collection: String,
        key: String,
        previous: Option<Doc>,
    },
    RemoveCollection(String),
    RestoreCollection(Box<Collection>),
    RemoveIndex {
        collection: String,
        name: String,
    },
    RestoreIndex {
        collection: String,
        name: String,
        meta: Option<IndexMeta>,
        state: Option<IndexState>,
    },
    Sequence(Vec<StoreUndo>),
}

/// The complete in-memory database state.
#[derive(Clone, Debug, Default)]
pub struct Store {
    pub collections: BTreeMap<String, Collection>,
    pub limits: Limits,
}

impl Store {
    pub fn new(limits: Limits) -> Store {
        Store {
            collections: BTreeMap::new(),
            limits,
        }
    }

    pub fn collection(&self, name: &str) -> Result<&Collection> {
        self.collections
            .get(name)
            .ok_or_else(|| DbError::CollectionNotFound(name.to_string()))
    }

    pub fn collection_mut(&mut self, name: &str) -> Result<&mut Collection> {
        self.collections
            .get_mut(name)
            .ok_or_else(|| DbError::CollectionNotFound(name.to_string()))
    }

    /// Restore one document and its indexes after an unsuccessful WAL append.
    /// The caller must hold the engine write lock throughout mutation and undo.
    pub(crate) fn restore_document(
        &mut self,
        collection: &str,
        key: &str,
        previous: Option<Doc>,
    ) -> Result<()> {
        let coll = self.collection_mut(collection)?;
        if let Some((stored_key, current)) = coll.docs.remove_entry(key) {
            coll.unindex_document(&stored_key, &current);
        }
        if let Some(doc) = previous {
            let stored_key = Arc::<str>::from(key);
            coll.index_document(&stored_key, &doc)?;
            coll.docs.insert(stored_key, doc);
        }
        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.collections.contains_key(name)
    }

    /// Create a collection (idempotent only when the definition is identical).
    pub fn create_collection(&mut self, meta: CollectionMeta, mode: ApplyMode) -> Result<()> {
        self.create_collection_with_index_policy(meta, mode, false)
    }

    pub(crate) fn create_collection_for_verification(
        &mut self,
        meta: CollectionMeta,
    ) -> Result<()> {
        self.create_collection_with_index_policy(meta, ApplyMode::Replay, true)
    }

    fn create_collection_with_index_policy(
        &mut self,
        meta: CollectionMeta,
        mode: ApplyMode,
        unique_only: bool,
    ) -> Result<()> {
        if self.collections.contains_key(&meta.name) {
            return match mode {
                ApplyMode::Replay => Ok(()),
                ApplyMode::Live => Err(DbError::CollectionExists(meta.name)),
            };
        }
        if mode == ApplyMode::Live && self.collections.len() >= self.limits.max_collections {
            return Err(DbError::LimitExceeded {
                limit: "max_collections",
                max: self.limits.max_collections as u64,
                actual: self.collections.len() as u64 + 1,
            });
        }
        if let Some(schema) = &meta.schema {
            schema::validate_schema(schema, &meta.name, 0)?;
        }
        let collection = Collection::new_with_unique_indexes_only(meta.clone(), unique_only);
        self.collections.insert(meta.name.clone(), collection);
        Ok(())
    }

    pub fn drop_collection(&mut self, name: &str, mode: ApplyMode) -> Result<()> {
        match self.collections.remove(name) {
            Some(_) => Ok(()),
            None if mode == ApplyMode::Replay => Ok(()),
            None => Err(DbError::CollectionNotFound(name.to_string())),
        }
    }

    pub fn create_index(
        &mut self,
        collection: &str,
        index: IndexMeta,
        mode: ApplyMode,
    ) -> Result<()> {
        self.create_index_with_index_policy(collection, index, mode, false)
    }

    fn create_index_with_index_policy(
        &mut self,
        collection: &str,
        index: IndexMeta,
        mode: ApplyMode,
        unique_only: bool,
    ) -> Result<()> {
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok(()),
            None => return Err(DbError::CollectionNotFound(collection.to_string())),
        };
        if coll.meta.indexes.contains_key(&index.name) {
            return match mode {
                ApplyMode::Replay => Ok(()),
                ApplyMode::Live => Err(DbError::IndexExists {
                    collection: collection.to_string(),
                    index: index.name.clone(),
                }),
            };
        }
        let name = index.name.clone();
        if unique_only && !index.unique {
            coll.meta.indexes.insert(name, index);
            return Ok(());
        }
        let mut state = IndexState::new(index.clone());
        for (key, doc) in &coll.docs {
            state.insert(key, doc)?;
        }
        coll.meta.indexes.insert(name.clone(), index);
        coll.indexes.insert(name, state);
        Ok(())
    }

    pub fn drop_index(&mut self, collection: &str, name: &str, mode: ApplyMode) -> Result<()> {
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok(()),
            None => return Err(DbError::CollectionNotFound(collection.to_string())),
        };
        if coll.meta.indexes.remove(name).is_none() && mode == ApplyMode::Live {
            return Err(DbError::IndexNotFound {
                collection: collection.to_string(),
                index: name.to_string(),
            });
        }
        coll.indexes.remove(name);
        Ok(())
    }

    /// Write a document, enforcing limits, schema and unique indexes.
    ///
    /// `expected_version` implements compare-and-set semantics: `Some(0)` means
    /// "must not exist", `Some(n)` means "must currently be version n", `None` means
    /// unconditional.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &mut self,
        collection: &str,
        key: &str,
        value: Json,
        expires_at: Option<i64>,
        expected_version: Option<u64>,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
    ) -> Result<u64> {
        self.put_with_undo(
            collection,
            key,
            value,
            expires_at,
            expected_version,
            now_ms,
            revision,
            mode,
        )
        .map(|(version, _previous)| version)
    }

    /// Apply a put and return the displaced document for an in-memory rollback.
    /// Moving the old value out avoids a full document clone on the engine's
    /// single-document WAL path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn put_with_undo(
        &mut self,
        collection: &str,
        key: &str,
        value: Json,
        expires_at: Option<i64>,
        expected_version: Option<u64>,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
    ) -> Result<(u64, Option<Doc>)> {
        validate_key(key, self.limits.max_key_len)?;
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok((0, None)),
            None => return Err(DbError::CollectionNotFound(collection.to_string())),
        };
        if mode == ApplyMode::Live {
            if let Some(schema) = &coll.meta.schema {
                schema::validate_value(&value, schema)?;
            }
            let encoded_len = value.to_string().len() as u64;
            let max_bytes = coll.meta.max_doc_bytes.unwrap_or(self.limits.max_doc_bytes);
            if encoded_len > max_bytes {
                return Err(DbError::LimitExceeded {
                    limit: "max_doc_bytes",
                    max: max_bytes,
                    actual: encoded_len,
                });
            }
        }
        let (stored_key, existing_version, created_at) = match coll.docs.get_key_value(key) {
            Some((stored_key, doc)) => (Arc::clone(stored_key), Some(doc.version), doc.created_at),
            None => (Arc::<str>::from(key), None, now_ms),
        };
        if mode == ApplyMode::Live {
            match expected_version {
                Some(0) if existing_version.is_some() => {
                    return Err(DbError::Conflict {
                        collection: collection.to_string(),
                        key: key.to_string(),
                        expected: Some(0),
                        actual: existing_version,
                    })
                }
                Some(expected) if expected > 0 => {
                    let actual = existing_version.unwrap_or(0);
                    if actual != expected {
                        return Err(DbError::Conflict {
                            collection: collection.to_string(),
                            key: key.to_string(),
                            expected: Some(expected),
                            actual: existing_version,
                        });
                    }
                }
                _ => {}
            }
            if existing_version.is_none() {
                let max_docs = coll
                    .meta
                    .max_docs
                    .unwrap_or(self.limits.max_docs_per_collection);
                if coll.docs.len() as u64 >= max_docs {
                    return Err(DbError::LimitExceeded {
                        limit: "max_docs_per_collection",
                        max: max_docs,
                        actual: coll.docs.len() as u64 + 1,
                    });
                }
            }
        }
        let version = match existing_version {
            Some(current) => current.checked_add(1).ok_or(DbError::LimitExceeded {
                limit: "document_version",
                max: u64::MAX,
                actual: u64::MAX,
            })?,
            None => 1,
        };
        let expires_at = match (expires_at, coll.meta.default_ttl_ms) {
            (Some(expires_at), _) => Some(expires_at),
            (None, Some(ttl)) => Some(
                now_ms
                    .checked_add(ttl)
                    .ok_or_else(|| DbError::validation("default TTL overflows timestamp"))?,
            ),
            (None, None) => None,
        };
        let doc = Doc {
            value,
            version,
            revision,
            created_at,
            updated_at: now_ms,
            expires_at,
        };
        // Validate before removing the old document. This lets the update path
        // avoid cloning the prior document while preserving failed-write atomicity.
        coll.validate_index_document(&stored_key, &doc)?;
        let previous = coll.docs.remove_entry(key).map(|(stored, old)| {
            coll.unindex_document(&stored, &old);
            old
        });
        // A second validation cannot fail after the same key was removed; keep a
        // defensive restoration path so the old value survives any future change.
        if let Err(err) = coll.index_document(&stored_key, &doc) {
            if let Some(old) = previous {
                let _ = coll.index_document(&stored_key, &old);
                coll.docs.insert(Arc::clone(&stored_key), old);
            }
            return Err(err);
        }
        coll.docs.insert(stored_key, doc);
        Ok((version, previous))
    }

    pub fn delete(
        &mut self,
        collection: &str,
        key: &str,
        expected_version: Option<u64>,
        mode: ApplyMode,
    ) -> Result<bool> {
        self.delete_with_undo(collection, key, expected_version, mode)
            .map(|(deleted, _previous)| deleted)
    }

    /// Apply a delete and return the removed document for an in-memory rollback.
    pub(crate) fn delete_with_undo(
        &mut self,
        collection: &str,
        key: &str,
        expected_version: Option<u64>,
        mode: ApplyMode,
    ) -> Result<(bool, Option<Doc>)> {
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok((false, None)),
            None => return Err(DbError::CollectionNotFound(collection.to_string())),
        };
        let actual_version = coll.docs.get(key).map(|doc| doc.version);
        if mode == ApplyMode::Live {
            match (expected_version, actual_version) {
                (None, None) => {
                    return Err(DbError::NotFound {
                        collection: collection.to_string(),
                        key: key.to_string(),
                    })
                }
                (Some(expected), Some(actual)) if expected != actual => {
                    return Err(DbError::Conflict {
                        collection: collection.to_string(),
                        key: key.to_string(),
                        expected: Some(expected),
                        actual: Some(actual),
                    })
                }
                (Some(_), None) => {
                    return Err(DbError::Conflict {
                        collection: collection.to_string(),
                        key: key.to_string(),
                        expected: expected_version,
                        actual: None,
                    })
                }
                _ => {}
            }
        }
        if let Some((stored_key, removed)) = coll.docs.remove_entry(key) {
            coll.unindex_document(&stored_key, &removed);
            Ok((true, Some(removed)))
        } else {
            Ok((false, None))
        }
    }

    /// Apply one logical operation. `revision` is the sequence number of the record.
    pub fn apply(
        &mut self,
        op: &Op,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
    ) -> Result<ApplyStats> {
        self.apply_with_index_policy(op, now_ms, revision, mode, false)
    }

    pub(crate) fn apply_for_verification(
        &mut self,
        op: &Op,
        now_ms: i64,
        revision: u64,
    ) -> Result<ApplyStats> {
        self.apply_with_index_policy(op, now_ms, revision, ApplyMode::Replay, true)
    }

    fn apply_with_index_policy(
        &mut self,
        op: &Op,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
        unique_only: bool,
    ) -> Result<ApplyStats> {
        let mut stats = ApplyStats::default();
        match op {
            Op::Put {
                collection,
                key,
                value,
                expires_at,
            } => {
                self.put(
                    collection,
                    key,
                    value.clone(),
                    *expires_at,
                    None,
                    now_ms,
                    revision,
                    mode,
                )?;
                stats.puts += 1;
            }
            Op::Delete { collection, key } => {
                self.delete(collection, key, None, mode)?;
                stats.deletes += 1;
            }
            Op::CreateCollection { meta } => {
                self.create_collection_with_index_policy(meta.clone(), mode, unique_only)?;
                stats.collections_created += 1;
            }
            Op::DropCollection { collection } => {
                self.drop_collection(collection, mode)?;
                stats.collections_dropped += 1;
            }
            Op::CreateIndex { collection, index } => {
                self.create_index_with_index_policy(collection, index.clone(), mode, unique_only)?;
            }
            Op::DropIndex { collection, name } => {
                self.drop_index(collection, name, mode)?;
            }
            Op::Batch { ops, .. } => {
                if ops.len() > self.limits.max_batch_ops && mode == ApplyMode::Live {
                    return Err(DbError::LimitExceeded {
                        limit: "max_batch_ops",
                        max: self.limits.max_batch_ops as u64,
                        actual: ops.len() as u64,
                    });
                }
                // Apply atomically using a reversible operation journal. In particular,
                // schema changes must not clone entire collections just to support rollback.
                let mut undo = Vec::with_capacity(ops.len());
                let mut undo_bytes = 0;
                let mut staged_stats = ApplyStats::default();
                for op in ops {
                    match self.apply_with_undo_and_index_policy(
                        op,
                        now_ms,
                        revision,
                        mode,
                        &mut undo_bytes,
                        unique_only,
                    ) {
                        Ok((sub, inverse)) => {
                            staged_stats.puts += sub.puts;
                            staged_stats.deletes += sub.deletes;
                            staged_stats.collections_created += sub.collections_created;
                            staged_stats.collections_dropped += sub.collections_dropped;
                            undo.push(inverse);
                        }
                        Err(err) => {
                            Self::rollback(self, StoreUndo::Sequence(undo))?;
                            return Err(err);
                        }
                    }
                }
                stats = staged_stats;
            }
        }
        Ok(stats)
    }

    pub(crate) fn apply_with_undo(
        &mut self,
        op: &Op,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
        undo_bytes: &mut usize,
    ) -> Result<(ApplyStats, StoreUndo)> {
        self.apply_with_undo_and_index_policy(op, now_ms, revision, mode, undo_bytes, false)
    }

    fn apply_with_undo_and_index_policy(
        &mut self,
        op: &Op,
        now_ms: i64,
        revision: u64,
        mode: ApplyMode,
        undo_bytes: &mut usize,
        unique_only: bool,
    ) -> Result<(ApplyStats, StoreUndo)> {
        match op {
            Op::Put {
                collection, key, ..
            }
            | Op::Delete { collection, key } => {
                let collection_exists = self.collections.contains_key(collection);
                let previous_ref = self
                    .collections
                    .get(collection)
                    .and_then(|coll| coll.docs.get(key.as_str()));
                if let Some(previous) = previous_ref {
                    let additional = previous.to_json().to_string().len();
                    *undo_bytes = if mode == ApplyMode::Live {
                        reserve_undo_bytes(*undo_bytes, additional)?
                    } else {
                        undo_bytes
                            .checked_add(additional)
                            .ok_or(DbError::LimitExceeded {
                                limit: "recovery_undo_bytes",
                                max: u64::MAX,
                                actual: u64::MAX,
                            })?
                    };
                }
                let previous = previous_ref.cloned();
                let stats =
                    self.apply_with_index_policy(op, now_ms, revision, mode, unique_only)?;
                if mode == ApplyMode::Replay && !collection_exists {
                    return Ok((stats, StoreUndo::None));
                }
                Ok((
                    stats,
                    StoreUndo::Document {
                        collection: collection.clone(),
                        key: key.clone(),
                        previous,
                    },
                ))
            }
            Op::CreateCollection { meta } => {
                let existed = self.collections.contains_key(&meta.name);
                let stats =
                    self.apply_with_index_policy(op, now_ms, revision, mode, unique_only)?;
                Ok((
                    stats,
                    if existed {
                        StoreUndo::None
                    } else {
                        StoreUndo::RemoveCollection(meta.name.clone())
                    },
                ))
            }
            Op::DropCollection { collection } => {
                let removed = self.collections.remove(collection);
                if removed.is_none() && mode == ApplyMode::Live {
                    return Err(DbError::CollectionNotFound(collection.clone()));
                }
                let mut stats = ApplyStats::default();
                if removed.is_some() {
                    stats.collections_dropped += 1;
                }
                Ok((
                    stats,
                    removed
                        .map(|coll| StoreUndo::RestoreCollection(Box::new(coll)))
                        .unwrap_or(StoreUndo::None),
                ))
            }
            Op::CreateIndex { collection, index } => {
                let existed = self
                    .collections
                    .get(collection)
                    .is_some_and(|coll| coll.meta.indexes.contains_key(&index.name));
                let stats =
                    self.apply_with_index_policy(op, now_ms, revision, mode, unique_only)?;
                Ok((
                    stats,
                    if existed {
                        StoreUndo::None
                    } else {
                        StoreUndo::RemoveIndex {
                            collection: collection.clone(),
                            name: index.name.clone(),
                        }
                    },
                ))
            }
            Op::DropIndex { collection, name } => {
                let coll = match self.collections.get_mut(collection) {
                    Some(coll) => coll,
                    None if mode == ApplyMode::Replay => {
                        return Ok((ApplyStats::default(), StoreUndo::None));
                    }
                    None => {
                        return Err(DbError::CollectionNotFound(collection.clone()));
                    }
                };
                let had_meta = coll.meta.indexes.contains_key(name);
                if !had_meta && mode == ApplyMode::Live {
                    return Err(DbError::IndexNotFound {
                        collection: collection.clone(),
                        index: name.clone(),
                    });
                }
                let meta = coll.meta.indexes.remove(name);
                let state = coll.indexes.remove(name);
                Ok((
                    ApplyStats::default(),
                    if meta.is_none() && state.is_none() {
                        StoreUndo::None
                    } else {
                        StoreUndo::RestoreIndex {
                            collection: collection.clone(),
                            name: name.clone(),
                            meta,
                            state,
                        }
                    },
                ))
            }
            Op::Batch { ops, .. } => {
                let mut inverses = Vec::with_capacity(ops.len());
                let mut stats = ApplyStats::default();
                for item in ops {
                    match self.apply_with_undo_and_index_policy(
                        item,
                        now_ms,
                        revision,
                        mode,
                        undo_bytes,
                        unique_only,
                    ) {
                        Ok((sub, inverse)) => {
                            stats.puts += sub.puts;
                            stats.deletes += sub.deletes;
                            stats.collections_created += sub.collections_created;
                            stats.collections_dropped += sub.collections_dropped;
                            inverses.push(inverse);
                        }
                        Err(err) => {
                            Self::rollback(self, StoreUndo::Sequence(inverses))?;
                            return Err(err);
                        }
                    }
                }
                Ok((stats, StoreUndo::Sequence(inverses)))
            }
        }
    }

    pub(crate) fn rollback(&mut self, undo: StoreUndo) -> Result<()> {
        match undo {
            StoreUndo::None => Ok(()),
            StoreUndo::Document {
                collection,
                key,
                previous,
            } => self.restore_document(&collection, &key, previous),
            StoreUndo::RemoveCollection(name) => {
                self.collections.remove(&name);
                Ok(())
            }
            StoreUndo::RestoreCollection(collection) => {
                let name = collection.meta.name.clone();
                if self.collections.contains_key(&name) {
                    return Err(DbError::Internal(format!(
                        "cannot restore collection '{name}': name is already present"
                    )));
                }
                self.collections.insert(name, *collection);
                Ok(())
            }
            StoreUndo::RemoveIndex { collection, name } => {
                self.drop_index(&collection, &name, ApplyMode::Replay)
            }
            StoreUndo::RestoreIndex {
                collection,
                name,
                meta,
                state,
            } => {
                let coll = self.collections.get_mut(&collection).ok_or_else(|| {
                    DbError::Internal(format!(
                        "cannot restore index '{name}': collection '{collection}' is missing"
                    ))
                })?;
                if let Some(meta) = meta {
                    coll.meta.indexes.insert(name.clone(), meta);
                }
                if let Some(state) = state {
                    coll.indexes.insert(name, state);
                }
                Ok(())
            }
            StoreUndo::Sequence(inverses) => {
                for inverse in inverses.into_iter().rev() {
                    self.rollback(inverse)?;
                }
                Ok(())
            }
        }
    }

    /// Read a document, ignoring expired entries.
    pub fn get(&self, collection: &str, key: &str, now_ms: i64) -> Result<Option<&Doc>> {
        let coll = self.collection(collection)?;
        Ok(coll.docs.get(key).filter(|doc| !doc.is_expired(now_ms)))
    }

    /// Current version of a document (0 when absent).
    pub fn version_of(&self, collection: &str, key: &str, now_ms: i64) -> u64 {
        match self.collections.get(collection) {
            Some(coll) => coll
                .docs
                .get(key)
                .filter(|doc| !doc.is_expired(now_ms))
                .map(|doc| doc.version)
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Return up to `limit` expired document keys for a bounded TTL sweep.
    pub fn expired_keys(&self, now_ms: i64, limit: usize) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(limit.min(128));
        if limit == 0 {
            return out;
        }
        'collections: for (name, coll) in &self.collections {
            for (key, doc) in &coll.docs {
                if doc.is_expired(now_ms) {
                    out.push((name.clone(), key.to_string()));
                    if out.len() == limit {
                        break 'collections;
                    }
                }
            }
        }
        out
    }

    /// Total number of stored documents (including not-yet-swept expired ones).
    pub fn total_docs(&self) -> usize {
        self.collections.values().map(|coll| coll.docs.len()).sum()
    }
}

/// Validate a document key.
pub fn validate_key(key: &str, max_len: usize) -> Result<()> {
    if key.is_empty() {
        return Err(DbError::validation("document key must not be empty"));
    }
    if key.len() > max_len {
        return Err(DbError::LimitExceeded {
            limit: "max_key_len",
            max: max_len as u64,
            actual: key.len() as u64,
        });
    }
    if key.chars().any(|c| c.is_control()) {
        return Err(DbError::validation(
            "document key must not contain control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let mut store = Store::new(Limits::default());
        store
            .create_collection(CollectionMeta::new("people", 0), ApplyMode::Live)
            .unwrap();
        store
    }

    fn put(store: &mut Store, key: &str, value: Json) -> Result<u64> {
        store.put("people", key, value, None, None, 1000, 1, ApplyMode::Live)
    }

    #[test]
    fn batch_undo_budget_is_checked_without_overflow() {
        assert_eq!(reserve_undo_bytes(10, 20).unwrap(), 30);
        assert!(matches!(
            reserve_undo_bytes(MAX_BATCH_UNDO_BYTES, 1),
            Err(DbError::LimitExceeded {
                limit: "batch_undo_bytes",
                ..
            })
        ));
        assert!(matches!(
            reserve_undo_bytes(usize::MAX, 1),
            Err(DbError::LimitExceeded {
                limit: "batch_undo_bytes",
                ..
            })
        ));
    }

    #[test]
    fn put_get_versions() {
        let mut s = store();
        assert_eq!(put(&mut s, "a", Json::int(1)).unwrap(), 1);
        assert_eq!(put(&mut s, "a", Json::int(2)).unwrap(), 2);
        let doc = s.get("people", "a", 1000).unwrap().unwrap();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.value.as_i64(), Some(2));
        assert_eq!(doc.created_at, 1000);
    }

    #[test]
    fn exhausted_document_version_rejects_update_without_changing_indexes() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "by_value".into(),
                field: "value".into(),
                unique: false,
                sparse: false,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        let original = Json::obj().with("value", Json::int(1));
        put(&mut s, "a", original.clone()).unwrap();
        s.collections
            .get_mut("people")
            .unwrap()
            .docs
            .get_mut("a")
            .unwrap()
            .version = u64::MAX;

        let error = s
            .put(
                "people",
                "a",
                Json::obj().with("value", Json::int(2)),
                None,
                None,
                1000,
                2,
                ApplyMode::Live,
            )
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::LIMIT_EXCEEDED);
        assert_eq!(s.get("people", "a", 1000).unwrap().unwrap().value, original);
        let index = &s.collection("people").unwrap().indexes["by_value"].entries;
        assert!(index[&Json::int(1).canonical_bytes()].contains("a"));
        assert!(!index.contains_key(&Json::int(2).canonical_bytes()));
    }

    #[test]
    fn snapshot_document_rejects_present_but_invalid_metadata() {
        let valid = Doc {
            value: Json::int(1),
            version: 1,
            revision: 1,
            created_at: 1000,
            updated_at: 1000,
            expires_at: Some(2000),
        }
        .to_json();
        for (field, value) in [
            ("version", Json::str("invalid")),
            ("version", Json::uint(0)),
            ("revision", Json::str("invalid")),
            ("created_at", Json::str("invalid")),
            ("updated_at", Json::str("invalid")),
            ("expires_at", Json::str("invalid")),
        ] {
            let mut forged = valid.clone();
            forged.set(field, value);
            let error = Doc::from_json(&forged).unwrap_err();
            assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA, "{field}");
        }
    }

    #[test]
    fn snapshot_document_keeps_legacy_defaults_for_absent_metadata() {
        let decoded = Doc::from_json(&Json::obj().with("value", Json::int(1))).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.revision, 0);
        assert_eq!(decoded.created_at, 0);
        assert_eq!(decoded.updated_at, 0);
        assert_eq!(decoded.expires_at, None);
    }

    #[test]
    fn compare_and_set() {
        let mut s = store();
        put(&mut s, "a", Json::int(1)).unwrap();
        let err = s
            .put(
                "people",
                "a",
                Json::int(2),
                None,
                Some(5),
                1000,
                2,
                ApplyMode::Live,
            )
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict { .. }));
        s.put(
            "people",
            "a",
            Json::int(3),
            None,
            Some(1),
            1000,
            3,
            ApplyMode::Live,
        )
        .unwrap();
        let err = s
            .put(
                "people",
                "b",
                Json::int(1),
                None,
                Some(0),
                1000,
                4,
                ApplyMode::Live,
            )
            .unwrap();
        assert_eq!(err, 1);
        assert!(s
            .put(
                "people",
                "b",
                Json::int(2),
                None,
                Some(0),
                1000,
                5,
                ApplyMode::Live
            )
            .is_err());
    }

    #[test]
    fn delete_removes_and_errors_when_missing() {
        let mut s = store();
        put(&mut s, "a", Json::int(1)).unwrap();
        assert!(s.delete("people", "a", None, ApplyMode::Live).unwrap());
        assert!(s.get("people", "a", 1000).unwrap().is_none());
        assert!(matches!(
            s.delete("people", "a", None, ApplyMode::Live),
            Err(DbError::NotFound { .. })
        ));
    }

    #[test]
    fn ttl_expiry_hides_documents() {
        let mut s = store();
        s.put(
            "people",
            "tmp",
            Json::int(1),
            Some(2000),
            None,
            1000,
            1,
            ApplyMode::Live,
        )
        .unwrap();
        assert!(s.get("people", "tmp", 1500).unwrap().is_some());
        assert!(s.get("people", "tmp", 2500).unwrap().is_none());
        assert_eq!(s.expired_keys(2500, 10).len(), 1);
        assert_eq!(s.expired_keys(1500, 10).len(), 0);
    }

    #[test]
    fn expired_key_collection_stops_at_requested_bound() {
        let mut s = store();
        for index in 0..20 {
            s.put(
                "people",
                &format!("tmp-{index:02}"),
                Json::int(index),
                Some(2000),
                None,
                1000,
                1,
                ApplyMode::Live,
            )
            .unwrap();
        }

        assert_eq!(s.expired_keys(2500, 7).len(), 7);
        assert!(s.expired_keys(2500, 0).is_empty());
    }

    #[test]
    fn default_ttl_from_collection_meta() {
        let mut s = Store::new(Limits::default());
        let mut meta = CollectionMeta::new("cache", 0);
        meta.default_ttl_ms = Some(500);
        s.create_collection(meta, ApplyMode::Live).unwrap();
        s.put(
            "cache",
            "x",
            Json::int(1),
            None,
            None,
            1000,
            1,
            ApplyMode::Live,
        )
        .unwrap();
        assert_eq!(
            s.get("cache", "x", 1000).unwrap().unwrap().expires_at,
            Some(1500)
        );
    }

    #[test]
    fn overflowing_default_ttl_rejects_write_without_changing_document() {
        let mut s = Store::new(Limits::default());
        let mut meta = CollectionMeta::new("cache", 0);
        meta.default_ttl_ms = Some(i64::MAX);
        s.create_collection(meta, ApplyMode::Live).unwrap();
        let error = s
            .put(
                "cache",
                "x",
                Json::int(1),
                None,
                None,
                1000,
                1,
                ApplyMode::Live,
            )
            .unwrap_err();
        assert_eq!(error.code(), crate::error::codes::VALIDATION_FAILED);
        assert!(s.get("cache", "x", 1000).unwrap().is_none());
    }

    #[test]
    fn secondary_index_maintenance() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "by_name".into(),
                field: "name".into(),
                unique: false,
                sparse: true,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(&mut s, "a", Json::obj().with("name", Json::str("Molly"))).unwrap();
        put(&mut s, "b", Json::obj().with("name", Json::str("Tori"))).unwrap();
        put(&mut s, "a", Json::obj().with("name", Json::str("Tori"))).unwrap();
        let coll = s.collection("people").unwrap();
        let index = coll.indexes.get("by_name").unwrap();
        let tori = Json::str("Tori").canonical_bytes();
        assert_eq!(index.entries.get(&tori).unwrap().len(), 2);
        let molly = Json::str("Molly").canonical_bytes();
        assert!(!index.entries.contains_key(&molly));
    }

    #[test]
    fn secondary_index_keys_share_primary_key_allocation() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "by_name".into(),
                field: "name".into(),
                unique: false,
                sparse: false,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(
            &mut s,
            "a-long-document-key",
            Json::obj().with("name", Json::str("Molly")),
        )
        .unwrap();

        let collection = s.collection("people").unwrap();
        let primary_key = collection
            .docs
            .get_key_value("a-long-document-key")
            .unwrap()
            .0;
        let index_key = collection.indexes["by_name"].entries
            [&Json::str("Molly").canonical_bytes()]
            .iter()
            .next()
            .unwrap();

        assert!(std::sync::Arc::ptr_eq(primary_key, index_key));
    }

    #[test]
    fn unique_index_rejects_duplicates() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "uniq_name".into(),
                field: "name".into(),
                unique: true,
                sparse: false,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(&mut s, "a", Json::obj().with("name", Json::str("Molly"))).unwrap();
        let err = put(&mut s, "b", Json::obj().with("name", Json::str("Molly"))).unwrap_err();
        assert!(err.to_string().contains("unique index"), "{err}");
        // The failed write must not leave the document behind.
        assert!(s.get("people", "b", 1000).unwrap().is_none());
        // Same key may keep its own value.
        put(
            &mut s,
            "a",
            Json::obj()
                .with("name", Json::str("Molly"))
                .with("x", Json::int(1)),
        )
        .unwrap();
    }

    #[test]
    fn verification_store_materializes_unique_indexes_but_preserves_other_metadata() {
        let mut s = Store::new(Limits::default());
        let unique = IndexMeta {
            name: "uniq_name".into(),
            field: "name".into(),
            unique: true,
            sparse: false,
            created_at: 0,
        };
        let secondary = IndexMeta {
            name: "by_group".into(),
            field: "group".into(),
            unique: false,
            sparse: true,
            created_at: 0,
        };
        s.apply_for_verification(
            &Op::CreateCollection {
                meta: CollectionMeta::new("people", 0),
            },
            1000,
            1,
        )
        .unwrap();
        s.apply_for_verification(
            &Op::CreateIndex {
                collection: "people".into(),
                index: unique,
            },
            1000,
            2,
        )
        .unwrap();
        s.apply_for_verification(
            &Op::CreateIndex {
                collection: "people".into(),
                index: secondary,
            },
            1000,
            3,
        )
        .unwrap();

        let collection = s.collection("people").unwrap();
        assert!(collection.meta.indexes.contains_key("uniq_name"));
        assert!(collection.meta.indexes.contains_key("by_group"));
        assert!(collection.indexes.contains_key("uniq_name"));
        assert!(!collection.indexes.contains_key("by_group"));

        let put = |key: &str| Op::Put {
            collection: "people".into(),
            key: key.into(),
            value: Json::obj()
                .with("name", Json::str("Molly"))
                .with("group", Json::str("staff")),
            expires_at: None,
        };
        s.apply_for_verification(&put("a"), 1000, 4).unwrap();
        let conflict = s.apply_for_verification(&put("b"), 1000, 5);
        assert!(conflict.is_err(), "unique replay conflict must still fail");
        assert_eq!(s.total_docs(), 1);

        s.apply_for_verification(
            &Op::Delete {
                collection: "people".into(),
                key: "a".into(),
            },
            1000,
            6,
        )
        .unwrap();
        s.apply_for_verification(&put("b"), 1000, 7).unwrap();
        assert_eq!(s.total_docs(), 1);
    }

    #[test]
    fn unique_conflicting_update_preserves_document_and_index_without_mutation() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "uniq_name".into(),
                field: "name".into(),
                unique: true,
                sparse: false,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(&mut s, "a", Json::obj().with("name", Json::str("Molly"))).unwrap();
        put(&mut s, "b", Json::obj().with("name", Json::str("Tori"))).unwrap();

        assert!(put(&mut s, "a", Json::obj().with("name", Json::str("Tori"))).is_err());

        assert_eq!(
            s.get("people", "a", 1000).unwrap().unwrap().value,
            Json::obj().with("name", Json::str("Molly"))
        );
        let index = &s.collection("people").unwrap().indexes["uniq_name"];
        assert_eq!(
            index.entries[&Json::str("Molly").canonical_bytes()]
                .iter()
                .map(|key| key.as_ref())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(
            index.entries[&Json::str("Tori").canonical_bytes()]
                .iter()
                .map(|key| key.as_ref())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn non_sparse_missing_value_is_removed_from_null_index() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "by_name".into(),
                field: "name".into(),
                unique: true,
                sparse: false,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(&mut s, "a", Json::obj()).unwrap();
        s.delete("people", "a", None, ApplyMode::Live).unwrap();
        put(&mut s, "b", Json::obj()).unwrap();
        let index = &s.collection("people").unwrap().indexes["by_name"];
        let bucket = &index.entries[&Json::Null.canonical_bytes()];
        assert_eq!(
            bucket.iter().map(|key| key.as_ref()).collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn unique_conflict_does_not_leave_earlier_index_entries() {
        let mut s = store();
        for (name, field, unique) in [("a_color", "color", false), ("z_name", "name", true)] {
            s.create_index(
                "people",
                IndexMeta {
                    name: name.into(),
                    field: field.into(),
                    unique,
                    sparse: true,
                    created_at: 0,
                },
                ApplyMode::Live,
            )
            .unwrap();
        }
        put(&mut s, "a", Json::obj().with("name", Json::str("same"))).unwrap();
        assert!(put(
            &mut s,
            "b",
            Json::obj()
                .with("name", Json::str("same"))
                .with("color", Json::str("pink"))
        )
        .is_err());
        let index = &s.collection("people").unwrap().indexes["a_color"];
        assert!(!index
            .entries
            .contains_key(&Json::str("pink").canonical_bytes()));
        assert!(s.get("people", "b", 1000).unwrap().is_none());
    }

    #[test]
    fn schema_enforced_on_live_writes_only() {
        let mut s = Store::new(Limits::default());
        let mut meta = CollectionMeta::new("strict", 0);
        meta.schema = Some(preppydb_json::parse(r#"{"type":"object","required":["a"]}"#).unwrap());
        s.create_collection(meta, ApplyMode::Live).unwrap();
        let err = s
            .put(
                "strict",
                "k",
                Json::obj(),
                None,
                None,
                0,
                1,
                ApplyMode::Live,
            )
            .unwrap_err();
        assert!(err.to_string().contains("missing required member"));
        // Replay path skips validation so old logs stay readable.
        s.put(
            "strict",
            "k",
            Json::obj(),
            None,
            None,
            0,
            2,
            ApplyMode::Replay,
        )
        .unwrap();
    }

    #[test]
    fn batch_is_atomic() {
        let mut s = store();
        let batch = Op::Batch {
            label: "test".into(),
            ops: vec![
                Op::Put {
                    collection: "people".into(),
                    key: "ok".into(),
                    value: Json::int(1),
                    expires_at: None,
                },
                Op::Delete {
                    collection: "people".into(),
                    key: "nope".into(),
                },
            ],
        };
        let err = s.apply(&batch, 0, 1, ApplyMode::Live).unwrap_err();
        assert!(matches!(err, DbError::NotFound { .. }));
        assert_eq!(s.total_docs(), 0, "partial batch must not be visible");
    }

    #[test]
    fn replay_batch_rollback_ignores_missing_collection_document_operations() {
        let mut s = store();
        put(
            &mut s,
            "a",
            Json::obj().with("name", Json::str("duplicate")),
        )
        .unwrap();
        put(
            &mut s,
            "b",
            Json::obj().with("name", Json::str("duplicate")),
        )
        .unwrap();

        let err = s
            .apply(
                &Op::Batch {
                    ops: vec![
                        Op::Put {
                            collection: "missing".into(),
                            key: "key".into(),
                            value: Json::int(1),
                            expires_at: None,
                        },
                        Op::CreateIndex {
                            collection: "people".into(),
                            index: IndexMeta {
                                name: "unique_name".into(),
                                field: "name".into(),
                                unique: true,
                                sparse: false,
                                created_at: 0,
                            },
                        },
                    ],
                    label: "recovery".into(),
                },
                1000,
                2,
                ApplyMode::Replay,
            )
            .unwrap_err();
        assert!(err.to_string().contains("unique index"), "{err}");
        assert!(s.collection("missing").is_err());
        assert!(!s
            .collection("people")
            .unwrap()
            .meta
            .indexes
            .contains_key("unique_name"));
    }

    #[test]
    fn limits_are_enforced() {
        let limits = Limits {
            max_doc_bytes: 16,
            ..Default::default()
        };
        let mut s = Store::new(limits);
        s.create_collection(CollectionMeta::new("c", 0), ApplyMode::Live)
            .unwrap();
        let err = s
            .put(
                "c",
                "k",
                Json::str("x".repeat(100)),
                None,
                None,
                0,
                1,
                ApplyMode::Live,
            )
            .unwrap_err();
        assert!(matches!(err, DbError::LimitExceeded { .. }));
        assert!(validate_key("", 10).is_err());
        assert!(validate_key("with\nnewline", 10).is_err());
        assert!(validate_key(&"k".repeat(11), 10).is_err());
    }

    #[test]
    fn rebuild_indexes_after_load() {
        let mut s = store();
        s.create_index(
            "people",
            IndexMeta {
                name: "by_age".into(),
                field: "age".into(),
                unique: false,
                sparse: true,
                created_at: 0,
            },
            ApplyMode::Live,
        )
        .unwrap();
        put(&mut s, "a", Json::obj().with("age", Json::int(30))).unwrap();
        let mut coll = s.collection("people").unwrap().clone();
        coll.indexes.clear();
        coll.rebuild_indexes().unwrap();
        assert_eq!(coll.indexes["by_age"].entries.len(), 1);
    }
}
