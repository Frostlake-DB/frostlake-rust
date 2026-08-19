//! A minimal JSON parser and string escaper — just enough for the Frostlake
//! HTTP protocol, so the crate stays dependency-free.

/// A SQL cell or JSON value. Integral numbers parse into `i128`, which holds
/// every `NUMBER(38,0)` exactly; `Date`/`Timestamp`/`Bytes` are produced by the
/// driver's typed conversion (and render as typed literals when used as binds).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date(String),
    Timestamp(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Object member lookup.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Date(s) | Value::Timestamp(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Escapes a string for embedding in a JSON document (no surrounding quotes).
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Where a document stopped parsing, and why.
#[derive(Debug)]
pub struct SyntaxError {
    pub message: String,
    /// The byte offset the parser stopped at.
    pub at: usize,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.at)
    }
}

pub fn parse(text: &str) -> Result<Value, SyntaxError> {
    let mut parser = Parser { bytes: text.as_bytes(), pos: 0 };
    document(&mut parser).map_err(|message| SyntaxError { message, at: parser.pos })
}

fn document(parser: &mut Parser<'_>) -> Result<Value, String> {
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err("trailing content".to_string());
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_whitespace(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}'", b as char))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't') => self.parse_keyword("true", Value::Bool(true)),
            Some(b'f') => self.parse_keyword("false", Value::Bool(false)),
            Some(b'n') => self.parse_keyword("null", Value::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(other) => Err(format!("unexpected '{}'", other as char)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_keyword(&mut self, keyword: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(keyword.as_bytes()) {
            self.pos += keyword.len();
            Ok(value)
        } else {
            Err("invalid token".to_string())
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.parse_value()?;
            members.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err("expected ',' or ']'".to_string()),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| "invalid UTF-8 in string".to_string())?,
            );
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                _ => return Err("unterminated string".to_string()),
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        let b = self.peek().ok_or("unterminated escape")?;
        self.pos += 1;
        match b {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let high = self.parse_hex4()?;
                let code = if (0xD800..0xDC00).contains(&high) {
                    // surrogate pair — the second escape must be a low surrogate
                    if self.bytes[self.pos..].starts_with(b"\\u") {
                        self.pos += 2;
                        let low = self.parse_hex4()?;
                        if !(0xDC00..0xE000).contains(&low) {
                            return Err("unpaired surrogate".to_string());
                        }
                        0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                    } else {
                        return Err("unpaired surrogate".to_string());
                    }
                } else {
                    high
                };
                out.push(char::from_u32(code).ok_or("invalid code point")?);
            }
            other => return Err(format!("invalid escape '\\{}'", other as char)),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated \\u escape".to_string());
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "invalid \\u escape".to_string())?;
        self.pos += 4;
        u32::from_str_radix(hex, 16).map_err(|_| "invalid \\u escape".to_string())
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        let mut fractional = false;
        while let Some(&b) = self.bytes.get(self.pos) {
            match b {
                b'0'..=b'9' | b'-' | b'+' => self.pos += 1,
                b'.' | b'e' | b'E' => {
                    fractional = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if !fractional {
            if let Ok(i) = text.parse::<i128>() {
                return Ok(Value::Int(i));
            }
        }
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("invalid number '{text}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, parse, Value};

    #[test]
    fn parses_numbers_in_every_shape() {
        assert_eq!(parse("0").unwrap(), Value::Int(0));
        assert_eq!(parse("-42").unwrap(), Value::Int(-42));
        assert_eq!(parse("9.5").unwrap(), Value::Float(9.5));
        assert_eq!(parse("-2.5e-2").unwrap(), Value::Float(-0.025));
        assert_eq!(parse("1E3").unwrap(), Value::Float(1000.0));
        // A decimal that happens to be integral still stays Float — the
        // wire's "9.0" is a scaled NUMBER or FLOAT, not an Int.
        assert_eq!(parse("9.0").unwrap(), Value::Float(9.0));
        assert!(parse("1-2").is_err());
    }

    #[test]
    fn parses_keywords_and_empty_containers() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("[]").unwrap(), Value::Array(Vec::new()));
        assert_eq!(parse("{}").unwrap(), Value::Object(Vec::new()));
        assert_eq!(
            parse(" [ null , {\"a\" : 1} ] ").unwrap(),
            Value::Array(vec![Value::Null, Value::Object(vec![("a".to_string(), Value::Int(1))])])
        );
    }

    #[test]
    fn rejects_malformed_documents() {
        assert!(parse("").is_err());
        assert!(parse("{\"a\":1} extra").is_err());
        assert!(parse("\"unterminated").is_err());
        assert!(parse("{\"a\"}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("nulL").is_err());
        assert!(parse("\"bad \\x escape\"").is_err());
    }

    #[test]
    fn syntax_errors_name_the_byte_they_stopped_at() {
        // The shape a raw `undefined` array element in a response produces.
        let error = parse("[1, undefined]").unwrap_err();
        assert_eq!(error.at, 4);
        assert_eq!(error.to_string(), "unexpected 'u' at byte 4");
        assert_eq!(parse("[1").unwrap_err().to_string(), "expected ',' or ']' at byte 2");
        assert_eq!(parse("{} x").unwrap_err().to_string(), "trailing content at byte 3");
    }

    #[test]
    fn escape_survives_a_parse_round_trip() {
        let original = "a\"b\\c\nd\re\tf\u{0001}g é😀";
        let parsed = parse(&format!("\"{}\"", escape(original))).unwrap();
        assert_eq!(parsed, Value::Str(original.to_string()));
    }
}
