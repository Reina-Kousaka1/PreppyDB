//! Bounded, storage-independent Lua execution.
//!
//! A script computes a result and a list of document mutations. The Lua VM has no
//! database handle, filesystem, network, package loader, or debug library. The
//! caller validates the returned mutation list and submits it in one Rust batch.

use mlua::{ChunkMode, HookTriggers, Lua, LuaOptions, StdLib, Table, UserData, Value, VmState};
use preppydb_json::{Json, Number};
use std::collections::{BTreeMap, HashSet};

pub const MAX_SCRIPT_BYTES: usize = 128 * 1024;
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_OPERATIONS: usize = 1000;
const MAX_VM_MEMORY: usize = 16 * 1024 * 1024;
const MAX_INSTRUCTIONS: u64 = 2_000_000;
const HOOK_GRANULARITY: u64 = 1000;
const MAX_VALUE_NODES: usize = 100_000;
const MAX_VALUE_DEPTH: usize = preppydb_json::MAX_DEPTH;

#[derive(Debug)]
pub struct ScriptOutput {
    pub result: Json,
    pub operations: Vec<Json>,
}

#[derive(Debug)]
pub struct LuaExecutionError(pub String);

impl std::fmt::Display for LuaExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LuaExecutionError {}

struct JsonNull;
impl UserData for JsonNull {}

/// Execute one source chunk against bounded JSON input.
///
/// The source must return `{ result = <JSON value>, operations = { ... } }`.
/// Empty Lua tables are objects; use `preppydb.array()` to express an empty JSON
/// array. The operation schema is intentionally parsed by Rust, not executed by
/// Lua.
pub fn execute(source: &str, input: &Json) -> Result<ScriptOutput, LuaExecutionError> {
    if source.len() > MAX_SCRIPT_BYTES {
        return Err(LuaExecutionError(
            "script exceeds the source size limit".into(),
        ));
    }
    let mut input_json = String::new();
    input.write_compact(&mut input_json);
    if input_json.len() > MAX_INPUT_BYTES {
        return Err(LuaExecutionError(
            "script input exceeds the size limit".into(),
        ));
    }

    let lua = Lua::new_with(StdLib::TABLE | StdLib::MATH, LuaOptions::default())
        .map_err(runtime_error)?;
    lua.set_memory_limit(MAX_VM_MEMORY).map_err(runtime_error)?;

    let instruction_ticks = std::cell::Cell::new(0u64);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_GRANULARITY as u32),
        move |_lua, _debug| {
            let next = instruction_ticks.get().saturating_add(HOOK_GRANULARITY);
            instruction_ticks.set(next);
            if next > MAX_INSTRUCTIONS {
                return Err(mlua::Error::RuntimeError(
                    "Lua instruction budget exceeded".into(),
                ));
            }
            Ok(VmState::Continue)
        },
    )
    .map_err(runtime_error)?;

    let globals = lua.globals();
    // The base library is initialized by Lua. Remove globals that could load code,
    // access files, manipulate metatables, or write unbounded logs.
    for name in [
        "dofile",
        "loadfile",
        "load",
        "collectgarbage",
        "print",
        "getmetatable",
        "setmetatable",
        "rawget",
        "rawset",
        "pcall",
        "xpcall",
        "require",
    ] {
        globals.set(name, Value::Nil).map_err(runtime_error)?;
    }

    let array_metatable = lua.create_table().map_err(runtime_error)?;
    array_metatable
        .set("__metatable", false)
        .map_err(runtime_error)?;

    let library = lua.create_table().map_err(runtime_error)?;
    let array_meta = array_metatable.clone();
    library
        .set(
            "array",
            lua.create_function(move |lua, table: Option<Table>| {
                let table = match table {
                    Some(table) => table,
                    None => lua.create_table()?,
                };
                table.set_metatable(Some(array_meta.clone()))?;
                Ok(table)
            })
            .map_err(runtime_error)?,
        )
        .map_err(runtime_error)?;
    library
        .set(
            "null",
            lua.create_userdata(JsonNull).map_err(runtime_error)?,
        )
        .map_err(runtime_error)?;
    library
        .set(
            "is_null",
            lua.create_function(|_, value: Value| {
                Ok(matches!(value, Value::UserData(data) if data.is::<JsonNull>()))
            })
            .map_err(runtime_error)?,
        )
        .map_err(runtime_error)?;
    globals.set("preppydb", library).map_err(runtime_error)?;

    let mut nodes = 0;
    let input_value =
        json_to_lua(&lua, input, &array_metatable, 0, &mut nodes).map_err(runtime_error)?;
    globals.set("input", input_value).map_err(runtime_error)?;

    let returned = lua
        .load(source)
        .set_name("preppydb_script")
        .set_mode(ChunkMode::Text)
        .eval::<Value>()
        .map_err(runtime_error)?;
    let returned = match returned {
        Value::Table(table) => table,
        _ => return Err(LuaExecutionError("script must return a table".into())),
    };
    let mut nodes = 0;
    let mut stack = HashSet::new();
    let returned_json = lua_to_json(
        Value::Table(returned),
        0,
        &mut nodes,
        &mut stack,
        &array_metatable,
    )
    .map_err(runtime_error)?;
    if returned_json
        .as_object()
        .is_none_or(|fields| fields.len() != 2)
    {
        return Err(LuaExecutionError(
            "script must return only result and operations".into(),
        ));
    }

    let mut output_bytes = String::new();
    returned_json.write_compact(&mut output_bytes);
    if output_bytes.len() > MAX_OUTPUT_BYTES {
        return Err(LuaExecutionError(
            "script output exceeds the size limit".into(),
        ));
    }

    let result = returned_json
        .get("result")
        .cloned()
        .ok_or_else(|| LuaExecutionError("script result is missing 'result'".into()))?;
    let operations = returned_json
        .get("operations")
        .and_then(Json::as_array)
        .ok_or_else(|| LuaExecutionError("script result 'operations' must be an array".into()))?;
    if operations.is_empty() {
        return Err(LuaExecutionError(
            "script must return at least one operation".into(),
        ));
    }
    if operations.len() > MAX_OPERATIONS {
        return Err(LuaExecutionError(
            "script returned too many operations".into(),
        ));
    }
    Ok(ScriptOutput {
        result,
        operations: operations.clone(),
    })
}

