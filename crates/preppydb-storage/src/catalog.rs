//! Collection catalogue: metadata, index definitions and name rules.

use crate::error::{DbError, Result};
use preppydb_json::Json;
use std::collections::BTreeMap;

/// Metadata for one secondary index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMeta {
    /// Index name, unique per collection.
    pub name: String,
    /// Dotted path into the document (`profile.age`, `tags[0]`, ...).
    pub field: String,
    /// Reject duplicate values when true.
    pub unique: bool,
    /// Skip documents that do not contain the field.
    pub sparse: bool,
    pub created_at: i64,
}

impl IndexMeta {
    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("name", Json::str(self.name.clone()))
            .with("field", Json::str(self.field.clone()))
            .with("unique", Json::Bool(self.unique))
            .with("sparse", Json::Bool(self.sparse))
            .with("created_at", Json::int(self.created_at))
    }

    pub fn from_json(value: &Json) -> Result<IndexMeta> {
        let name = value
            .get_str("name")
            .ok_or_else(|| DbError::validation("index is missing 'name'"))?;
        validate_name("index", name)?;
        let field = value
            .get_str("field")
            .ok_or_else(|| DbError::validation("index is missing 'field'"))?;
        preppydb_json::Json::parse_path(field)
            .map_err(|err| DbError::validation(format!("invalid index field '{field}': {err}")))?;
        Ok(IndexMeta {
            name: name.to_string(),
            field: field.to_string(),
            unique: optional_bool(value, "unique", false)?,
            sparse: optional_bool(value, "sparse", true)?,
            created_at: optional_i64(value, "created_at")?.unwrap_or(0),
        })
    }
}

/// Metadata for one collection.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectionMeta {
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Optional schema (see [`crate::schema`]).
    pub schema: Option<Json>,
    /// Hard cap on document count (enforced on insert).
    pub max_docs: Option<u64>,
    /// Hard cap on a single encoded document size in bytes.
    pub max_doc_bytes: Option<u64>,
    /// Default TTL applied to writes that do not specify one.
    pub default_ttl_ms: Option<i64>,
    pub indexes: BTreeMap<String, IndexMeta>,
}

impl CollectionMeta {
    pub fn new(name: impl Into<String>, created_at: i64) -> CollectionMeta {
        CollectionMeta {
            name: name.into(),
            created_at,
            updated_at: created_at,
            schema: None,
            max_docs: None,
            max_doc_bytes: None,
            default_ttl_ms: None,
            indexes: BTreeMap::new(),
        }
    }

    pub fn to_json(&self) -> Json {
        let indexes = Json::Array(self.indexes.values().map(|idx| idx.to_json()).collect());
        Json::obj()
            .with("name", Json::str(self.name.clone()))
            .with("created_at", Json::int(self.created_at))
            .with("updated_at", Json::int(self.updated_at))
            .with("schema", self.schema.clone().unwrap_or(Json::Null))
            .with(
                "max_docs",
                self.max_docs.map(Json::uint).unwrap_or(Json::Null),
            )
            .with(
                "max_doc_bytes",
                self.max_doc_bytes.map(Json::uint).unwrap_or(Json::Null),
            )
            .with(
                "default_ttl_ms",
                self.default_ttl_ms.map(Json::int).unwrap_or(Json::Null),
            )
            .with("indexes", indexes)
    }

    pub fn from_json(value: &Json) -> Result<CollectionMeta> {
        let name = value
            .get_str("name")
            .ok_or_else(|| DbError::validation("collection is missing 'name'"))?;
        validate_name("collection", name)?;
        let mut meta = CollectionMeta::new(name, optional_i64(value, "created_at")?.unwrap_or(0));
        meta.updated_at = optional_i64(value, "updated_at")?.unwrap_or(meta.created_at);
        meta.schema = match value.get("schema") {
            None | Some(Json::Null) => None,
            Some(schema) => Some(schema.clone()),
        };
        meta.max_docs = optional_u64(value, "max_docs")?;
        meta.max_doc_bytes = optional_u64(value, "max_doc_bytes")?;
        meta.default_ttl_ms = optional_i64(value, "default_ttl_ms")?;
        if let Some(indexes) = value.get("indexes") {
            let indexes = indexes
                .as_array()
                .ok_or_else(|| DbError::corrupt("catalog", "indexes must be an array"))?;
            for entry in indexes {
                let index = IndexMeta::from_json(entry)?;
                if meta.indexes.insert(index.name.clone(), index).is_some() {
                    return Err(DbError::corrupt("catalog", "duplicate index name"));
                }
            }
        }
        Ok(meta)
    }
}

