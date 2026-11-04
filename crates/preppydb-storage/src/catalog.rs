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
            unique: value
                .get("unique")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            sparse: value
                .get("sparse")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            created_at: value.get_i64("created_at").unwrap_or(0),
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
        let mut meta = CollectionMeta::new(name, value.get_i64("created_at").unwrap_or(0));
        meta.updated_at = value.get_i64("updated_at").unwrap_or(meta.created_at);
        meta.schema = match value.get("schema") {
            None | Some(Json::Null) => None,
            Some(schema) => Some(schema.clone()),
        };
        meta.max_docs = value.get("max_docs").and_then(|v| v.as_u64());
        meta.max_doc_bytes = value.get("max_doc_bytes").and_then(|v| v.as_u64());
        meta.default_ttl_ms = value.get("default_ttl_ms").and_then(|v| v.as_i64());
        if let Some(indexes) = value.get("indexes").and_then(|v| v.as_array()) {
            for entry in indexes {
                let index = IndexMeta::from_json(entry)?;
                meta.indexes.insert(index.name.clone(), index);
            }
        }
        Ok(meta)
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
}
