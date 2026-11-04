//! The optional *Preppy personality model*: a set of general-purpose collections and
//! document schemas for personality profiles, memories, diary entries, virtual items,
//! themes, shop catalogues and pricing history.
//!
//! Design rules:
//!
//! * The model never touches an "economy balance" or any external database. There is no
//!   collection for money the database could mutate, and creating one of the names in
//!   [`FORBIDDEN_COLLECTIONS`] is rejected by the API. Purchases are append-only
//!   ([`IMMUTABLE_COLLECTIONS`]) so recorded history cannot be rewritten.
//! * Every collection has a schema, so bad documents are rejected at write time instead
//!   of corrupting data.
//! * The whole model is optional: PreppyDB works as a plain document store without ever
//!   calling [`bootstrap`].

use preppydb_json::Json;
use preppydb_storage::catalog::{CollectionMeta, IndexMeta};
use preppydb_storage::engine::Engine;
use preppydb_storage::error::Result;

/// Version of the model definitions below (bump when a schema changes).
pub const MODEL_VERSION: &str = "1.0.0";

/// Collections that store records which must never be rewritten.
pub const IMMUTABLE_COLLECTIONS: &[&str] = &["purchases", "price_history"];

/// Names that PreppyDB refuses to create: it never manages real or virtual currency
/// balances, and it never writes to another database's economy tables.
pub const FORBIDDEN_COLLECTIONS: &[&str] = &[
    "economy_balances",
    "wallet",
    "wallets",
    "balance",
    "balances",
    "credit_ledger",
    "debit_ledger",
];

/// One index definition of a collection spec.
#[derive(Clone, Debug)]
pub struct IndexSpec {
    pub name: &'static str,
    pub field: &'static str,
    pub unique: bool,
}

/// Declaration of one collection in the personality model.
#[derive(Clone, Debug)]
pub struct CollectionSpec {
    pub name: &'static str,
    pub schema: &'static str,
    pub max_docs: Option<u64>,
    pub default_ttl_ms: Option<i64>,
    pub indexes: &'static [IndexSpec],
}

impl CollectionSpec {
    pub fn schema_json(&self) -> Json {
        preppydb_json::parse(self.schema).expect("model schemas are valid JSON literals")
    }

    pub fn collection_meta(&self, created_at: i64) -> CollectionMeta {
        let mut meta = CollectionMeta::new(self.name, created_at);
        meta.schema = Some(self.schema_json());
        meta.max_docs = self.max_docs;
        meta.default_ttl_ms = self.default_ttl_ms;
        meta
    }

    pub fn index_metas(&self, created_at: i64) -> Vec<IndexMeta> {
        self.indexes
            .iter()
            .map(|spec| IndexMeta {
                name: spec.name.to_string(),
                field: spec.field.to_string(),
                unique: spec.unique,
                sparse: true,
                created_at,
            })
            .collect()
    }
}

