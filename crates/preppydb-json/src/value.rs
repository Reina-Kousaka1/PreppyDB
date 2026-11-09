//! The `Json` value type, path access and canonical encodings.

use crate::number::Number;
use crate::MAX_DEPTH;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

/// A JSON value.
///
/// Objects use a `BTreeMap` so that serialisation, comparison and hashing are
/// deterministic regardless of insertion order. That property is what makes the
/// write-ahead log checksums and the secondary index keys stable.
#[derive(Clone, Debug, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

/// The kind of a [`Json`] value, useful in error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonKind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl JsonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JsonKind::Null => "null",
            JsonKind::Bool => "boolean",
            JsonKind::Number => "number",
            JsonKind::String => "string",
            JsonKind::Array => "array",
            JsonKind::Object => "object",
        }
    }
}

/// One step of a document path, e.g. `profile.memories[3].title`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

impl Json {
    // ---------------------------------------------------------------- constructors

    pub fn obj() -> Json {
        Json::Object(BTreeMap::new())
    }

    pub fn array(items: Vec<Json>) -> Json {
        Json::Array(items)
    }

    pub fn str<S: Into<String>>(value: S) -> Json {
        Json::String(value.into())
    }

    pub fn int<I: Into<i64>>(value: I) -> Json {
        Json::Number(Number::Int(value.into()))
    }

    pub fn uint(value: u64) -> Json {
        Json::Number(Number::from(value))
    }

    pub fn float(value: f64) -> Json {
        Json::Number(Number::Float(value))
    }

    /// Insert an object member, replacing any previous value. Returns `self` so that
    /// documents can be built fluently.
    pub fn set<S: Into<String>>(&mut self, key: S, value: Json) -> &mut Json {
        match self {
            Json::Object(map) => {
                map.insert(key.into(), value);
            }
            _ => {
                let mut map = BTreeMap::new();
                map.insert(key.into(), value);
                *self = Json::Object(map);
            }
        }
        self
    }

    /// Builder form of [`Json::set`].
    pub fn with<S: Into<String>>(mut self, key: S, value: Json) -> Json {
        self.set(key, value);
        self
    }

    pub fn push(&mut self, value: Json) -> &mut Json {
        match self {
            Json::Array(items) => items.push(value),
            _ => *self = Json::Array(vec![value]),
        }
        self
    }

