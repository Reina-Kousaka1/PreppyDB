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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type CanonicalValue = Arc<[u8]>;
// Keep only fields actually present in a document. A dense Vec<Option<_>> would
// allocate one empty slot per field in the bounded plan for every document, which
// can amplify memory use for sparse collections with many indexed fields.
type ProjectedValues = Vec<(usize, CanonicalValue)>;

struct UniqueIndex {
    meta: IndexMeta,
    field_slot: usize,
    entries: BTreeMap<CanonicalValue, Arc<str>>,
}

struct VerifyCollection {
    meta: CollectionMeta,
    // The reverse entries share canonical bytes with the unique-index trees.
    docs: BTreeMap<Arc<str>, ProjectedValues>,
    field_slots: BTreeMap<String, usize>,
    unique_indexes: BTreeMap<String, UniqueIndex>,
}

pub(crate) struct VerifyStore {
    collections: BTreeMap<String, VerifyCollection>,
    field_plan: BTreeMap<String, BTreeSet<String>>,
    limits: Limits,
}

impl VerifyStore {
    #[cfg(test)]
    pub(crate) fn new(limits: Limits) -> Self {
        Self::with_field_plan(limits, BTreeMap::new())
    }

    pub(crate) fn with_field_plan(
        limits: Limits,
        field_plan: BTreeMap<String, BTreeSet<String>>,
    ) -> Self {
        Self {
            collections: BTreeMap::new(),
            field_plan,
            limits,
        }
    }

