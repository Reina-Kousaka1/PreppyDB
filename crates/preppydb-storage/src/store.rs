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
        Ok(Doc {
            value: doc,
            version: value.get("version").and_then(|v| v.as_u64()).unwrap_or(1),
            revision: value.get("revision").and_then(|v| v.as_u64()).unwrap_or(0),
            created_at: value.get_i64("created_at").unwrap_or(0),
            updated_at: value.get_i64("updated_at").unwrap_or(0),
            expires_at: value.get("expires_at").and_then(|v| v.as_i64()),
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
    pub entries: BTreeMap<Vec<u8>, BTreeSet<String>>,
}

impl IndexState {
    pub fn new(meta: IndexMeta) -> IndexState {
        IndexState {
            meta,
            entries: BTreeMap::new(),
        }
    }

    fn insert(&mut self, doc_key: &str, doc: &Doc) -> Result<()> {
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
        let bucket = self.entries.entry(encoded).or_default();
        if self.meta.unique && !bucket.is_empty() && !bucket.contains(doc_key) {
            return Err(DbError::validation(format!(
                "unique index '{}' already contains value {} for key '{}'",
                self.meta.name,
                value,
                bucket.iter().next().unwrap_or(&String::new())
            )));
        }
        bucket.insert(doc_key.to_string());
        Ok(())
    }

    fn remove(&mut self, doc_key: &str, doc: &Doc) {
        if let Some(value) = doc.value.get_path(&self.meta.field) {
            let encoded = value.canonical_bytes();
            if let Some(bucket) = self.entries.get_mut(&encoded) {
                bucket.remove(doc_key);
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
    pub docs: BTreeMap<String, Doc>,
    pub indexes: BTreeMap<String, IndexState>,
}

impl Collection {
    pub fn new(meta: CollectionMeta) -> Collection {
        let mut indexes = BTreeMap::new();
        for index in meta.indexes.values() {
            indexes.insert(index.name.clone(), IndexState::new(index.clone()));
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
        self.indexes.clear();
        for index in self.meta.indexes.values() {
            self.indexes
                .insert(index.name.clone(), IndexState::new(index.clone()));
        }
        let docs: Vec<(String, Doc)> = self
            .docs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (key, doc) in docs {
            self.index_document(&key, &doc)?;
        }
        Ok(())
    }

    fn index_document(&mut self, key: &str, doc: &Doc) -> Result<()> {
        for index in self.indexes.values_mut() {
            index.insert(key, doc)?;
        }
        Ok(())
    }

    fn unindex_document(&mut self, key: &str, doc: &Doc) {
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

    pub fn has_collection(&self, name: &str) -> bool {
        self.collections.contains_key(name)
    }

    /// Create a collection (idempotent only when the definition is identical).
    pub fn create_collection(&mut self, meta: CollectionMeta, mode: ApplyMode) -> Result<()> {
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
        self.collections
            .insert(meta.name.clone(), Collection::new(meta));
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
        validate_key(key, self.limits.max_key_len)?;
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok(0),
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
        let existing = coll.docs.get(key).cloned();
        if mode == ApplyMode::Live {
            match expected_version {
                Some(0) if existing.is_some() => {
                    return Err(DbError::Conflict {
                        collection: collection.to_string(),
                        key: key.to_string(),
                        expected: Some(0),
                        actual: existing.as_ref().map(|doc| doc.version),
                    })
                }
                Some(expected) if expected > 0 => {
                    let actual = existing.as_ref().map(|doc| doc.version).unwrap_or(0);
                    if actual != expected {
                        return Err(DbError::Conflict {
                            collection: collection.to_string(),
                            key: key.to_string(),
                            expected: Some(expected),
                            actual: existing.as_ref().map(|doc| doc.version),
                        });
                    }
                }
                _ => {}
            }
            if existing.is_none() {
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
        if let Some(old) = &existing {
            coll.unindex_document(key, old);
        }
        let (version, created_at) = match &existing {
            Some(doc) => (doc.version + 1, doc.created_at),
            None => (1, now_ms),
        };
        let expires_at = expires_at.or(coll.meta.default_ttl_ms.map(|ttl| now_ms + ttl));
        let doc = Doc {
            value,
            version,
            revision,
            created_at,
            updated_at: now_ms,
            expires_at,
        };
        // Index maintenance can fail (unique constraint); roll back cleanly.
        if let Err(err) = coll.index_document(key, &doc) {
            if let Some(old) = existing {
                let _ = coll.index_document(key, &old);
            }
            return Err(err);
        }
        coll.docs.insert(key.to_string(), doc);
        Ok(version)
    }

    pub fn delete(
        &mut self,
        collection: &str,
        key: &str,
        expected_version: Option<u64>,
        mode: ApplyMode,
    ) -> Result<bool> {
        let coll = match self.collections.get_mut(collection) {
            Some(coll) => coll,
            None if mode == ApplyMode::Replay => return Ok(false),
            None => return Err(DbError::CollectionNotFound(collection.to_string())),
        };
        let existing = coll.docs.get(key).cloned();
        if mode == ApplyMode::Live {
            match (expected_version, &existing) {
                (None, None) => {
                    return Err(DbError::NotFound {
                        collection: collection.to_string(),
                        key: key.to_string(),
                    })
                }
                (Some(expected), Some(doc)) if expected != doc.version => {
                    return Err(DbError::Conflict {
                        collection: collection.to_string(),
                        key: key.to_string(),
                        expected: Some(expected),
                        actual: Some(doc.version),
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
        match existing {
            Some(doc) => {
                coll.unindex_document(key, &doc);
                coll.docs.remove(key);
                Ok(true)
            }
            None => Ok(false),
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
                self.create_collection(meta.clone(), mode)?;
                stats.collections_created += 1;
            }
            Op::DropCollection { collection } => {
                self.drop_collection(collection, mode)?;
                stats.collections_dropped += 1;
            }
            Op::CreateIndex { collection, index } => {
                self.create_index(collection, index.clone(), mode)?;
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
                // Batches are atomic: apply to a clone and swap on success so a failure
                // half-way cannot leave partial state behind.
                let mut staged = self.clone();
                for op in ops {
                    let sub = staged.apply(op, now_ms, revision, mode)?;
                    stats.puts += sub.puts;
                    stats.deletes += sub.deletes;
                    stats.collections_created += sub.collections_created;
                    stats.collections_dropped += sub.collections_dropped;
                }
                *self = staged;
            }
        }
        Ok(stats)
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

    /// Keys of all expired documents (for the TTL sweeper).
    pub fn expired_keys(&self, now_ms: i64) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, coll) in &self.collections {
            for (key, doc) in &coll.docs {
                if doc.is_expired(now_ms) {
                    out.push((name.clone(), key.clone()));
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
        assert_eq!(s.expired_keys(2500).len(), 1);
        assert_eq!(s.expired_keys(1500).len(), 0);
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