fn runtime_error(error: mlua::Error) -> LuaExecutionError {
    // Do not return source snippets or host paths in the public response.
    let message = error.to_string();
    if message.contains("instruction budget exceeded") {
        LuaExecutionError("Lua instruction budget exceeded".into())
    } else if message.contains("memory limit") || message.contains("memory error") {
        LuaExecutionError("Lua memory budget exceeded".into())
    } else {
        LuaExecutionError("Lua script failed or returned an unsupported value".into())
    }
}

fn json_to_lua(
    lua: &Lua,
    value: &Json,
    array_metatable: &Table,
    depth: usize,
    nodes: &mut usize,
) -> mlua::Result<Value> {
    count_node(depth, nodes).map_err(mlua::Error::RuntimeError)?;
    Ok(match value {
        Json::Null => Value::UserData(lua.create_userdata(JsonNull)?),
        Json::Bool(value) => Value::Boolean(*value),
        Json::Number(Number::Int(value)) => Value::Integer(*value),
        Json::Number(Number::Float(value)) if value.is_finite() => Value::Number(*value),
        Json::Number(Number::Float(_)) => {
            return Err(mlua::Error::RuntimeError(
                "non-finite numbers are not supported".into(),
            ))
        }
        Json::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        Json::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            table.set_metatable(Some(array_metatable.clone()))?;
            for (index, item) in values.iter().enumerate() {
                let converted = json_to_lua(lua, item, array_metatable, depth + 1, nodes)?;
                table.raw_set(index + 1, converted)?;
            }
            Value::Table(table)
        }
        Json::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, item) in values {
                let converted = json_to_lua(lua, item, array_metatable, depth + 1, nodes)?;
                table.raw_set(key.as_str(), converted)?;
            }
            Value::Table(table)
        }
    })
}

fn lua_to_json(
    value: Value,
    depth: usize,
    nodes: &mut usize,
    stack: &mut HashSet<usize>,
    array_metatable: &Table,
) -> mlua::Result<Json> {
    count_node(depth, nodes).map_err(mlua::Error::RuntimeError)?;
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(value) => Ok(Json::Bool(value)),
        Value::Integer(value) => Ok(Json::int(value)),
        Value::Number(value) if value.is_finite() => Ok(Json::float(value)),
        Value::Number(_) => Err(mlua::Error::RuntimeError(
            "non-finite numbers are not supported".into(),
        )),
        Value::String(value) => Ok(Json::str(
            value
                .to_str()
                .map_err(|_| mlua::Error::RuntimeError("invalid UTF-8 string".into()))?
                .to_owned(),
        )),
        Value::UserData(value) if value.is::<JsonNull>() => Ok(Json::Null),
        Value::Table(table) => {
            let identity = table.to_pointer() as usize;
            if !stack.insert(identity) {
                return Err(mlua::Error::RuntimeError("cyclic table is not JSON".into()));
            }
            let is_array = table
                .metatable()
                .is_some_and(|meta| meta.equals(array_metatable).unwrap_or(false));
            let mut strings = BTreeMap::new();
            let mut indices = BTreeMap::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, item) = pair?;
                match key {
                    Value::String(key) => {
                        let key = key.to_str().map_err(|_| {
                            mlua::Error::RuntimeError("invalid UTF-8 object key".into())
                        })?;
                        let item = lua_to_json(item, depth + 1, nodes, stack, array_metatable)?;
                        if strings.insert(key.to_owned(), item).is_some() {
                            return Err(mlua::Error::RuntimeError(
                                "duplicate JSON object key".into(),
                            ));
                        }
                    }
                    Value::Integer(index) if index > 0 => {
                        let item = lua_to_json(item, depth + 1, nodes, stack, array_metatable)?;
                        let index = usize::try_from(index).map_err(|_| {
                            mlua::Error::RuntimeError("JSON array index is out of bounds".into())
                        })?;
                        indices.insert(index, item);
                    }
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "JSON table keys must be strings or contiguous positive integers"
                                .into(),
                        ))
                    }
                }
            }
            stack.remove(&identity);
            if !strings.is_empty() && !indices.is_empty() {
                return Err(mlua::Error::RuntimeError(
                    "mixed object and array table is not JSON".into(),
                ));
            }
            if !indices.is_empty() || (is_array && strings.is_empty()) {
                if !strings.is_empty() {
                    return Err(mlua::Error::RuntimeError(
                        "marked JSON array contains object keys".into(),
                    ));
                }
                let mut values = Vec::with_capacity(indices.len());
                for expected in 1..=indices.len() {
                    values.push(indices.remove(&expected).ok_or_else(|| {
                        mlua::Error::RuntimeError("JSON array has a missing index".into())
                    })?);
                }
                if !indices.is_empty() {
                    return Err(mlua::Error::RuntimeError(
                        "JSON array index overflow".into(),
                    ));
                }
                Ok(Json::Array(values))
            } else if indices.is_empty() {
                Ok(Json::Object(strings))
            } else {
                Err(mlua::Error::RuntimeError("invalid JSON table".into()))
            }
        }
        _ => Err(mlua::Error::RuntimeError(
            "functions, threads, and non-JSON Lua values are unsupported".into(),
        )),
    }
}