fn optional_u64(value: &Json, field: &str) -> Result<Option<u64>> {
    match value.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| DbError::corrupt("catalog", format!("invalid {field}"))),
    }
}

fn optional_i64(value: &Json, field: &str) -> Result<Option<i64>> {
    match value.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| DbError::corrupt("catalog", format!("invalid {field}"))),
    }
}

fn optional_bool(value: &Json, field: &str, default: bool) -> Result<bool> {
    match value.get(field) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| DbError::corrupt("catalog", format!("invalid {field}"))),
    }
}

/// Names of collections and indexes: 1..=64 chars, `[A-Za-z0-9_-]`.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(DbError::validation(format!(
            "{kind} name must not be empty"
        )));
    }
    if name.len() > 64 {
        return Err(DbError::validation(format!(
            "{kind} name must not exceed 64 bytes (got {})",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(DbError::validation(format!(
            "{kind} name '{name}' may only contain letters, digits, '_' and '-'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for name in ["personality_profiles", "shop-catalog", "A1", "_internal"] {
            assert!(validate_name("collection", name).is_ok(), "{name}");
        }
    }

    #[test]
    fn invalid_names() {
        for name in ["", "has space", "sla/sh", "ümlaut", &"x".repeat(65)] {
            assert!(validate_name("collection", name).is_err(), "{name}");
        }
    }

    #[test]
    fn meta_roundtrip() {
        let mut meta = CollectionMeta::new("profiles", 1000);
        meta.max_docs = Some(500);
        meta.default_ttl_ms = Some(60_000);
        meta.schema = Some(Json::obj().with("type", Json::str("object")));
        meta.indexes.insert(
            "by_name".into(),
            IndexMeta {
                name: "by_name".into(),
                field: "name".into(),
                unique: true,
                sparse: false,
                created_at: 5,
            },
        );
        let encoded = meta.to_json();
        let decoded = CollectionMeta::from_json(&encoded).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn persisted_catalog_rejects_malformed_limits_and_index_flags() {
        let meta = CollectionMeta::new("profiles", 1000).to_json();
        for (field, value) in [
            ("max_docs", Json::str("unbounded")),
            ("max_doc_bytes", Json::str("unbounded")),
            ("default_ttl_ms", Json::str("never")),
            ("indexes", Json::str("none")),
        ] {
            let mut forged = meta.clone();
            forged.set(field, value);
            assert!(CollectionMeta::from_json(&forged).is_err(), "{field}");
        }

        let index = IndexMeta {
            name: "by_name".into(),
            field: "name".into(),
            unique: true,
            sparse: false,
            created_at: 1000,
        }
        .to_json();
        for field in ["unique", "sparse"] {
            let mut forged = index.clone();
            forged.set(field, Json::str("invalid"));
            assert!(IndexMeta::from_json(&forged).is_err(), "{field}");
        }
    }

    #[test]
    fn persisted_catalog_rejects_duplicate_index_names() {
        let index = IndexMeta {
            name: "by_name".into(),
            field: "name".into(),
            unique: true,
            sparse: false,
            created_at: 1000,
        }
        .to_json();
        let mut meta = CollectionMeta::new("profiles", 1000).to_json();
        meta.set("indexes", Json::Array(vec![index.clone(), index]));
        assert!(CollectionMeta::from_json(&meta).is_err());
    }

    #[test]
    fn persisted_catalog_preserves_defaults_for_absent_optional_fields() {
        let meta =
            CollectionMeta::from_json(&Json::obj().with("name", Json::str("profiles"))).unwrap();
        assert_eq!(meta.max_docs, None);
        assert_eq!(meta.max_doc_bytes, None);
        assert_eq!(meta.default_ttl_ms, None);
        assert!(meta.indexes.is_empty());
        let index = IndexMeta::from_json(
            &Json::obj()
                .with("name", Json::str("by_name"))
                .with("field", Json::str("name")),
        )
        .unwrap();
        assert!(!index.unique);
        assert!(index.sparse);
    }
}
