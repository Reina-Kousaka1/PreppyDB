//! Validation and execution boundary for proposed document query plans.
//!
//! Planner output is untrusted. Range plans and numeric/null index equality are
//! deliberately rejected until index completeness and comparison semantics are proven.

use preppydb_json::{Json, ParseLimits};
use preppydb_storage::catalog::CollectionMeta;
use preppydb_storage::engine::{DocumentAccess, DocumentFilter, Engine};
use preppydb_storage::store::Doc;

#[derive(Debug, PartialEq, Eq)]
pub enum AccessPath {
    FullScan,
    IndexEquality(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ValidatedPlan {
    pub collection: String,
    pub limit: u64,
    pub access: AccessPath,
    filter: Json,
}

/// Execute a previously validated plan. Authorization is the caller's responsibility.
/// The storage engine rechecks index metadata and the full filter under its read lock.
pub fn execute_plan(
    engine: &Engine,
    plan: &ValidatedPlan,
    query: &Json,
) -> Result<Vec<(String, Doc)>, String> {
    if query.get("version").and_then(Json::as_u64) != Some(1)
        || query.get("filter").unwrap_or(&Json::Null) != &plan.filter
        || query.get_str("collection") != Some(plan.collection.as_str())
        || query.get("limit").map_or(Some(100), Json::as_u64) != Some(plan.limit)
    {
        return Err("query changed after plan validation".into());
    }
    let filter = match query.get("filter") {
        None | Some(Json::Null) => None,
        Some(value) => Some(DocumentFilter {
            field: value
                .get_str("field")
                .ok_or("missing filter field")?
                .to_string(),
            operator: value
                .get_str("op")
                .ok_or("missing filter operator")?
                .to_string(),
            value: value.get("value").ok_or("missing filter value")?.clone(),
        }),
    };
    let access = match &plan.access {
        AccessPath::FullScan => DocumentAccess::FullScan,
        AccessPath::IndexEquality(name) => DocumentAccess::IndexEquality(name.clone()),
    };
    let limit = usize::try_from(plan.limit).map_err(|_| "invalid query limit")?;
    engine
        .query_documents(&plan.collection, filter.as_ref(), &access, limit)
        .map_err(|err| err.to_string())
}

/// Validate a bounded planner response against the original query and live metadata.
pub fn validate_plan(
    raw: &str,
    original: &Json,
    metadata: &CollectionMeta,
) -> Result<ValidatedPlan, &'static str> {
    let limits = ParseLimits {
        max_bytes: 65_536,
        max_depth: 16,
        max_string_len: 16_384,
        max_items: 1024,
    };
    let plan = preppydb_json::parse_with_limit(raw, limits).map_err(|_| "malformed plan")?;
    if original.get("version").and_then(Json::as_u64) != Some(1)
        || plan.get("version").and_then(Json::as_u64) != Some(1)
    {
        return Err("unsupported version");
    }
    let collection = original.get_str("collection").ok_or("missing collection")?;
    if collection != metadata.name || plan.get_str("collection") != Some(collection) {
        return Err("collection mismatch");
    }
    let limit = match original.get("limit") {
        Some(value) => value.as_u64().ok_or("invalid limit")?,
        None => 100,
    };
    if !(1..=1000).contains(&limit) || plan.get("limit").and_then(Json::as_u64) != Some(limit) {
        return Err("limit mismatch");
    }
    if plan.get("recheck").and_then(Json::as_bool) != Some(true) {
        return Err("filter recheck required");
    }
    let null = Json::Null;
    let filter = original.get("filter").unwrap_or(&null);
    if plan.get("filter").unwrap_or(&null) != filter {
        return Err("filter mismatch");
    }
    if !matches!(filter, Json::Null) {
        let field = filter.get_str("field").ok_or("invalid filter field")?;
        if field.len() > 128 || Json::parse_path(field).is_err() {
            return Err("invalid filter field");
        }
        if !matches!(filter.get_str("op"), Some("eq" | "gte" | "lte")) {
            return Err("invalid filter operator");
        }
        match filter.get("value") {
            Some(Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_)) => (),
            _ => return Err("invalid filter value"),
        }
    }
    let access = match plan.get_str("strategy") {
        Some("full_scan") if matches!(plan.get("index"), None | Some(Json::Null)) => {
            AccessPath::FullScan
        }
        Some("index_equality") => {
            if filter.get_str("op") != Some("eq")
                || !matches!(filter.get("value"), Some(Json::String(_) | Json::Bool(_)))
            {
                return Err("unsupported indexed comparison");
            }
            let name = plan.get_str("index").ok_or("missing index")?;
            let index = metadata.indexes.get(name).ok_or("unknown index")?;
            if Some(index.field.as_str()) != filter.get_str("field") {
                return Err("index field mismatch");
            }
            AccessPath::IndexEquality(name.to_string())
        }
        _ => return Err("unsupported access path"),
    };
    Ok(ValidatedPlan {
        collection: collection.to_string(),
        limit,
        access,
        filter: filter.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use preppydb_storage::catalog::IndexMeta;

    #[test]
    fn rejects_altered_filter_untrusted_indexes_and_range_plans() {
        let query = preppydb_json::parse(r#"{"version":1,"collection":"users","filter":{"field":"name","op":"eq","value":"A"},"limit":10}"#).unwrap();
        let mut meta = CollectionMeta::new("users", 0);
        meta.indexes.insert(
            "names".into(),
            IndexMeta {
                name: "names".into(),
                field: "name".into(),
                unique: false,
                sparse: true,
                created_at: 0,
            },
        );
        let valid = r#"{"version":1,"collection":"users","filter":{"field":"name","op":"eq","value":"A"},"limit":10,"strategy":"index_equality","index":"names","recheck":true}"#;
        assert_eq!(
            validate_plan(valid, &query, &meta).unwrap().access,
            AccessPath::IndexEquality("names".into())
        );
        for bad in [
            valid.replace("\"A\"", "\"B\""),
            valid.replace("names", "missing"),
            valid.replace("index_equality", "index_range"),
            valid.replace("true", "false"),
            valid.replace("\"limit\":10", "\"limit\":11"),
        ] {
            assert!(validate_plan(&bad, &query, &meta).is_err());
        }
        assert!(validate_plan(&" ".repeat(65_537), &query, &meta).is_err());
    }
}