fn count_node(depth: usize, nodes: &mut usize) -> Result<(), String> {
    if depth > MAX_VALUE_DEPTH {
        return Err("JSON value nesting exceeds the limit".into());
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| "JSON value node count overflow".to_string())?;
    if *nodes > MAX_VALUE_NODES {
        return Err("JSON value exceeds the node limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_bounded_script_and_preserves_json_shapes() {
        let input = Json::obj()
            .with("balance", Json::int(100))
            .with("items", Json::Array(vec![Json::Null, Json::int(2)]));
        let output = execute(
            r#"
                return {
                    result = { balance = input.balance + 1, items = input.items, null_ok = preppydb.is_null(input.items[1]) },
                    operations = preppydb.array({
                        { op = "put", collection = "wallets", key = "u1", expected_version = 0, value = { balance = input.balance + 1 } }
                    })
                }
            "#,
            &input,
        )
        .unwrap();
        assert_eq!(output.result.get_i64("balance"), Some(101));
        assert_eq!(
            output.result.get("null_ok").and_then(Json::as_bool),
            Some(true)
        );
        assert_eq!(
            output
                .result
                .get("items")
                .and_then(Json::as_array)
                .unwrap()
                .len(),
            2
        );
        assert!(output.result.get("items").and_then(Json::as_array).unwrap()[0].is_null());
        assert_eq!(output.operations.len(), 1);
    }

    #[test]
    fn instruction_budget_stops_infinite_loops() {
        let error = execute(
            "while true do end; return { result = true, operations = {} }",
            &Json::obj(),
        )
        .unwrap_err();
        assert_eq!(error.0, "Lua instruction budget exceeded");
    }

    #[test]
    fn script_cannot_catch_the_instruction_budget() {
        let output = execute(
            "return {result = (pcall == nil and xpcall == nil), operations = preppydb.array({{op='put',collection='c',key='k',expected_version=0,value={}}})}",
            &Json::obj(),
        )
        .unwrap();
        assert_eq!(output.result, Json::Bool(true));
    }

    #[test]
    fn memory_budget_stops_unbounded_table_growth() {
        let error = execute(
            "local t = {}; for i = 1, 1000000 do t[i] = {i, i, i, i, i, i, i, i} end; return {result=true, operations=preppydb.array({{op='put',collection='c',key='k',expected_version=0,value={}}})}",
            &Json::obj(),
        )
        .unwrap_err();
        assert!(error.0.contains("failed") || error.0.contains("memory"));
    }

    #[test]
    fn filesystem_and_package_globals_are_unavailable() {
        let output = execute(
            "return { result = (io == nil and os == nil and package == nil and dofile == nil), operations = preppydb.array({{op='put',collection='c',key='k',expected_version=0,value={}}}) }",
            &Json::obj(),
        )
        .unwrap();
        assert_eq!(output.result, Json::Bool(true));
    }

    #[test]
    fn rejects_cycles_and_bad_operation_container() {
        assert!(execute(
            "local t = {}; t.self = t; return { result=t, operations=preppydb.array({{op='put',collection='c',key='k',expected_version=0,value={}}}) }",
            &Json::obj(),
        )
        .is_err());
        assert!(execute("return { result=true, operations={} }", &Json::obj(),).is_err());
    }
}