/// All collections of the personality model.
pub fn collections() -> Vec<CollectionSpec> {
    vec![
        CollectionSpec {
            name: "personality_profiles",
            schema: r#"{
                "type": "object",
                "required": ["name", "version"],
                "additionalProperties": true,
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "pronouns": {"type": "string", "maxLength": 64},
                    "traits": {"type": "array", "items": {"type": "string", "maxLength": 128}, "maxItems": 128},
                    "backstory": {"type": "string", "maxLength": 20000},
                    "voice": {"type": "object"},
                    "version": {"type": "integer", "minimum": 1},
                    "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 64}
                }
            }"#,
            max_docs: Some(1_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_name",
                field: "name",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "memories",
            schema: r#"{
                "type": "object",
                "required": ["profile_id", "kind", "content"],
                "properties": {
                    "profile_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "kind": {"enum": ["episodic", "semantic", "preference", "goal", "note"]},
                    "content": {"type": "string", "minLength": 1, "maxLength": 20000},
                    "importance": {"type": "integer", "minimum": 0, "maximum": 100},
                    "occurred_at": {"type": "integer"},
                    "tags": {"type":"array","items":{"type":"string","minLength":1,"maxLength":64},"maxItems":64}
                }
            }"#,
            max_docs: Some(2_000_000),
            default_ttl_ms: None,
            indexes: &[
                IndexSpec {
                    name: "by_profile",
                    field: "profile_id",
                    unique: false,
                },
                IndexSpec {
                    name: "by_kind",
                    field: "kind",
                    unique: false,
                },
            ],
        },
        CollectionSpec {
            name: "preferences",
            schema: r#"{
                "type": "object",
                "required": ["profile_id", "subject", "value"],
                "properties": {
                    "profile_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "subject": {"type": "string", "minLength": 1, "maxLength": 256},
                    "value": {},
                    "weight": {"type": "integer", "minimum": 0, "maximum": 100},
                    "source": {"type": "string", "maxLength": 256},
                    "updated_at": {"type": "integer"}
                }
            }"#,
            max_docs: Some(1_000_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_subject",
                field: "subject",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "virtual_items",
            schema: r#"{
                "type": "object",
                "required": ["name", "kind"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "kind": {"enum": ["clothing", "accessory", "furniture", "food", "tool", "keepsake", "consumable"]},
                    "description": {"type": "string", "maxLength": 4000},
                    "rarity": {"enum": ["common", "uncommon", "rare", "epic", "legendary"]},
                    "tags": {"type":"array","items":{"type":"string","minLength":1,"maxLength":64},"maxItems":64},
                    "theme_id": {"type": "string", "maxLength": 128},
                    "stackable": {"type": "boolean"}
                }
            }"#,
            max_docs: Some(500_000),
            default_ttl_ms: None,
            indexes: &[
                IndexSpec {
                    name: "by_rarity",
                    field: "rarity",
                    unique: false,
                },
                IndexSpec {
                    name: "by_kind",
                    field: "kind",
                    unique: false,
                },
            ],
        },
        CollectionSpec {
            name: "virtual_collections",
            schema: r#"{
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "owner_id": {"type": "string", "maxLength": 128},
                    "item_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 10000},
                    "description": {"type": "string", "maxLength": 4000}
                }
            }"#,
            max_docs: Some(100_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_owner",
                field: "owner_id",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "diary_entries",
            schema: r#"{
                "type": "object",
                "required": ["profile_id", "title", "body", "created_at"],
                "properties": {
                    "profile_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "title": {"type": "string", "minLength": 1, "maxLength": 256},
                    "body": {"type": "string", "minLength": 1, "maxLength": 100000},
                    "mood": {"type": "string", "maxLength": 64},
                    "created_at": {"type": "integer"},
                    "tags": {"type":"array","items":{"type":"string","minLength":1,"maxLength":64},"maxItems":64},
                    "private": {"type": "boolean"}
                }
            }"#,
            max_docs: Some(1_000_000),
            default_ttl_ms: None,
            indexes: &[
                IndexSpec {
                    name: "by_profile",
                    field: "profile_id",
                    unique: false,
                },
                IndexSpec {
                    name: "by_created",
                    field: "created_at",
                    unique: false,
                },
            ],
        },
        CollectionSpec {
            name: "events",
            schema: r#"{
                "type": "object",
                "required": ["title", "starts_at"],
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": 256},
                    "starts_at": {"type": "integer"},
                    "ends_at": {"type": "integer"},
                    "location": {"type": "string", "maxLength": 256},
                    "participants": {"type": "array", "items": {"type": "string"}, "maxItems": 256},
                    "status": {"enum": ["planned", "active", "done", "cancelled"]},
                    "notes": {"type": "string", "maxLength": 20000}
                }
            }"#,
            max_docs: Some(1_000_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_starts",
                field: "starts_at",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "schedules",
            schema: r#"{
                "type": "object",
                "required": ["name", "entries"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "profile_id": {"type": "string", "maxLength": 128},
                    "timezone": {"type": "string", "maxLength": 64},
                    "entries": {
                        "type": "array",
                        "maxItems": 512,
                        "items": {
                            "type": "object",
                            "required": ["weekday", "start_minute", "activity"],
                            "properties": {
                                "weekday": {"type": "integer", "minimum": 0, "maximum": 6},
                                "start_minute": {"type": "integer", "minimum": 0, "maximum": 1439},
                                "end_minute": {"type": "integer", "minimum": 0, "maximum": 1439},
                                "activity": {"type": "string", "minLength": 1, "maxLength": 256}
                            }
                        }
                    }
                }
            }"#,
            max_docs: Some(10_000),
            default_ttl_ms: None,
            indexes: &[],
        },
        CollectionSpec {
            name: "themes",
            schema: r#"{
                "type": "object",
                "required": ["name", "palette"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "palette": {"type": "object"},
                    "css_variables": {"type": "object"},
                    "description": {"type": "string", "maxLength": 4000},
                    "created_at": {"type": "integer"},
                    "tags": {"type":"array","items":{"type":"string","minLength":1,"maxLength":64},"maxItems":64}
                }
            }"#,
            max_docs: Some(10_000),
            default_ttl_ms: None,
            indexes: &[],
        },
        CollectionSpec {
            name: "makeover_history",
            schema: r#"{
                "type": "object",
                "required": ["profile_id", "applied_at"],
                "properties": {
                    "profile_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "theme_id": {"type": "string", "maxLength": 128},
                    "applied_at": {"type": "integer"},
                    "notes": {"type": "string", "maxLength": 4000},
                    "changes": {"type": "array", "items": {"type": "object"}, "maxItems": 512}
                }
            }"#,
            max_docs: Some(1_000_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_profile",
                field: "profile_id",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "shop_catalogs",
            schema: r#"{
                "type": "object",
                "required": ["name", "items"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 128},
                    "currency": {"type": "string", "minLength": 1, "maxLength": 16},
                    "active": {"type": "boolean"},
                    "items": {
                        "type": "array",
                        "maxItems": 10000,
                        "items": {
                            "type": "object",
                            "required": ["item_id", "price_cents"],
                            "properties": {
                                "item_id": {"type": "string", "minLength": 1, "maxLength": 128},
                                "price_cents": {"type": "integer", "minimum": 1},
                                "stock": {"type": "integer", "minimum": 0}
                            }
                        }
                    }
                }
            }"#,
            max_docs: Some(1_000),
            default_ttl_ms: None,
            indexes: &[],
        },
        CollectionSpec {
            name: "price_rules",
            schema: r#"{
                "type": "object",
                "required": ["item_id", "min_price_cents", "max_price_cents", "anchor_price_cents"],
                "properties": {
                    "item_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "min_price_cents": {"type": "integer", "minimum": 1},
                    "max_price_cents": {"type": "integer", "minimum": 1},
                    "anchor_price_cents": {"type": "integer", "minimum": 1},
                    "step_cents": {"type": "integer", "minimum": 0},
                    "floor_price_cents": {"type": "integer", "minimum": 1},
                    "version": {"type": "integer", "minimum": 1},
                    "updated_at": {"type": "integer"},
                    "enabled": {"type": "boolean"},
                    "discounts": {"type": "array", "items": {"type": "object"}, "maxItems": 32}
                }
            }"#,
            max_docs: Some(100_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_item",
                field: "item_id",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "price_history",
            schema: r#"{
                "type": "object",
                "required": ["item_id", "price_cents", "rule_version", "at"],
                "properties": {
                    "item_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "price_cents": {"type": "integer", "minimum": 1},
                    "previous_cents": {"type": "integer", "minimum": 1},
                    "rule_version": {"type": "integer", "minimum": 1},
                    "at": {"type": "integer"},
                    "reason": {"type": "string", "maxLength": 256}
                }
            }"#,
            max_docs: Some(5_000_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_item",
                field: "item_id",
                unique: false,
            }],
        },
        CollectionSpec {
            name: "purchases",
            schema: r#"{
                "type": "object",
                "required": ["item_id", "price_cents", "purchased_at", "quantity"],
                "properties": {
                    "item_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "price_cents": {"type": "integer", "minimum": 1},
                    "quantity": {"type": "integer", "minimum": 1},
                    "total_cents": {"type": "integer", "minimum": 1},
                    "offer_id": {"type": "string", "maxLength": 128},
                    "rule_version": {"type": "integer", "minimum": 1},
                    "purchased_at": {"type": "integer"}
                }
            }"#,
            max_docs: Some(5_000_000),
            default_ttl_ms: None,
            indexes: &[IndexSpec {
                name: "by_item",
                field: "item_id",
                unique: false,
            }],
        },
    ]
}

