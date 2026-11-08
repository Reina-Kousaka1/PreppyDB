//! Compact semantic replay state used only by `Engine::verify`.
//!
//! The live engine already holds its complete store in memory while verification runs.
//! Retaining a second `Doc` map (including versions, timestamps and expiry metadata) is
//! unnecessary for checking whether the snapshot and WAL can be replayed and whether
//! unique constraints remain valid. This projection keeps only document keys, fields
//! needed by unique indexes, and unique index entries. It is never used for open,
//! recovery, queries, or writes.

use crate::catalog::{CollectionMeta, IndexMeta};
use crate::error::{DbError, Result};
use crate::record::Op;
use crate::store::{validate_key, Limits};
use preppydb_json::Json;
use std::collections::BTreeMap;
use std::sync::Arc;

struct UniqueIndex {
    meta: IndexMeta,
    entries: BTreeMap<Vec<u8>, Arc<str>>,
}

struct VerifyCollection {
    meta: CollectionMeta,
    // Projection only: one compact JSON object per key, containing the bounded set
    // of field paths that may participate in a unique index during this replay.
    docs: BTreeMap<Arc<str>, Json>,
    unique_indexes: BTreeMap<String, UniqueIndex>,
}

pub(crate) struct VerifyStore {
    collections: BTreeMap<String, VerifyCollection>,
    limits: Limits,
}

