//! Strict RFC 8259 parser with explicit resource limits.

use crate::number::Number;
use crate::value::Json;
use crate::MAX_DEPTH;
use std::collections::BTreeMap;
use std::fmt;

/// Parser resource limits. These are enforced before allocation so that hostile input
/// cannot exhaust the server's memory.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    /// Maximum nesting depth for arrays/objects.
    pub max_depth: usize,
    /// Maximum length (in bytes) of a single decoded string.
    pub max_string_len: usize,
    /// Maximum number of elements in a single array/object.
    pub max_items: usize,
    /// Maximum total input bytes accepted (checked before parsing starts).
    pub max_bytes: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        ParseLimits {
            max_depth: MAX_DEPTH,
            max_string_len: 1 << 20,
            max_items: 100_000,
            max_bytes: 8 << 20,
        }
    }
}

/// A parse error with byte offset and a human readable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub message: String,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JSON error at line {} column {} (byte {}): {}",
            self.line, self.column, self.offset, self.message
        )
    }
}

impl std::error::Error for JsonError {}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    limits: ParseLimits,
}

/// Parse with default limits.
pub fn parse(input: &str) -> Result<Json, JsonError> {
    parse_with_limit(input, ParseLimits::default())
}

/// Parse with explicit limits.
pub fn parse_with_limit(input: &str, limits: ParseLimits) -> Result<Json, JsonError> {
    if input.len() > limits.max_bytes {
        return Err(JsonError {
            message: format!(
                "input of {} bytes exceeds limit of {}",
                input.len(),
                limits.max_bytes
            ),
            offset: 0,
            line: 1,
            column: 1,
        });
    }
    let mut parser = Parser {
        input: input.as_bytes(),
        pos: 0,
        limits,
    };
    parser.skip_whitespace();
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.input.len() {
        return Err(parser.error("trailing characters after JSON value"));
    }
    Ok(value)
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> JsonError {
        let (line, column) = self.line_col(self.pos);
        JsonError {
            message: message.into(),
            offset: self.pos,
            line,
            column,
        }
    }

    fn line_col(&self, offset: usize) -> (usize, usize) {
        let end = offset.min(self.input.len());
        let mut line = 1;
        let mut column = 1;
        for &byte in &self.input[..end] {
            if byte == b'\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), JsonError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(format!("expected '{}'", byte as char)))
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > self.limits.max_depth {
            return Err(self.error(format!(
                "maximum nesting depth {} exceeded",
                self.limits.max_depth
            )));
        }
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => {
                self.expect_keyword(b"null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.expect_keyword(b"true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.expect_keyword(b"false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => Ok(Json::String(self.parse_string()?)),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(other) => Err(self.error(format!("unexpected character '{}'", other as char))),
        }
    }

    fn expect_keyword(&mut self, keyword: &[u8]) -> Result<(), JsonError> {
        if self.input.len() >= self.pos + keyword.len()
            && &self.input[self.pos..self.pos + keyword.len()] == keyword
        {
            self.pos += keyword.len();
            Ok(())
        } else {
            Err(self.error(format!(
                "expected '{}'",
                std::str::from_utf8(keyword).unwrap_or("keyword")
            )))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            if items.len() > self.limits.max_items {
                return Err(self.error(format!("array exceeds {} items", self.limits.max_items)));
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']' in array")),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected string key in object"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            if map.insert(key, value).is_some() {
                return Err(self.error("duplicate object key"));
            }
            if map.len() > self.limits.max_items {
                return Err(self.error(format!("object exceeds {} members", self.limits.max_items)));
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.error("expected ',' or '}' in object")),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = match self.bump() {
                Some(byte) => byte,
                None => return Err(self.error("unterminated string")),
            };
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self
                        .bump()
                        .ok_or_else(|| self.error("unterminated escape sequence"))?;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let ch = self.parse_unicode_escape()?;
                            out.push(ch);
                        }
                        other => {
                            return Err(self.error(format!("invalid escape '\\{}'", other as char)))
                        }
                    }
                }
                0x00..=0x1f => {
                    return Err(self.error("raw control character in string"));
                }
                byte if byte < 0x80 => out.push(byte as char),
                byte => {
                    // Multi-byte UTF-8: determine sequence length from the lead byte.
                    let extra = if byte >= 0xf0 {
                        3
                    } else if byte >= 0xe0 {
                        2
                    } else if byte >= 0xc0 {
                        1
                    } else {
                        return Err(self.error("invalid UTF-8 lead byte"));
                    };
                    let start = self.pos - 1;
                    if self.pos + extra > self.input.len() {
                        return Err(self.error("truncated UTF-8 sequence"));
                    }
                    self.pos += extra;
                    let slice = &self.input[start..self.pos];
                    match std::str::from_utf8(slice) {
                        Ok(text) => out.push_str(text),
                        Err(_) => return Err(self.error("invalid UTF-8 sequence")),
                    }
                }
            }
            if out.len() > self.limits.max_string_len {
                return Err(self.error(format!(
                    "string exceeds {} bytes",
                    self.limits.max_string_len
                )));
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.parse_hex4()?;
        if (0xd800..0xdc00).contains(&first) {
            // High surrogate: a low surrogate must follow.
            if self.peek() == Some(b'\\') {
                self.pos += 1;
                if self.peek() == Some(b'u') {
                    self.pos += 1;
                    let second = self.parse_hex4()?;
                    if (0xdc00..0xe000).contains(&second) {
                        let code = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                        return char::from_u32(code)
                            .ok_or_else(|| self.error("invalid surrogate pair"));
                    }
                    return Err(self.error("invalid low surrogate"));
                }
            }
            return Err(self.error("unpaired high surrogate"));
        }
        if (0xdc00..0xe000).contains(&first) {
            return Err(self.error("unpaired low surrogate"));
        }
        char::from_u32(first).ok_or_else(|| self.error("invalid unicode escape"))
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let byte = self
                .bump()
                .ok_or_else(|| self.error("truncated \\u escape"))?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("invalid hex digit in \\u escape")),
            };
            value = value * 16 + digit as u32;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("leading zeros are not allowed"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("expected digit after '-'")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected digit after decimal point"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected digit in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| self.error("invalid number"))?;
        if !is_float {
            if let Ok(int) = text.parse::<i64>() {
                return Ok(Json::Number(Number::Int(int)));
            }
        }
        let float: f64 = text
            .parse()
            .map_err(|_| self.error("number out of range"))?;
        if !float.is_finite() {
            return Err(self.error("number out of range"));
        }
        Ok(Json::Number(Number::Float(float)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::to_string;

    #[test]
    fn scalars() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse(" true ").unwrap(), Json::Bool(true));
        assert_eq!(parse("false").unwrap(), Json::Bool(false));
        assert_eq!(parse("42").unwrap().as_i64(), Some(42));
        assert_eq!(parse("-0.5e2").unwrap().as_f64(), Some(-50.0));
        assert_eq!(parse("\"hi\"").unwrap().as_str(), Some("hi"));
    }

    #[test]
    fn nesting_and_roundtrip() {
        let text = r#"{"a":[1,2,{"b":"c\td"}],"e":null}"#;
        let value = parse(text).unwrap();
        assert_eq!(to_string(&value), r#"{"a":[1,2,{"b":"c\td"}],"e":null}"#);
        assert_eq!(value.get_path("a[2].b").unwrap().as_str(), Some("c\td"));
    }

    #[test]
    fn unicode_escapes() {
        assert_eq!(parse(r#""\u00e9""#).unwrap().as_str(), Some("é"));
        assert_eq!(parse(r#""\ud83e\udd84""#).unwrap().as_str(), Some("🦄"));
        assert!(parse(r#""\ud83e""#).is_err());
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "01",
            "1.",
            "tru",
            "\"unterminated",
            "{} trailing",
            "{\"a\":1,\"a\":2}",
            "\"\\x\"",
        ] {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn depth_limit() {
        let deep = format!("{}{}", "[".repeat(100), "]".repeat(100));
        let err = parse(&deep).unwrap_err();
        assert!(err.message.contains("depth"), "{}", err.message);
    }

    #[test]
    fn byte_limit() {
        let limits = ParseLimits {
            max_bytes: 4,
            ..Default::default()
        };
        assert!(parse_with_limit("[1,2,3,4]", limits).is_err());
    }

    #[test]
    fn error_positions_are_useful() {
        let err = parse("{\n  \"a\": x\n}").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("unexpected character"));
    }

    #[test]
    fn string_length_limit() {
        let limits = ParseLimits {
            max_string_len: 4,
            ..Default::default()
        };
        assert!(parse_with_limit("\"abcdefg\"", limits).is_err());
    }
}