/// Look up a collection spec by name.
pub fn spec(name: &str) -> Option<CollectionSpec> {
    collections().into_iter().find(|spec| spec.name == name)
}

/// Whether a collection stores append-only records.
pub fn is_immutable_collection(name: &str) -> bool {
    IMMUTABLE_COLLECTIONS.contains(&name)
}

/// Whether a collection name is one PreppyDB refuses to manage.
pub fn is_forbidden_collection(name: &str) -> bool {
    FORBIDDEN_COLLECTIONS.contains(&name)
}

/// Create every model collection and index that does not exist yet.
///
/// Idempotent: existing collections are left untouched (their schemas are not
/// overwritten, so local changes are never silently reverted).
pub fn bootstrap(engine: &Engine) -> Result<BootstrapReport> {
    let mut created = Vec::new();
    let mut indexes_created = Vec::new();
    let mut skipped = Vec::new();
    let now = preppydb_storage::clock::now_ms();
    let existing: Vec<String> = engine
        .list_collections()
        .iter()
        .map(|m| m.name.clone())
        .collect();
    for spec in collections() {
        if existing.iter().any(|name| name == spec.name) {
            skipped.push(spec.name.to_string());
            // Still make sure declared indexes exist.
            let meta = engine.collection_meta(spec.name)?;
            for index in spec.index_metas(now) {
                if !meta.indexes.contains_key(&index.name) {
                    engine.create_index(spec.name, index.clone())?;
                    indexes_created.push(format!("{}/{}", spec.name, index.name));
                }
            }
            continue;
        }
        engine.create_collection(spec.collection_meta(now))?;
        created.push(spec.name.to_string());
        for index in spec.index_metas(now) {
            engine.create_index(spec.name, index.clone())?;
            indexes_created.push(format!("{}/{}", spec.name, index.name));
        }
    }
    Ok(BootstrapReport {
        model_version: MODEL_VERSION.to_string(),
        created,
        indexes_created,
        skipped,
    })
}

