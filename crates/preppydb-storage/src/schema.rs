//! A deliberately small JSON-schema subset used to validate documents on write.
//!
//! Supported keywords (all optional):
//! * `type`: `"object" | "array" | "string" | "number" | "integer" | "boolean" | "null"`
//!   or an array of those names.
//! * `required`: array of member names (objects only).
//! * `properties`: per-member schemas (applied when the member is present).
//! * `additionalProperties`: `false` rejects unlisted members (default `true`).
//! * `items`: schema applied to every array element.
//! * `enum`: allowed values (compared structurally).
//! * `minimum` / `maximum`: numeric bounds (numbers and integers).
//! * `minLength` / `maxLength`: string length bounds in characters.
//! * `minItems` / `maxItems`: array length bounds.
//!
//! Anything else is rejected at collection-creation time (`unknown keyword`), so a typo
//! cannot silently disable validation.

use crate::error::{DbError, Result};
use preppydb_json::Json;

const SUPPORTED: &[&str] = &[
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "description",
    "title",
];

/// Validate a schema definition itself.
pub fn validate_schema(schema: &Json, path: &str, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err(DbError::validation("schema nesting too deep"));
    }
    let map = schema
        .as_object()
        .ok_or_else(|| DbError::validation(format!("schema at '{path}' must be an object")))?;
    for key in map.keys() {
        if !SUPPORTED.contains(&key.as_str()) {
            return Err(DbError::validation(format!(
                "unsupported schema keyword '{key}' at '{path}'"
            )));
        }
    }
    if let Some(types) = map.get("type") {
        check_types(types, path)?;
    }
    if let Some(required) = map.get("required") {
        let items = required.as_array().ok_or_else(|| {
            DbError::validation(format!("'required' at '{path}' must be an array"))
        })?;
        for item in items {
            if item.as_str().is_none() {
                return Err(DbError::validation(format!(
                    "'required' entries at '{path}' must be strings"
                )));
            }
        }
    }
    if let Some(props) = map.get("properties") {
        let props = props.as_object().ok_or_else(|| {
            DbError::validation(format!("'properties' at '{path}' must be an object"))
        })?;
        for (name, sub) in props {
            validate_schema(sub, &format!("{path}.{name}"), depth + 1)?;
        }
    }
    if let Some(additional) = map.get("additionalProperties") {
        match additional {
            Json::Bool(true) | Json::Bool(false) => {}
            _ => {
                return Err(DbError::validation(format!(
                    "'additionalProperties' at '{path}' must be a boolean"
                )))
            }
        }
    }
    if let Some(items) = map.get("items") {
        validate_schema(items, &format!("{path}[]"), depth + 1)?;
    }
    for key in ["minimum", "maximum"] {
        if let Some(value) = map.get(key) {
            if value.as_f64().is_none() {
                return Err(DbError::validation(format!(
                    "'{key}' at '{path}' must be a number"
                )));
            }
        }
    }
    for key in ["minLength", "maxLength", "minItems", "maxItems"] {
        if let Some(value) = map.get(key) {
            match value.as_i64() {
                Some(v) if v >= 0 => {}
                _ => {
                    return Err(DbError::validation(format!(
                        "'{key}' at '{path}' must be a non-negative integer"
                    )))
                }
            }
        }
    }
    if let Some(Json::Array(values)) = map.get("enum") {
        if values.is_empty() {
            return Err(DbError::validation(format!(
                "'enum' at '{path}' must not be empty"
            )));
        }
    } else if map.contains_key("enum") {
        return Err(DbError::validation(format!(
            "'enum' at '{path}' must be an array"
        )));
    }
    Ok(())
}