    pub(crate) fn create_collection(&mut self, meta: CollectionMeta) -> Result<()> {
        if self.collections.contains_key(&meta.name) {
            return Ok(()); // Match idempotent replay semantics.
        }
        let mut fields = self.field_plan.get(&meta.name).cloned().unwrap_or_default();
        fields.extend(
            meta.indexes
                .values()
                .filter(|index| index.unique)
                .map(|index| index.field.clone()),
        );
        let field_slots: BTreeMap<String, usize> = fields
            .into_iter()
            .enumerate()
            .map(|(slot, field)| (field, slot))
            .collect();
        let mut unique_indexes = BTreeMap::new();
        for index in meta.indexes.values().filter(|index| index.unique) {
            let field_slot = *field_slots.get(&index.field).ok_or_else(|| {
                DbError::corrupt("WAL verification", "unique index field is absent from plan")
            })?;
            unique_indexes.insert(
                index.name.clone(),
                UniqueIndex {
                    meta: index.clone(),
                    field_slot,
                    entries: BTreeMap::new(),
                },
            );
        }
        self.collections.insert(
            meta.name.clone(),
            VerifyCollection {
                meta,
                field_slots,
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
        value: &Json,
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
        let values = Self::project_values(coll, value);
        Self::validate_indexes(coll, &key, &values)?;
        Self::index_document(coll, &key, &values);
        coll.docs.insert(key, values);
        Ok(())
    }

    pub(crate) fn apply(&mut self, op: &Op) -> Result<()> {
        match op {
            Op::Put {
                collection,
                key,
                value,
                ..
            } => self.put(collection, key, value),
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

    fn put(&mut self, collection: &str, key: &str, value: &Json) -> Result<()> {
        validate_key(key, self.limits.max_key_len)?;
        let Some(coll) = self.collections.get_mut(collection) else {
            return Ok(()); // Match replay behavior for operations before collection creation.
        };
        let stored_key = coll
            .docs
            .get_key_value(key)
            .map(|(key, _)| Arc::clone(key))
            .unwrap_or_else(|| Arc::<str>::from(key));
        let values = Self::project_values(coll, value);
        Self::validate_indexes(coll, &stored_key, &values)?;
        if let Some(previous) = coll.docs.remove(key) {
            Self::unindex_document(coll, &stored_key, &previous);
        }
        Self::index_document(coll, &stored_key, &values);
        coll.docs.insert(stored_key, values);
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
        if !index.unique {
            coll.meta.indexes.insert(index.name.clone(), index);
            return Ok(());
        }
        let field_slot = *coll.field_slots.get(&index.field).ok_or_else(|| {
            DbError::corrupt(
                "WAL verification",
                "unique index field is absent from bounded replay plan",
            )
        })?;
        let mut state = UniqueIndex {
            meta: index.clone(),
            field_slot,
            entries: BTreeMap::new(),
        };
        for (key, values) in &coll.docs {
            if let Some(encoded) = Self::index_value(&state, values) {
                if let Some(other) = state.entries.get(encoded.as_ref()) {
                    if other.as_ref() != key.as_ref() {
                        return Err(Self::unique_conflict(&index, encoded.as_ref(), other));
                    }
                }
                state.entries.insert(encoded, Arc::clone(key));
            }
        }
        coll.meta.indexes.insert(index.name.clone(), index.clone());
        coll.unique_indexes.insert(index.name.clone(), state);
        Ok(())
    }

    fn project_values(coll: &VerifyCollection, value: &Json) -> ProjectedValues {
        let mut projected = Vec::new();
        // `field_slots` is keyed by the same lexical order used to assign slots,
        // so this compact list stays sorted for `index_value`'s binary search.
        for (field, slot) in &coll.field_slots {
            if let Some(value) = value.get_path(field) {
                projected.push((*slot, Arc::<[u8]>::from(value.canonical_bytes())));
            }
        }
        projected
    }

    fn validate_indexes(
        coll: &VerifyCollection,
        key: &str,
        values: &[(usize, CanonicalValue)],
    ) -> Result<()> {
        for index in coll.unique_indexes.values() {
            if let Some(encoded) = Self::index_value(index, values) {
                if let Some(other) = index
                    .entries
                    .get(encoded.as_ref())
                    .filter(|other| other.as_ref() != key)
                {
                    return Err(Self::unique_conflict(&index.meta, encoded.as_ref(), other));
                }
            }
        }
        Ok(())
    }

    fn index_value(
        index: &UniqueIndex,
        values: &[(usize, CanonicalValue)],
    ) -> Option<CanonicalValue> {
        match values.binary_search_by_key(&index.field_slot, |(slot, _)| *slot) {
            Ok(position) => Some(Arc::clone(&values[position].1)),
            Err(_) if index.meta.sparse => None,
            Err(_) => Some(null_index_key()),
        }
    }

    fn unique_conflict(index: &IndexMeta, encoded: &[u8], other: &str) -> DbError {
        let value = std::str::from_utf8(encoded).unwrap_or("<encoded value>");
        DbError::validation(format!(
            "unique index '{}' already contains value {} for key '{}'",
            index.name, value, other
        ))
    }

    fn index_document(
        coll: &mut VerifyCollection,
        key: &Arc<str>,
        values: &[(usize, CanonicalValue)],
    ) {
        for index in coll.unique_indexes.values_mut() {
            if let Some(encoded) = Self::index_value(index, values) {
                index.entries.insert(encoded, Arc::clone(key));
            }
        }
    }

    fn unindex_document(
        coll: &mut VerifyCollection,
        key: &str,
        values: &[(usize, CanonicalValue)],
    ) {
        for index in coll.unique_indexes.values_mut() {
            if let Some(encoded) = Self::index_value(index, values) {
                if index
                    .entries
                    .get(encoded.as_ref())
                    .is_some_and(|stored| stored.as_ref() == key)
                {
                    index.entries.remove(encoded.as_ref());
                }
            }
        }
    }
}

fn null_index_key() -> Arc<[u8]> {
    static NULL_KEY: std::sync::OnceLock<Arc<[u8]>> = std::sync::OnceLock::new();
    Arc::clone(NULL_KEY.get_or_init(|| Arc::from(Json::Null.canonical_bytes())))
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
        let field_plan =
            BTreeMap::from([("users".to_string(), BTreeSet::from(["email".to_string()]))]);
        let mut verify = VerifyStore::with_field_plan(Limits::default(), field_plan);
        verify
            .create_collection(CollectionMeta::new("users", 1))
            .unwrap();
        verify
            .insert_snapshot_document(
                "users",
                "a".to_string(),
                &Json::obj().with("email", Json::str("a@example.test")),
            )
            .unwrap();
        verify
            .insert_snapshot_document(
                "users",
                "b".to_string(),
                &Json::obj().with("email", Json::str("b@example.test")),
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

    #[test]
    fn projected_values_store_only_present_fields_and_keep_sparse_index_semantics() {
        let fields = (0..1_000).map(|n| format!("f{n:04}")).collect();
        let plan = BTreeMap::from([("users".to_string(), fields)]);
        let mut verify = VerifyStore::with_field_plan(Limits::default(), plan);
        verify
            .create_collection(CollectionMeta::new("users", 1))
            .unwrap();
        verify
            .apply(&put(
                "a",
                Json::obj()
                    .with("f0002", Json::str("other"))
                    .with("f0999", Json::str("present")),
            ))
            .unwrap();

        let values = verify.collections["users"].docs.get("a").unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].0, 2);
        assert_eq!(values[1].0, 999);

        verify
            .apply(&Op::CreateIndex {
                collection: "users".to_string(),
                index: index("sparse_f0998", "f0998", true, true),
            })
            .unwrap();
        verify
            .apply(&Op::CreateIndex {
                collection: "users".to_string(),
                index: index("f0999", "f0999", true, false),
            })
            .unwrap();
        verify.apply(&put("b", Json::obj())).unwrap();
        assert!(verify
            .apply(&put("c", Json::obj().with("f0999", Json::str("present")),))
            .is_err());
    }
}