impl VerifyStore {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            collections: BTreeMap::new(),
            limits,
        }
    }

    pub(crate) fn create_collection(&mut self, meta: CollectionMeta) -> Result<()> {
        if self.collections.contains_key(&meta.name) {
            return Ok(()); // Match idempotent replay semantics.
        }
        let unique_indexes = meta
            .indexes
            .values()
            .filter(|index| index.unique)
            .map(|index| {
                (
                    index.name.clone(),
                    UniqueIndex {
                        meta: index.clone(),
                        entries: BTreeMap::new(),
                    },
                )
            })
            .collect();
        self.collections.insert(
            meta.name.clone(),
            VerifyCollection {
                meta,
                docs: BTreeMap::new(),
                unique_indexes,
            },
        );
        Ok(())
    }

    pub(crate) fn insert_snapshot_document(
        &mut self,
        collection: &str,
        key: String,
        projected_value: Json,
    ) -> Result<()> {
        let coll = self.collections.get_mut(collection).ok_or_else(|| {
            DbError::corrupt(
                "snapshot",
                format!("document for unknown collection '{collection}'"),
            )
        })?;
        let key: Arc<str> = key.into();
        if coll.docs.contains_key(key.as_ref()) {
            return Err(DbError::corrupt(
                "snapshot",
                format!("duplicate document key in collection '{collection}'"),
            ));
        }
        Self::validate_indexes(coll, &key, &projected_value)?;
        Self::index_document(coll, &key, &projected_value);
        coll.docs.insert(key, projected_value);
        Ok(())
    }

    pub(crate) fn apply(&mut self, op: &Op) -> Result<()> {
        match op {
            Op::Put {
                collection,
                key,
                value,
                ..
            } => self.put(collection, key, value.clone()),
            Op::Delete { collection, key } => self.delete(collection, key),
            Op::CreateCollection { meta } => self.create_collection(meta.clone()),
            Op::DropCollection { collection } => {
                self.collections.remove(collection);
                Ok(())
            }
            Op::CreateIndex { collection, index } => self.create_index(collection, index.clone()),
            Op::DropIndex { collection, name } => {
                if let Some(coll) = self.collections.get_mut(collection) {
                    coll.meta.indexes.remove(name);
                    coll.unique_indexes.remove(name);
                }
                Ok(())
            }
            Op::Batch { ops, .. } => {
                for item in ops {
                    self.apply(item)?;
                }
                Ok(())
            }
        }
    }

    fn put(&mut self, collection: &str, key: &str, value: Json) -> Result<()> {
        validate_key(key, self.limits.max_key_len)?;
        let Some(coll) = self.collections.get_mut(collection) else {
            return Ok(()); // Match replay behavior for operations before collection creation.
        };
        let stored_key = coll
            .docs
            .get_key_value(key)
            .map(|(key, _)| Arc::clone(key))
            .unwrap_or_else(|| Arc::<str>::from(key));
        Self::validate_indexes(coll, &stored_key, &value)?;
        if let Some(previous) = coll.docs.remove(key) {
            Self::unindex_document(coll, &stored_key, &previous);
        }
        Self::index_document(coll, &stored_key, &value);
        coll.docs.insert(stored_key, value);
        Ok(())
    }

    fn delete(&mut self, collection: &str, key: &str) -> Result<()> {
        if let Some(coll) = self.collections.get_mut(collection) {
            if let Some((stored_key, previous)) = coll.docs.remove_entry(key) {
                Self::unindex_document(coll, &stored_key, &previous);
            }
        }
        Ok(())
    }

    fn create_index(&mut self, collection: &str, index: IndexMeta) -> Result<()> {
        let Some(coll) = self.collections.get_mut(collection) else {
            return Ok(());
        };
        if coll.meta.indexes.contains_key(&index.name) {
            return Ok(());
        }
        coll.meta.indexes.insert(index.name.clone(), index.clone());
        if !index.unique {
            return Ok(());
        }
        let mut state = UniqueIndex {
            meta: index.clone(),
            entries: BTreeMap::new(),
        };
        for (key, value) in &coll.docs {
            if let Some(encoded) = Self::index_value(&index, value) {
                if let Some(other) = state.entries.get(&encoded) {
                    if other.as_ref() != key.as_ref() {
                        return Err(Self::unique_conflict(&index, &encoded, other));
                    }
                }
                state.entries.insert(encoded, Arc::clone(key));
            }
        }
        coll.unique_indexes.insert(index.name.clone(), state);
        Ok(())
    }

    fn validate_indexes(coll: &VerifyCollection, key: &str, value: &Json) -> Result<()> {
        for index in coll.unique_indexes.values() {
            if let Some(encoded) = Self::index_value(&index.meta, value) {
                if let Some(other) = index
                    .entries
                    .get(&encoded)
                    .filter(|other| other.as_ref() != key)
                {
                    return Err(Self::unique_conflict(&index.meta, &encoded, other));
                }
            }
        }
        Ok(())
    }

    fn index_value(index: &IndexMeta, value: &Json) -> Option<Vec<u8>> {
        match value.get_path(&index.field) {
            Some(value) => Some(value.canonical_bytes()),
            None if index.sparse => None,
            None => Some(Json::Null.canonical_bytes()),
        }
    }

    fn unique_conflict(index: &IndexMeta, encoded: &[u8], other: &str) -> DbError {
        let value = std::str::from_utf8(encoded).unwrap_or("<encoded value>");
        DbError::validation(format!(
            "unique index '{}' already contains value {} for key '{}'",
            index.name, value, other
        ))
    }

    fn index_document(coll: &mut VerifyCollection, key: &Arc<str>, value: &Json) {
        for index in coll.unique_indexes.values_mut() {
            if let Some(encoded) = Self::index_value(&index.meta, value) {
                index.entries.insert(encoded, Arc::clone(key));
            }
        }
    }

    fn unindex_document(coll: &mut VerifyCollection, key: &str, value: &Json) {
        for index in coll.unique_indexes.values_mut() {
            if let Some(encoded) = Self::index_value(&index.meta, value) {
                if index
                    .entries
                    .get(&encoded)
                    .is_some_and(|stored| stored.as_ref() == key)
                {
                    index.entries.remove(&encoded);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn index(name: &str, field: &str, unique: bool, sparse: bool) -> IndexMeta {
        IndexMeta {
            name: name.to_string(),
            field: field.to_string(),
            unique,
            sparse,
            created_at: 1,
        }
    }

    fn put(key: &str, value: Json) -> Op {
        Op::Put {
            collection: "users".to_string(),
            key: key.to_string(),
            value,
            expires_at: None,
        }
    }

    fn assert_matches_full_replay(ops: impl IntoIterator<Item = Op>) {
        let limits = Limits::default();
        let mut compact = VerifyStore::new(limits.clone());
        let mut full = Store::new(limits);
        for op in ops {
            let compact_ok = compact.apply(&op).is_ok();
            let full_ok = full.apply_for_verification(&op, 1, 1).is_ok();
            assert_eq!(compact_ok, full_ok, "different result for {op:?}");
        }
    }

    #[test]
    fn compact_replay_matches_unique_mutations_and_sparse_semantics() {
        let mut meta = CollectionMeta::new("users", 1);
        meta.indexes.insert(
            "email".to_string(),
            index("email", "profile.email", true, true),
        );
        assert_matches_full_replay([
            Op::CreateCollection { meta },
            put(
                "a",
                Json::obj().with(
                    "profile",
                    Json::obj().with("email", Json::str("a@example.test")),
                ),
            ),
            put("missing-a", Json::obj()),
            put(
                "b",
                Json::obj().with(
                    "profile",
                    Json::obj().with("email", Json::str("b@example.test")),
                ),
            ),
            put(
                "b",
                Json::obj().with(
                    "profile",
                    Json::obj().with("email", Json::str("a@example.test")),
                ),
            ),
            Op::Delete {
                collection: "users".to_string(),
                key: "a".to_string(),
            },
            put(
                "b",
                Json::obj().with(
                    "profile",
                    Json::obj().with("email", Json::str("a@example.test")),
                ),
            ),
        ]);
    }

    #[test]
    fn compact_replay_can_build_unique_index_after_snapshot_documents() {
        let mut verify = VerifyStore::new(Limits::default());
        verify
            .create_collection(CollectionMeta::new("users", 1))
            .unwrap();
        verify
            .insert_snapshot_document(
                "users",
                "a".to_string(),
                Json::obj().with("email", Json::str("a@example.test")),
            )
            .unwrap();
        verify
            .insert_snapshot_document(
                "users",
                "b".to_string(),
                Json::obj().with("email", Json::str("b@example.test")),
            )
            .unwrap();
        verify
            .apply(&Op::CreateIndex {
                collection: "users".to_string(),
                index: index("email", "email", true, false),
            })
            .unwrap();
        assert!(verify
            .apply(&put(
                "c",
                Json::obj().with("email", Json::str("a@example.test")),
            ))
            .is_err());
        verify
            .apply(&Op::Delete {
                collection: "users".to_string(),
                key: "a".to_string(),
            })
            .unwrap();
        verify
            .apply(&put(
                "c",
                Json::obj().with("email", Json::str("a@example.test")),
            ))
            .unwrap();
    }
}