fn check_types(types: &Json, path: &str) -> Result<()> {
    let names: Vec<&Json> = match types {
        Json::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for name in names {
        let text = name
            .as_str()
            .ok_or_else(|| DbError::validation(format!("'type' at '{path}' must be a string")))?;
        if !matches!(
            text,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        ) {
            return Err(DbError::validation(format!(
                "unknown type '{text}' at '{path}'"
            )));
        }
    }
    Ok(())
}

/// Validate `value` against `schema`, returning a descriptive error on the first
/// violation. The error message includes the JSON path of the offending value.
pub fn validate_value(value: &Json, schema: &Json) -> Result<()> {
    validate_impl(value, schema, "$")
}

fn validate_impl(value: &Json, schema: &Json, path: &str) -> Result<()> {
    let map = match schema.as_object() {
        Some(map) => map,
        None => return Ok(()),
    };

    if let Some(types) = map.get("type") {
        if !type_matches(value, types) {
            return Err(DbError::validation(format!(
                "{path}: expected {}, got {}",
                type_names(types).join(" or "),
                value.kind().as_str()
            )));
        }
    }

    if let Some(Json::Array(allowed)) = map.get("enum") {
        if !allowed.iter().any(|candidate| candidate.deep_eq(value)) {
            return Err(DbError::validation(format!(
                "{path}: value is not one of the allowed enum values"
            )));
        }
    }

    if let (Some(min), Some(max)) = (map.get("minimum"), map.get("maximum")) {
        if let (Some(min), Some(max), Some(actual)) = (min.as_f64(), max.as_f64(), value.as_f64()) {
            if actual < min || actual > max {
                return Err(DbError::validation(format!(
                    "{path}: {actual} outside range [{min}, {max}]"
                )));
            }
        }
    } else if let Some(min) = map.get("minimum") {
        if let (Some(min), Some(actual)) = (min.as_f64(), value.as_f64()) {
            if actual < min {
                return Err(DbError::validation(format!(
                    "{path}: {actual} < minimum {min}"
                )));
            }
        }
    } else if let Some(max) = map.get("maximum") {
        if let (Some(max), Some(actual)) = (max.as_f64(), value.as_f64()) {
            if actual > max {
                return Err(DbError::validation(format!(
                    "{path}: {actual} > maximum {max}"
                )));
            }
        }
    }

    if let Some(text) = value.as_str() {
        let chars = text.chars().count() as i64;
        if let Some(min) = map.get("minLength").and_then(|v| v.as_i64()) {
            if chars < min {
                return Err(DbError::validation(format!(
                    "{path}: string shorter than {min} characters"
                )));
            }
        }
        if let Some(max) = map.get("maxLength").and_then(|v| v.as_i64()) {
            if chars > max {
                return Err(DbError::validation(format!(
                    "{path}: string longer than {max} characters"
                )));
            }
        }
    }

    if let Some(items) = value.as_array() {
        let len = items.len() as i64;
        if let Some(min) = map.get("minItems").and_then(|v| v.as_i64()) {
            if len < min {
                return Err(DbError::validation(format!(
                    "{path}: array has {len} items, minimum is {min}"
                )));
            }
        }
        if let Some(max) = map.get("maxItems").and_then(|v| v.as_i64()) {
            if len > max {
                return Err(DbError::validation(format!(
                    "{path}: array has {len} items, maximum is {max}"
                )));
            }
        }
        if let Some(item_schema) = map.get("items") {
            for (idx, item) in items.iter().enumerate() {
                validate_impl(item, item_schema, &format!("{path}[{idx}]"))?;
            }
        }
    }

    if let Some(members) = value.as_object() {
        if let Some(Json::Array(required)) = map.get("required") {
            for name in required {
                if let Some(name) = name.as_str() {
                    if !members.contains_key(name) {
                        return Err(DbError::validation(format!(
                            "{path}: missing required member '{name}'"
                        )));
                    }
                }
            }
        }
        let properties = map.get("properties").and_then(|v| v.as_object());
        let additional_allowed = match map.get("additionalProperties") {
            Some(Json::Bool(value)) => *value,
            _ => true,
        };
        for (key, member) in members {
            match properties.and_then(|props| props.get(key)) {
                Some(subschema) => validate_impl(member, subschema, &format!("{path}.{key}"))?,
                None => {
                    if !additional_allowed {
                        return Err(DbError::validation(format!(
                            "{path}: unexpected member '{key}'"
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

fn type_matches(value: &Json, types: &Json) -> bool {
    let check = |name: &str| match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.as_f64().is_some(),
        "integer" => value.as_i64().is_some(),
        "boolean" => value.as_bool().is_some(),
        "null" => value.is_null(),
        _ => false,
    };
    match types {
        Json::Array(items) => items.iter().filter_map(|v| v.as_str()).any(check),
        Json::String(name) => check(name),
        _ => true,
    }
}

fn type_names(types: &Json) -> Vec<String> {
    match types {
        Json::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect(),
        Json::String(name) => vec![name.clone()],
        _ => vec!["any".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Json {
        preppydb_json::parse(
            r#"{
                "type": "object",
                "required": ["name"],
                "additionalProperties": false,
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 32},
                    "age": {"type": "integer", "minimum": 0, "maximum": 200},
                    "mood": {"enum": ["happy", "sad"]},
                    "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3}
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn accepts_valid_document() {
        let doc = preppydb_json::parse(r#"{"name":"Molly","age":17,"mood":"happy","tags":["a"]}"#)
            .unwrap();
        validate_schema(&schema(), "$", 0).unwrap();
        validate_value(&doc, &schema()).unwrap();
    }

    #[test]
    fn rejects_missing_required() {
        let doc = preppydb_json::parse(r#"{"age":17}"#).unwrap();
        let err = validate_value(&doc, &schema()).unwrap_err();
        assert!(
            err.to_string().contains("missing required member 'name'"),
            "{err}"
        );
    }

    #[test]
    fn rejects_wrong_type_and_bounds() {
        for (doc, needle) in [
            (r#"{"name":5}"#, "expected string"),
            (r#"{"name":"x","age":300}"#, "outside range"),
            (r#"{"name":"x","mood":"angry"}"#, "enum"),
            (r#"{"name":"x","extra":1}"#, "unexpected member"),
            (r#"{"name":""}"#, "shorter than 1"),
            (r#"{"name":"x","tags":["a","b","c","d"]}"#, "maximum is 3"),
        ] {
            let doc = preppydb_json::parse(doc).unwrap();
            let err = validate_value(&doc, &schema()).unwrap_err();
            assert!(err.to_string().contains(needle), "{doc}: {err}");
        }
    }

    #[test]
    fn schema_errors() {
        for bad in [
            r#"{"type":"strng"}"#,
            r#"{"required":"name"}"#,
            r#"{"propeties":{}}"#,
            r#"{"minLength":-1}"#,
            r#"{"enum":[]}"#,
        ] {
            let schema = preppydb_json::parse(bad).unwrap();
            assert!(validate_schema(&schema, "$", 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn union_types() {
        let schema = preppydb_json::parse(r#"{"type":["string","null"]}"#).unwrap();
        validate_value(&Json::Null, &schema).unwrap();
        validate_value(&Json::str("x"), &schema).unwrap();
        assert!(validate_value(&Json::int(1), &schema).is_err());
    }
}