/// Result of [`bootstrap`].
#[derive(Clone, Debug)]
pub struct BootstrapReport {
    pub model_version: String,
    pub created: Vec<String>,
    pub indexes_created: Vec<String>,
    pub skipped: Vec<String>,
}

impl BootstrapReport {
    pub fn to_json(&self) -> Json {
        let list = |items: &[String]| {
            Json::Array(items.iter().map(|item| Json::str(item.clone())).collect())
        };
        Json::obj()
            .with("model_version", Json::str(self.model_version.clone()))
            .with("created", list(&self.created))
            .with("indexes_created", list(&self.indexes_created))
            .with("skipped", list(&self.skipped))
    }
}

/// Validate a document against the schema of its collection from the model (if any).
pub fn validate_document(collection: &str, document: &Json) -> Result<()> {
    match spec(collection) {
        Some(spec) => {
            let schema = spec.schema_json();
            preppydb_storage::schema::validate_value(document, &schema)
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_schemas_parse_and_validate() {
        let specs = collections();
        assert!(
            specs.len() >= 14,
            "expected the full model, got {}",
            specs.len()
        );
        for spec in &specs {
            preppydb_storage::schema::validate_schema(&spec.schema_json(), spec.name, 0)
                .unwrap_or_else(|err| panic!("schema for {} is invalid: {err}", spec.name));
            preppydb_storage::catalog::validate_name("collection", spec.name).unwrap();
            for index in spec.indexes {
                assert!(!index.name.is_empty());
                preppydb_json::Json::parse_path(index.field).unwrap();
            }
        }
    }

    #[test]
    fn sample_documents_validate() {
        let cases: Vec<(&str, &str)> = vec![
            (
                "personality_profiles",
                r#"{"name":"Molly","version":1,"traits":["curious"],"pronouns":"she/her"}"#,
            ),
            (
                "memories",
                r#"{"profile_id":"molly","kind":"episodic","content":"We planted tulips.","importance":70}"#,
            ),
            (
                "preferences",
                r#"{"profile_id":"molly","subject":"music.genre","value":["chamber pop"],"weight":60}"#,
            ),
            (
                "virtual_items",
                r#"{"name":"Plaid skirt","kind":"clothing","rarity":"uncommon","tags":["preppy"]}"#,
            ),
            (
                "virtual_collections",
                r#"{"name":"Spring set","item_ids":["skirt","blouse"]}"#,
            ),
            (
                "diary_entries",
                r#"{"profile_id":"molly","title":"Tuesday","body":"Sunny all day.","created_at":1700000000000}"#,
            ),
            (
                "events",
                r#"{"title":"Tea","starts_at":1700000000000,"status":"planned"}"#,
            ),
            (
                "schedules",
                r#"{"name":"Weekday","entries":[{"weekday":1,"start_minute":480,"activity":"school"}]}"#,
            ),
            (
                "themes",
                r##"{"name":"Pastel","palette":{"primary":"#ffb6c1"}}"##,
            ),
            (
                "makeover_history",
                r#"{"profile_id":"molly","applied_at":1700000000000}"#,
            ),
            (
                "shop_catalogs",
                r#"{"name":"Spring shop","currency":"tokens","items":[{"item_id":"skirt","price_cents":2500}]}"#,
            ),
            (
                "price_rules",
                r#"{"item_id":"skirt","min_price_cents":2000,"max_price_cents":4000,"anchor_price_cents":2500,"version":1}"#,
            ),
            (
                "price_history",
                r#"{"item_id":"skirt","price_cents":2500,"rule_version":1,"at":1700000000000}"#,
            ),
            (
                "purchases",
                r#"{"item_id":"skirt","price_cents":2500,"quantity":1,"purchased_at":1700000000000}"#,
            ),
        ];
        for (collection, document) in cases {
            let doc = preppydb_json::parse(document).unwrap();
            validate_document(collection, &doc)
                .unwrap_or_else(|err| panic!("{collection} rejected a valid document: {err}"));
        }
    }

    #[test]
    fn invalid_documents_are_rejected() {
        for (collection, document) in [
            ("personality_profiles", r#"{"version":1}"#),
            (
                "memories",
                r#"{"profile_id":"m","kind":"dream","content":"x"}"#,
            ),
            (
                "purchases",
                r#"{"item_id":"x","price_cents":0,"quantity":1,"purchased_at":1}"#,
            ),
            (
                "price_rules",
                r#"{"item_id":"x","min_price_cents":1,"max_price_cents":2}"#,
            ),
            (
                "schedules",
                r#"{"name":"w","entries":[{"weekday":9,"start_minute":1,"activity":"x"}]}"#,
            ),
        ] {
            let doc = preppydb_json::parse(document).unwrap();
            assert!(
                validate_document(collection, &doc).is_err(),
                "{collection} accepted {document}"
            );
        }
    }

    #[test]
    fn immutable_and_forbidden_lists() {
        assert!(IMMUTABLE_COLLECTIONS.contains(&"purchases"));
        assert!(is_immutable_collection("price_history"));
        assert!(!is_immutable_collection("memories"));
        assert!(is_forbidden_collection("economy_balances"));
        assert!(!is_forbidden_collection("memories"));
        // No model collection may collide with a forbidden name.
        for spec in collections() {
            assert!(
                !is_forbidden_collection(spec.name),
                "{} is forbidden",
                spec.name
            );
        }
    }

    #[test]
    fn unknown_collection_has_no_model_schema() {
        assert!(spec("something_else").is_none());
        assert!(validate_document("something_else", &Json::obj()).is_ok());
    }
}