    pub fn remove_key(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Object(map) => map.remove(key),
            _ => None,
        }
    }

    // ---------------------------------------------------------------- inspection

    pub fn kind(&self) -> JsonKind {
        match self {
            Json::Null => JsonKind::Null,
            Json::Bool(_) => JsonKind::Bool,
            Json::Number(_) => JsonKind::Number,
            Json::String(_) => JsonKind::String,
            Json::Array(_) => JsonKind::Array,
            Json::Object(_) => JsonKind::Object,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Json::Array(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Json::String(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|v| u64::try_from(v).ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, Json>> {
        match self {
            Json::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Object member access.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object().and_then(|map| map.get(key))
    }

    /// Mutable object member access.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.as_object_mut().and_then(|map| map.get_mut(key))
    }

    /// Object member access returning `Json::Null` for missing members.
    pub fn get_or_null(&self, key: &str) -> &Json {
        static NULL: Json = Json::Null;
        self.get(key).unwrap_or(&NULL)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_i64())
    }

    pub fn len(&self) -> usize {
        match self {
            Json::Array(items) => items.len(),
            Json::Object(map) => map.len(),
            Json::String(s) => s.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ---------------------------------------------------------------- paths

    /// Parse a path expression such as `a.b[2].c` into segments.
    pub fn parse_path(path: &str) -> Result<Vec<PathSegment>, String> {
        let mut segments = Vec::new();
        let mut current = String::new();
        let mut chars = path.chars().peekable();
        let mut started = false;
        while let Some(ch) = chars.next() {
            match ch {
                '.' => {
                    if !current.is_empty() {
                        segments.push(PathSegment::Key(std::mem::take(&mut current)));
                    } else if !started || !matches!(segments.last(), Some(PathSegment::Index(_))) {
                        // Empty segments such as "" , "a..b" or ".a" are invalid, but
                        // "a[0].b" is fine because the index already closed the segment.
                        return Err("empty path segment".to_string());
                    }
                    started = !segments.is_empty();
                }
                '[' => {
                    if !current.is_empty() {
                        segments.push(PathSegment::Key(std::mem::take(&mut current)));
                    }
                    let mut idx = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == ']' {
                            closed = true;
                            break;
                        }
                        idx.push(c);
                    }
                    if !closed {
                        return Err("unterminated '[' in path".to_string());
                    }
                    let parsed: usize = idx
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid array index '{idx}'"))?;
                    segments.push(PathSegment::Index(parsed));
                    started = true;
                }
                other => current.push(other),
            }
        }
        if !current.is_empty() {
            segments.push(PathSegment::Key(current));
        }
        if segments.is_empty() {
            return Err("empty path".to_string());
        }
        Ok(segments)
    }

    /// Look up a value by path expression. Returns `None` when any step is missing.
    pub fn get_path(&self, path: &str) -> Option<&Json> {
        let segments = Json::parse_path(path).ok()?;
        let mut node = self;
        for segment in &segments {
            node = match segment {
                PathSegment::Key(key) => node.get(key)?,
                PathSegment::Index(idx) => node.as_array()?.get(*idx)?,
            };
        }
        Some(node)
    }

    /// Create missing intermediate objects for a path and set the leaf value.
    pub fn set_path(&mut self, path: &str, value: Json) -> Result<(), String> {
        let segments = Json::parse_path(path)?;
        if segments.len() > MAX_DEPTH {
            return Err("path too deep".to_string());
        }
        let mut node = self;
        for (pos, segment) in segments.iter().enumerate() {
            let last = pos + 1 == segments.len();
            match segment {
                PathSegment::Key(key) => {
                    if !node.is_object() {
                        *node = Json::obj();
                    }
                    let map = node.as_object_mut().expect("object ensured above");
                    if last {
                        map.insert(key.clone(), value);
                        return Ok(());
                    }
                    if !map.contains_key(key) {
                        let next_is_index =
                            matches!(segments.get(pos + 1), Some(PathSegment::Index(_)));
                        let filler = if next_is_index {
                            Json::Array(vec![])
                        } else {
                            Json::obj()
                        };
                        map.insert(key.clone(), filler);
                    }
                    node = map.get_mut(key).expect("inserted or present");
                }
                PathSegment::Index(idx) => {
                    if !node.is_array() {
                        *node = Json::Array(Vec::new());
                    }
                    let items = node.as_array_mut().expect("array ensured above");
                    if *idx >= items.len() {
                        // Grow sparsely with nulls; never silently reorder.
                        items.resize(*idx + 1, Json::Null);
                    }
                    if last {
                        items[*idx] = value;
                        return Ok(());
                    }
                    node = &mut items[*idx];
                }
            }
        }
        Ok(())
    }

    /// Remove a value at a path, returning it when present.
    pub fn remove_path(&mut self, path: &str) -> Option<Json> {
        let segments = Json::parse_path(path).ok()?;
        let (last, prefix) = segments.split_last()?;
        let mut node = self;
        for segment in prefix {
            node = match segment {
                PathSegment::Key(key) => node.get_mut(key)?,
                PathSegment::Index(idx) => node.as_array_mut()?.get_mut(*idx)?,
            };
        }
        match last {
            PathSegment::Key(key) => node.remove_key(key),
            PathSegment::Index(idx) => {
                let items = node.as_array_mut()?;
                if *idx < items.len() {
                    Some(items.remove(*idx))
                } else {
                    None
                }
            }
        }
    }

    // ---------------------------------------------------------------- comparison

    /// Structural equality (numeric equality across int/float, key order ignored).
    pub fn deep_eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Bool(a), Json::Bool(b)) => a == b,
            (Json::Number(a), Json::Number(b)) => a == b,
            (Json::String(a), Json::String(b)) => a == b,
            (Json::Array(a), Json::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.deep_eq(y))
            }
            (Json::Object(a), Json::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.get(k).map(|o| v.deep_eq(o)).unwrap_or(false))
            }
            _ => false,
        }
    }

    /// Stable hash used by anti-entropy tooling and tests.
    pub fn deep_hash(&self) -> u64 {
        // FNV-1a over the canonical encoding: simple, deterministic, no allocation
        // beyond the encoding itself.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.canonical_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    // ---------------------------------------------------------------- encodings

    /// Deterministic, order-preserving-ish byte encoding used as secondary index key.
    ///
    /// Type order is null < boolean < number < string < array < object; within a type
    /// the natural order of that type is preserved.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Json::Null => out.push(0),
            Json::Bool(false) => out.push(1),
            Json::Bool(true) => out.push(2),
            Json::Number(n) => {
                out.push(3);
                let bits = n.as_f64();
                let bits = if bits.is_nan() {
                    u64::MAX
                } else {
                    let raw = bits.to_bits();
                    if raw & (1 << 63) != 0 {
                        !raw
                    } else {
                        raw | (1 << 63)
                    }
                };
                out.extend_from_slice(&bits.to_be_bytes());
            }
            Json::String(s) => {
                out.push(4);
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Json::Array(items) => {
                out.push(5);
                for item in items {
                    item.encode_into(out);
                    out.push(0x1f);
                }
                out.push(0);
            }
            Json::Object(map) => {
                out.push(6);
                for (key, value) in map {
                    out.extend_from_slice(key.as_bytes());
                    out.push(0);
                    value.encode_into(out);
                    out.push(0x1f);
                }
                out.push(0);
            }
        }
    }

    // ---------------------------------------------------------------- serialisation

    pub fn write_compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(n) => out.push_str(&n.to_string()),
            Json::String(s) => write_escaped(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Json::Object(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(key, out);
                    out.push(':');
                    value.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    /// Serialize compact JSON directly to an I/O sink without materializing the
    /// complete encoded value in an intermediate `String`.
    pub fn write_compact_to<W: std::io::Write>(&self, out: &mut W) -> std::io::Result<()> {
        match self {
            Json::Null => out.write_all(b"null"),
            Json::Bool(true) => out.write_all(b"true"),
            Json::Bool(false) => out.write_all(b"false"),
            Json::Number(number) => out.write_all(number.to_string().as_bytes()),
            Json::String(value) => write_escaped_to(value, out),
            Json::Array(items) => {
                out.write_all(b"[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",")?;
                    }
                    item.write_compact_to(out)?;
                }
                out.write_all(b"]")
            }
            Json::Object(map) => {
                out.write_all(b"{")?;
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",")?;
                    }
                    write_escaped_to(key, out)?;
                    out.write_all(b":")?;
                    value.write_compact_to(out)?;
                }
                out.write_all(b"}")
            }
        }
    }

    pub fn write_pretty(&self, out: &mut String, indent: usize) {
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    push_indent(out, indent + 1);
                    item.write_pretty(out, indent + 1);
                }
                out.push('\n');
                push_indent(out, indent);
                out.push(']');
            }
            Json::Object(map) if !map.is_empty() => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    push_indent(out, indent + 1);
                    write_escaped(key, out);
                    out.push_str(": ");
                    value.write_pretty(out, indent + 1);
                }
                out.push('\n');
                push_indent(out, indent);
                out.push('}');
            }
            other => other.write_compact(out),
        }
    }
}

fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_escaped(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_escaped_to<W: std::io::Write>(text: &str, out: &mut W) -> std::io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.write_all(b"\"")?;
    let bytes = text.as_bytes();
    let mut run_start = 0;
    for (index, byte) in bytes.iter().copied().enumerate() {
        let escaped: Option<&[u8]> = match byte {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            0x08 => Some(b"\\b"),
            0x0c => Some(b"\\f"),
            _ => None,
        };
        if let Some(escaped) = escaped {
            out.write_all(&bytes[run_start..index])?;
            out.write_all(escaped)?;
            run_start = index + 1;
        } else if byte < 0x20 {
            out.write_all(&bytes[run_start..index])?;
            out.write_all(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(byte >> 4) as usize],
                HEX[(byte & 0x0f) as usize],
            ])?;
            run_start = index + 1;
        }
    }
    out.write_all(&bytes[run_start..])?;
    out.write_all(b"\"")
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = String::new();
        self.write_compact(&mut buf);
        f.write_str(&buf)
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        self.deep_eq(other)
    }
}

impl Eq for Json {}

impl PartialOrd for Json {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Json {
    fn cmp(&self, other: &Self) -> Ordering {
        self.canonical_bytes().cmp(&other.canonical_bytes())
    }
}

impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}

impl From<i64> for Json {
    fn from(v: i64) -> Self {
        Json::int(v)
    }
}

