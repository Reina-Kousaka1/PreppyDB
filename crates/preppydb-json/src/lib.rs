//! A small, dependency-free JSON implementation.
//!
//! PreppyDB deliberately avoids third-party crates for its wire/document format so
//! that the whole surface of parsing and serialisation can be audited and tested in
//! one place. The implementation is strict (RFC 8259) on input and deterministic on
//! output (objects are ordered by key inside a `BTreeMap`).

mod number;
mod parse;
mod value;

pub use number::Number;
pub use parse::{parse, parse_with_limit, JsonError, ParseLimits};
pub use value::{Json, JsonKind, PathSegment};

/// Maximum nesting depth accepted by the parser and by [`Json::set_path`].
pub const MAX_DEPTH: usize = 64;

/// Serialise to compact JSON.
pub fn to_string(value: &Json) -> String {
    value.to_string()
}

/// Serialise with two-space indentation.
pub fn to_string_pretty(value: &Json) -> String {
    let mut out = String::new();
    value.write_pretty(&mut out, 0);
    out
}
