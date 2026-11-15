use std::fmt::{self, Write as _};

use indexmap::IndexMap;
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub type Map = IndexMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

// Kept as the source literal so `1` never becomes `1.0` on the way back out.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Number(String);

impl Number {
    pub fn from_literal(literal: &str) -> Option<Self> {
        let mut parser = Parser::new(literal);
        let number = parser.number().ok()?;
        (parser.pos == literal.len()).then_some(number)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_f64(&self) -> f64 {
        self.0.parse().unwrap_or(f64::NAN)
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.0.parse().ok()
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.0.parse().ok()
    }
}

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        Self(value.to_string())
    }
}

impl From<u64> for Number {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Value {
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<&Number> {
        match self {
            Value::Number(number) => Some(number),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|map| map.get(key))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    pub fn object() -> Self {
        Value::Object(Map::new())
    }

    pub fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, Some(0));
        out
    }

    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, None);
        out
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Number(value.into())
    }
}

impl From<Map> for Value {
    fn from(value: Map) -> Self {
        Value::Object(value)
    }
}

impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Value::Array(value)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_compact_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at line {line}, column {column}")]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

// Mirrors JSON.parse: a repeated key keeps its first position and takes its last value.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser::new(text);
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != text.len() {
        return Err(parser.error("unexpected trailing characters"));
    }
    Ok(value)
}

pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
    serde_json::from_str(&value.to_compact_string())
}

pub fn to_value<T: Serialize>(value: &T) -> Result<Value, serde_json::Error> {
    let text = serde_json::to_string(value)?;
    parse(&text).map_err(serde::ser::Error::custom)
}

const MAX_DEPTH: usize = 1024;

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn error(&self, message: &str) -> ParseError {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        ParseError {
            message: message.to_owned(),
            line,
            column,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, literal: &str, value: Value) -> Result<Value, ParseError> {
        if self.text[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("unexpected token"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.expect_literal("null", Value::Null),
            Some(b't') => self.expect_literal("true", Value::Bool(true)),
            Some(b'f') => self.expect_literal("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
            Some(_) => Err(self.error("unexpected token")),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Number, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.error("invalid number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("invalid number"));
            }
        }
        Ok(Number(self.text[start..self.pos].to_owned()))
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("invalid unicode escape"))?;
        self.pos += 4;
        u16::from_str_radix(digits, 16).map_err(|_| self.error("invalid unicode escape"))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.text[run_start..self.pos]);
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("unterminated string"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            out.push(self.unicode_escape(unit)?);
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                Some(_) => return Err(self.error("control character in string")),
            }
        }
    }

    // Rust strings cannot hold a lone surrogate; it becomes U+FFFD (JavaScript would keep it).
    fn unicode_escape(&mut self, unit: u16) -> Result<char, ParseError> {
        if (0xD800..0xDC00).contains(&unit) && self.text[self.pos..].starts_with("\\u") {
            let save = self.pos;
            self.pos += 2;
            let low = self.hex4()?;
            if (0xDC00..0xE000).contains(&low) {
                let code = 0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                return Ok(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            self.pos = save;
        }
        Ok(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'))
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut map = Map::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.error("expected `:`"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }
}

fn is_array_index(key: &str) -> bool {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if key.len() > 1 && key.starts_with('0') {
        return false;
    }
    key.parse::<u64>().is_ok_and(|n| n < u64::from(u32::MAX))
}

// JavaScript enumerates array-index keys first, ascending, whatever their insertion order.
pub fn js_key_order(map: &Map) -> Vec<(&String, &Value)> {
    let mut indices: Vec<(u64, (&String, &Value))> = Vec::new();
    let mut rest = Vec::new();
    for entry in map {
        if is_array_index(entry.0) {
            indices.push((entry.0.parse().unwrap_or(0), entry));
        } else {
            rest.push(entry);
        }
    }
    indices.sort_by_key(|(n, _)| *n);
    indices.into_iter().map(|(_, e)| e).chain(rest).collect()
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn newline_indent(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(number.as_str()),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    newline_indent(out, level + 1);
                }
                write_value(out, item, indent.map(|l| l + 1));
            }
            if let Some(level) = indent {
                newline_indent(out, level);
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in js_key_order(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    newline_indent(out, level + 1);
                }
                write_string(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent.map(|l| l + 1));
            }
            if let Some(level) = indent {
                newline_indent(out, level);
            }
            out.push('}');
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(flag) => serializer.serialize_bool(*flag),
            Value::Number(number) => {
                if let Some(n) = number.as_u64() {
                    serializer.serialize_u64(n)
                } else if let Some(n) = number.as_i64() {
                    serializer.serialize_i64(n)
                } else {
                    serializer.serialize_f64(number.as_f64())
                }
            }
            Value::String(text) => serializer.serialize_str(text),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Object(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for (key, item) in map {
                    out.serialize_entry(key, item)?;
                }
                out.end()
            }
        }
    }
}

// Number.prototype.toString, for numbers that reach us as f64 through serde.
fn js_number(value: f64) -> Option<Number> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some(Number("0".to_owned()));
    }
    let sci = format!("{:e}", value.abs());
    let (mantissa, exponent) = sci.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = i32::try_from(digits.len()).ok()?;
    let n = exponent + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat(usize::try_from(n - k).ok()?))
    } else if 0 < n && n <= 21 {
        let split = usize::try_from(n).ok()?;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(usize::try_from(-n).ok()?))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let (head, tail) = digits.split_at(1);
        let tail = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        format!("{head}{tail}e{sign}{}", (n - 1).abs())
    };
    let sign = if value < 0.0 { "-" } else { "" };
    Some(Number(format!("{sign}{body}")))
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        Value::deserialize(d)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(js_number(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_formatting_matches_number_to_string() {
        let cases = [
            (1.0, "1"),
            (0.5, "0.5"),
            (-2.25, "-2.25"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (123.456, "123.456"),
            (1.5e-10, "1.5e-10"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                js_number(input).map(|n| n.0),
                Some(expected.to_owned()),
                "{input}"
            );
        }
    }

    #[test]
    fn array_index_keys_come_first() {
        let value = parse(r#"{"b":1,"10":2,"2":3,"01":4,"a":5}"#).expect("parse");
        assert_eq!(
            value.to_compact_string(),
            r#"{"2":3,"10":2,"b":1,"01":4,"a":5}"#
        );
    }

    #[test]
    fn duplicate_keys_keep_first_position_and_last_value() {
        let value = parse(r#"{"a":1,"b":2,"a":3}"#).expect("parse");
        assert_eq!(value.to_compact_string(), r#"{"a":3,"b":2}"#);
    }

    #[test]
    fn rejects_what_json_parse_rejects() {
        for bad in [
            "",
            "01",
            "1.",
            ".5",
            "+1",
            "[1,]",
            "{\"a\":1,}",
            "'a'",
            "\u{feff}{}",
            "\"\t\"",
            "nul",
            "{} x",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn escapes_like_json_stringify() {
        let value = Value::String("a\"\\\u{1}\u{1f}\n\u{2028}é/".to_owned());
        assert_eq!(
            value.to_compact_string(),
            "\"a\\\"\\\\\\u0001\\u001f\\n\u{2028}é/\""
        );
    }

    #[test]
    fn surrogate_pairs_decode() {
        let value = parse(r#""😀""#).expect("parse");
        assert_eq!(value.as_str(), Some("😀"));
    }
}