impl From<u64> for Json {
    fn from(v: u64) -> Self {
        Json::uint(v)
    }
}

impl From<usize> for Json {
    fn from(v: usize) -> Self {
        Json::uint(v as u64)
    }
}

impl From<f64> for Json {
    fn from(v: f64) -> Self {
        Json::float(v)
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::String(v.to_string())
    }
}

impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::String(v)
    }
}

impl From<Vec<Json>> for Json {
    fn from(v: Vec<Json>) -> Self {
        Json::Array(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_compact_serialization_matches_string_serializer() {
        let value = Json::obj()
            .with(
                "escaped\"field",
                Json::str("quote\" slash\\ line\n tab\t\u{0001} 🌊"),
            )
            .with(
                "nested",
                Json::array(vec![Json::Null, Json::Bool(true), Json::int(-12)]),
            );
        let expected = value.to_string();
        let mut output = Vec::new();

        value.write_compact_to(&mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[test]
    fn path_parsing() {
        assert_eq!(
            Json::parse_path("a.b[2].c").unwrap(),
            vec![
                PathSegment::Key("a".into()),
                PathSegment::Key("b".into()),
                PathSegment::Index(2),
                PathSegment::Key("c".into()),
            ]
        );
        assert_eq!(Json::parse_path("items[0]").unwrap().len(), 2);
        assert!(Json::parse_path("").is_err());
        assert!(Json::parse_path("a[x]").is_err());
    }

    #[test]
    fn set_and_get_path_roundtrip() {
        let mut doc = Json::obj();
        doc.set_path("profile.memories[1].title", Json::str("hello"))
            .unwrap();
        assert_eq!(
            doc.get_path("profile.memories[1].title").unwrap().as_str(),
            Some("hello")
        );
        assert!(doc
            .get_path("profile.memories[1].title")
            .unwrap()
            .is_string());
        assert!(doc.get_path("profile.missing").is_none());
        assert_eq!(
            doc.remove_path("profile.memories[1].title")
                .unwrap()
                .as_str(),
            Some("hello")
        );
        assert!(doc.get_path("profile.memories[1].title").is_none());
    }

    #[test]
    fn canonical_order_numbers() {
        let mut values = [Json::int(2), Json::int(-5), Json::float(0.5), Json::int(0)];
        values.sort();
        let nums: Vec<f64> = values.iter().map(|v| v.as_f64().unwrap()).collect();
        assert_eq!(nums, vec![-5.0, 0.0, 0.5, 2.0]);
    }

    #[test]
    fn canonical_order_types() {
        let mut values = [
            Json::str("a"),
            Json::Bool(true),
            Json::Null,
            Json::Array(vec![]),
            Json::obj(),
            Json::int(1),
        ];
        values.sort();
        assert!(matches!(values[0], Json::Null));
        assert!(matches!(values[1], Json::Bool(true)));
        assert!(matches!(values[2], Json::Number(_)));
        assert!(matches!(values[3], Json::String(_)));
        assert!(matches!(values[4], Json::Array(_)));
        assert!(matches!(values[5], Json::Object(_)));
    }

    #[test]
    fn deep_equal_ignores_number_representation() {
        let a = Json::obj().with("x", Json::float(1.0));
        let b = Json::obj().with("x", Json::int(1));
        assert!(a.deep_eq(&b));
        assert_eq!(a.deep_hash(), b.deep_hash());
    }

    #[test]
    fn pretty_print_is_stable() {
        let doc = Json::obj()
            .with("b", Json::int(2))
            .with("a", Json::Array(vec![Json::int(1)]));
        let pretty = crate::to_string_pretty(&doc);
        assert!(pretty.starts_with('{'));
        assert!(pretty.contains("\"a\": ["));
        // Key order is sorted, not insertion order.
        assert!(pretty.find("\"a\"").unwrap() < pretty.find("\"b\"").unwrap());
        assert_eq!(crate::parse(&pretty).unwrap(), doc);
    }
}
